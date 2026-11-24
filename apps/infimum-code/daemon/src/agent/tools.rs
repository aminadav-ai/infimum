//! Capability-checked workspace tools, outline/symbol extraction, batch editing,
//! and native tool execution.

use crate::policy::CapabilityPolicy;
use crate::studio_abi::*;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::conversations::{active_conv_cell, conversations_dir, ActiveConversation};
use super::timeline::{
    signal_process_group, timeline_update_step_tail, ASYNC_CANCELLED, ASYNC_CMD_KILL_REQUESTED,
    ASYNC_CMD_PID, SIGKILL_NUM, SIGTERM_NUM,
};

// --- Native Agent Tools (Capability-Checked) ---

pub(super) fn is_safe_path(candidate: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let check_target = if candidate.exists() {
        fs::canonicalize(candidate).ok()
    } else {
        // Walk up to the nearest existing ancestor so creating files inside brand-new
        // nested subdirectories (e.g. `src/main.ifm` in an empty project) succeeds while
        // still rejecting `..` traversal and symlink escapes.
        let mut cur = candidate;
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut resolved: Option<PathBuf> = None;
        loop {
            if cur.exists() {
                if let Ok(mut base) = fs::canonicalize(cur) {
                    for seg in tail.iter().rev() {
                        base.push(seg);
                    }
                    resolved = Some(base);
                }
                break;
            }
            match cur.file_name() {
                Some(name) => {
                    if name == ".." {
                        break;
                    }
                    tail.push(name.to_os_string());
                }
                None => break,
            }
            match cur.parent() {
                Some(parent) if parent != cur => cur = parent,
                _ => break,
            }
        }
        // Reject if any component in the original candidate was `..` outside canonicalization
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            None
        } else {
            resolved
        }
    };
    let Some(canon) = check_target else {
        return false;
    };
    roots.iter().any(|r| {
        fs::canonicalize(r)
            .map(|cr| canon.starts_with(cr))
            .unwrap_or(false)
    })
}

/// Blocks reading, writing, or listing secret/credential files even if they exist inside a project folder.
pub fn is_sensitive_path(path: &Path) -> bool {
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy().to_ascii_lowercase();
        if matches!(s.as_str(), ".ssh" | ".gnupg" | ".aws" | ".gemini") {
            return true;
        }
    }
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if fname == ".env"
        || fname.starts_with(".env.")
        || fname.ends_with(".pem")
        || fname.ends_with(".key")
        || fname.ends_with(".p12")
        || fname.ends_with(".pfx")
        || matches!(
            fname.as_str(),
            "id_rsa"
                | "id_ed25519"
                | "id_ecdsa"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | "credentials.json"
        )
    {
        return true;
    }
    false
}

pub(super) fn workspaces_config_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/infimum-code/workspaces.json")
}

/// Prevents accidentally adding system roots, `$HOME` itself, or hidden `$HOME/.*` secret directories as a workspace.
pub(super) fn is_forbidden_workspace_root(canon: &Path) -> bool {
    if is_sensitive_path(canon) {
        return true;
    }
    let home = env::var("HOME")
        .ok()
        .and_then(|h| fs::canonicalize(h).ok());
    if let Some(ref h) = home {
        if canon == h {
            return true;
        }
        // Block any hidden dot-directory directly inside $HOME (~/.config, ~/.local, ~/.mozilla, ~/.aws, etc.)
        if let Ok(rel) = canon.strip_prefix(h) {
            if let Some(first) = rel.components().next() {
                let first_s = first.as_os_str().to_string_lossy();
                if first_s.starts_with('.') {
                    return true;
                }
            }
        }
    }
    let s = canon.to_string_lossy();
    matches!(
        s.as_ref(),
        "/" | "/home" | "/root" | "/etc" | "/usr" | "/bin" | "/sbin" | "/lib" | "/lib64" | "/var" | "/boot" | "/opt" | "/srv" | "/proc" | "/sys" | "/dev"
    )
}

/// Load user-approved workspace folders (strict manual whitelist).
/// Returns only existing safe directories explicitly added by the user.
pub fn load_allowed_workspaces(policy: &CapabilityPolicy) -> Vec<PathBuf> {
    if !policy.fs_enabled() {
        return Vec::new();
    }
    let cfg = workspaces_config_path();
    let Ok(raw) = fs::read_to_string(&cfg) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(arr) = v.get("workspaces").and_then(|w| w.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                let p = PathBuf::from(s);
                if p.is_dir() {
                    if let Ok(canon) = fs::canonicalize(&p) {
                        if !is_forbidden_workspace_root(&canon) && !out.contains(&canon) {
                            out.push(canon);
                        }
                    }
                }
            }
        }
    }
    out
}

pub fn save_allowed_workspaces(list: &[PathBuf]) -> Result<(), String> {
    let cfg = workspaces_config_path();
    if let Some(parent) = cfg.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let strings: Vec<String> = list.iter().map(|p| p.display().to_string()).collect();
    let payload = json!({ "workspaces": strings }).to_string();
    fs::write(&cfg, payload).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let _ = fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600));
    Ok(())
}

pub fn add_allowed_workspace(policy: &CapabilityPolicy, dir: &Path) -> Result<bool, String> {
    if !dir.is_dir() {
        return Err("Выбранный путь не является папкой".to_string());
    }
    let canon = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    if is_forbidden_workspace_root(&canon) {
        return Err(format!(
            "Нельзя добавить системную или домашнюю корневую папку ({})",
            canon.display()
        ));
    }
    let mut list = load_allowed_workspaces(policy);
    if !list.contains(&canon) {
        list.push(canon);
        save_allowed_workspaces(&list)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn delete_conversations_for_project(project_name: &str) {
    let target = project_name.trim();
    if target.is_empty() {
        return;
    }
    if let Some(dir) = conversations_dir() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Ok(raw) = fs::read_to_string(&p) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let proj = v
                    .get("project")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                if proj == target {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
    if let Ok(mut g) = active_conv_cell().lock() {
        if g.project.trim() == target {
            *g = ActiveConversation::default();
        }
    }
}

pub fn remove_allowed_workspace(policy: &CapabilityPolicy, idx: usize) -> Vec<PathBuf> {
    let mut list = load_allowed_workspaces(policy);
    if idx < list.len() {
        let removed = list.remove(idx);
        let _ = save_allowed_workspaces(&list);
        if let Some(name) = removed.file_name().and_then(|s| s.to_str()) {
            let still_exists = list
                .iter()
                .any(|p| p.file_name().and_then(|s| s.to_str()) == Some(name));
            if !still_exists {
                delete_conversations_for_project(name);
            }
        }
    }
    list
}

pub fn resolve_whitelisted_project(policy: &CapabilityPolicy, name: &str) -> Option<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let list = load_allowed_workspaces(policy);
    for ws in list {
        let base = ws
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if base == trimmed || ws.to_string_lossy() == trimmed {
            return Some(ws);
        }
    }
    None
}

pub(super) fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() >= 220 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.file_name());
    for ent in ents {
        if out.len() >= 220 {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "target" | "node_modules" | ".infimum" | "logs" | "dist"
        ) {
            continue;
        }
        let p = ent.path();
        if is_sensitive_path(&p) {
            continue;
        }
        if p.is_dir() {
            collect_files(root, &p, out, depth + 1);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
}

pub(super) const OUTLINE_MAX_ENTRIES: usize = 400;
pub(super) const OUTLINE_MAX_LINE_CHARS: usize = 300;
pub(super) const OUTLINE_CORE_AXW: &[u8] = include_bytes!("../../../cores/outline_core.ifw");

pub(super) fn outline_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("outline_core.ifw", OUTLINE_CORE_AXW))
}

pub(super) fn extract_outline_via_infimum(src: &str, is_rust: bool) -> (Vec<String>, bool) {
    let (engine, module) = outline_core_module();
    let mut store = wasmi::Store::new(engine, ());
    if store.set_fuel(400_000_000).is_err() {
        return (Vec::new(), false);
    }
    let linker = wasmi::Linker::new(engine);
    let Ok(inst) = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
    else {
        return (Vec::new(), false);
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return (Vec::new(), false);
    };
    let Ok(func) = inst.get_typed_func::<(), i32>(&store, "extract_outline_ifm") else {
        return (Vec::new(), false);
    };

    let chars: Vec<u32> = src.chars().take(262_144).map(|c| c as u32).collect();
    {
        let mem = memory.data_mut(&mut store);
        let put = |m: &mut [u8], word: usize, v: i32| {
            let off = word * 4;
            if off + 4 <= m.len() {
                m[off..off + 4].copy_from_slice(&v.to_le_bytes());
            }
        };
        put(mem, 0, chars.len() as i32);
        put(mem, 1, i32::from(is_rust));
        for (i, &cp) in chars.iter().enumerate() {
            put(mem, 25_000 + i, cp as i32);
        }
    }

    let Ok(out_len_raw) = func.call(&mut store, ()) else {
        return (Vec::new(), false);
    };
    let mem = memory.data(&store);
    let get = |word: usize| -> i32 {
        let off = word * 4;
        if off + 4 <= mem.len() {
            i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
        } else {
            0
        }
    };
    let truncated = get(4) != 0;
    let out_len = (out_len_raw.max(0) as usize).min(131_072);
    let mut text = String::with_capacity(out_len);
    for i in 0..out_len {
        let cp = get(287_200 + 1 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            text.push(ch);
        }
    }
    let entries = if text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|l| l.to_string()).collect()
    };
    (entries, truncated)
}

pub(super) fn extract_ifm_outline(src: &str) -> (Vec<String>, bool) {
    extract_outline_via_infimum(src, false)
}

pub(super) fn extract_rs_outline(src: &str) -> (Vec<String>, bool) {
    extract_outline_via_infimum(src, true)
}

#[derive(Debug, Clone)]
pub(super) struct IfmSymbolSpan {
    kind: &'static str,
    name: String,
    start_line: usize, // 1-based inclusive
    end_line: usize,   // 1-based inclusive
}

pub(super) fn scan_brace_delta(line: &str) -> (i32, bool) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut delta = 0i32;
    let mut saw_open = false;
    let mut in_str = false;
    let mut in_chr = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_chr {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_chr = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        match b {
            b'"' => in_str = true,
            b'\'' => in_chr = true,
            b'{' => {
                delta += 1;
                saw_open = true;
            }
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    (delta, saw_open)
}

pub(super) fn parse_ifm_decl_header(trimmed: &str) -> Option<(&'static str, String)> {
    let prefixes: &[(&str, &str)] = &[
        ("export fn ", "fn"),
        ("fn ", "fn"),
        ("export forma ", "forma"),
        ("forma ", "forma"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("layout ", "layout"),
        ("const ", "const"),
    ];
    for &(pfx, kind) in prefixes {
        if let Some(rest) = trimmed.strip_prefix(pfx) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some((kind, name));
            }
        }
    }
    None
}

pub(super) fn parse_ifm_symbol_spans(src: &str) -> Vec<IfmSymbolSpan> {
    let lines: Vec<&str> = src.lines().collect();
    let mut spans = Vec::new();
    let mut depth = 0i32;
    let mut cur: Option<(&'static str, String, usize, bool)> = None; // (kind, name, start, opened)

    for (idx, line) in lines.iter().enumerate() {
        let ln = idx + 1;
        let trimmed = line.trim_start();
        if depth == 0 && cur.is_none() {
            if let Some((kind, name)) = parse_ifm_decl_header(trimmed) {
                if kind == "const" {
                    let end = if trimmed.contains(';') {
                        ln
                    } else {
                        let mut e = ln;
                        for (j, l2) in lines.iter().enumerate().skip(idx + 1) {
                            e = j + 1;
                            if l2.contains(';') {
                                break;
                            }
                        }
                        e
                    };
                    spans.push(IfmSymbolSpan {
                        kind,
                        name,
                        start_line: ln,
                        end_line: end,
                    });
                    continue;
                }
                cur = Some((kind, name, ln, false));
            }
        }
        let (delta, saw_open) = scan_brace_delta(line);
        depth = (depth + delta).max(0);
        if let Some((kind, name, start_line, mut opened)) = cur.take() {
            if saw_open || depth > 0 {
                opened = true;
            }
            if opened && depth == 0 {
                spans.push(IfmSymbolSpan {
                    kind,
                    name,
                    start_line,
                    end_line: ln,
                });
            } else {
                cur = Some((kind, name, start_line, opened));
            }
        }
    }
    if let Some((kind, name, start_line, _)) = cur {
        spans.push(IfmSymbolSpan {
            kind,
            name,
            start_line,
            end_line: lines.len().max(start_line),
        });
    }
    spans
}

pub(super) fn line_contains_ident(line: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let code = line.split("//").next().unwrap_or("");
    let bytes = code.as_bytes();
    let ib = ident.as_bytes();
    if bytes.len() < ib.len() {
        return false;
    }
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + ib.len() <= bytes.len() {
        if &bytes[i..i + ib.len()] == ib {
            let left_ok = i == 0 || !is_id(bytes[i - 1]);
            let right_ok = i + ib.len() == bytes.len() || !is_id(bytes[i + ib.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

pub(super) fn infimum_template_source(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "ui" => Some((
            "Infimum.Forma 2.1 Interactive Native UI App (Toggle + Slider + TextInput + layout + if/for + ui_fmt_centered + test)",
            r#"use "std/prelude.ifm";

pub(super) enum Action {
  Inc = 1,
  Reset = 2,
  ToggleTurbo = 3,
  TabBase = 10,
}

layout AppState {
  tick: word,
  counter: word inv 0..=999999,
  active_tab: word inv 0..=2,
  turbo: word inv 0..=1,
  intensity: word inv 0..=100,
  name_buf: words[40],
}

forma ActionBtn(label: str, act_id: i64, bg_col: i64, hov_col: i64) -[Mem]-> i64 {
  Button(w: 154, h: 42, bg: bg_col, hover_bg: hov_col, radius: 8, on_click: act_id, align: center) {
    Text(label, w: 120, h: 24, size: 15, style: 1, color: pack_rgba(255, 255, 255, 255), align: center)
  }
}

forma MainScreen(vw: i64, vh: i64, tab: i64, turbo: i64)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160 && tab >= 0 && tab <= 2 && turbo >= 0 && turbo <= 1
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: pack_rgba(17, 19, 24, 255), align: center) {
    Col(w: 480, h: 416, pad: 24, gap: 12, bg: pack_rgba(28, 32, 42, 255), border: 1, border_col: pack_rgba(91, 155, 255, 255), radius: 14, align: center) {
      Text("Infimum Native App", w: 380, h: 36, size: 26, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
      Row(w: 324, h: 32, gap: 8, align: center) {
        for i in 0..3 {
          Button(
            w: 102, h: 32,
            bg: if tab == i { pack_rgba(59, 130, 246, 255) } else { pack_rgba(38, 43, 56, 255) },
            hover_bg: pack_rgba(74, 144, 255, 255),
            radius: 7,
            on_click: Action::TabBase + i,
            align: center
          ) {
            Text(if i == 0 { "Overview" } else if i == 1 { "Controls" } else { "About" }, w: 88, h: 20, size: 13, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
          }
        }
      }
      Box(w: 420, h: 26, align: center) {
        if tab == 0 {
          Text("Zero magic addresses, 100% Z3-verified Forma 2.1 UI", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(160, 174, 198, 255), align: center)
        } else if tab == 1 {
          Text("Interactive Toggle, Slider & TextInput widgets", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(110, 231, 183, 255), align: center)
        } else {
          Text("Built with Infimum Lang & Strata Wasm", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(253, 224, 71, 255), align: center)
        }
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Turbo Mode:", w: 120, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Toggle(id: 20, w: 44, h: 24, checked: turbo == 1, on_change: Action::ToggleTurbo, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Intensity:", w: 90, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Slider(id: 21, w: 240, h: 24, bind: AppState::intensity, min: 0, max: 100, step: 5, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      TextInput(id: 22, w: 420, h: 36, buf: AppState::name_buf, max_len: 32, placeholder: "Type a label...", bg: pack_rgba(18, 21, 29, 255), border_col: pack_rgba(64, 74, 96, 255), accent: pack_rgba(91, 155, 255, 255), color: pack_rgba(248, 250, 255, 255), size: 14)
      Row(w: 320, h: 42, gap: 12, align: center) {
        ActionBtn("Action", Action::Inc, pack_rgba(59, 130, 246, 255), pack_rgba(96, 165, 250, 255))
        ActionBtn("Reset", Action::Reset, pack_rgba(44, 50, 64, 255), pack_rgba(64, 72, 92, 255))
      }
    }
  }
}

export fn init_app() -[Mem]-> i64 {
  AppState.tick = 0;
  AppState.counter = 0;
  AppState.active_tab = 0;
  AppState.turbo = 1;
  AppState.intensity = 60;
  buf_clear(AppState::name_buf);
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  AppState.tick += 1;

  forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);
  forma_set_input(m_down, key_char, key_special, AppState.tick);
  let act = MainScreen(w, h, AppState.active_tab, AppState.turbo);
  if act == Action::Inc {
    let step = if AppState.turbo == 1 { 5 } else { 1 };
    AppState.counter = clamp_i64(AppState.counter + step, 0, 999999);
    sfx_click();
  } else if act == Action::Reset {
    AppState.counter = 0;
    AppState.intensity = 60;
    buf_clear(AppState::name_buf);
    sfx_clack();
  } else if act == Action::ToggleTurbo {
    AppState.turbo = 1 - AppState.turbo;
    sfx_click();
  } else if act >= Action::TabBase && act < Action::TabBase + 3 {
    AppState.active_tab = act - Action::TabBase;
    sfx_click();
  }
  gfx_set_cursor(forma_cursor());
  ui_fmt_centered(w / 2, h / 2 + 228, "Clicks: {AppState.counter}  |  Intensity: {AppState.intensity}%  |  Tab #{AppState.active_tab + 1}", 15, 1, pack_rgba(200, 214, 238, 255), 0);
  gfx_end_frame()
}

test "init resets app state" {
  let _ = init_app();
  assert(AppState.counter == 0);
  assert(AppState.active_tab == 0);
  assert(AppState.turbo == 1);
  assert(AppState.intensity == 60);
}
"#,
        )),
        "game2d" => Some((
            "2D GPU Game Starter (auto-placed layout + struct array + for + ui_fmt + test)",
            r#"use "std/prelude.ifm";

pub(super) struct Orb {
  x: word,
  y: word,
  vx: word,
  vy: word,
  col: word,
}

layout GameState {
  tick: word,
  score: word inv 0..=999999,
  orbs: Orb[12],
}

export fn init_app() -[Mem]-> i64 {
  GameState.tick = 0;
  GameState.score = 0;
  for i in 0..12 {
    GameState.orbs[i].x = 120 + i * 80;
    GameState.orbs[i].y = 200 + (i % 4) * 90;
    GameState.orbs[i].vx = if i % 2 == 0 { 3 } else { -3 };
    GameState.orbs[i].vy = if i % 3 == 0 { 2 } else { -2 };
    GameState.orbs[i].col = pack_rgba(60 + i * 15, 190 - i * 8, 240, 255);
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  GameState.tick += 1;
  gfx_begin_frame(w, h);
  ui_grad_rect(0, 0, w, h, 0, 0, 0, pack_rgba(14, 18, 28, 255), pack_rgba(24, 30, 46, 255), 0);

  for i in 0..12 {
    GameState.orbs[i].x += GameState.orbs[i].vx;
    GameState.orbs[i].y += GameState.orbs[i].vy;
    let ox = GameState.orbs[i].x;
    let oy = GameState.orbs[i].y;
    if ox < 32 || ox > w - 32 {
      GameState.orbs[i].vx = 0 - GameState.orbs[i].vx;
      GameState.score = clamp_i64(GameState.score + 1, 0, 999999);
      sfx_hop();
    }
    if oy < 64 || oy > h - 32 { GameState.orbs[i].vy = 0 - GameState.orbs[i].vy; }
    ui_orb(ox, oy, 22, GameState.orbs[i].col, 0);
  }
  ui_fmt(20, 18, "2D Infimum Game — Bounces: {GameState.score}", 20, 1, pack_rgba(245, 248, 255, 255), 0);
  gfx_end_frame()
}

test "init populates all 12 orbs" {
  let _ = init_app();
  assert(GameState.orbs[0].x == 120);
  assert(GameState.orbs[11].x == 1000);
}
"#,
        )),
        "game3d" => Some((
            "3D Vulkan Mesh Starter (auto-placed layout + value struct Vec3 + Q16.16 fixed + std/mesh3d + PBR + dynamic lights + ui_fmt)",
            r#"use "std/prelude.ifm";
use "std/mesh3d.ifm";

pub(super) struct Vec3 {
  x: fixed,
  y: fixed,
  z: fixed,
}

pub(super) struct Pillar {
  x: word,
  z: word,
  h: word,
}

layout WorldState {
  tick: word,
  pos: Vec3,
  yaw: word,
  pitch: word inv -220..=220,
  pillars: Pillar[8],
}

pub(super) fn vec3_add(a: Vec3, b: Vec3) -> Vec3 {
  Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

export fn init_app() -[Mem]-> i64 {
  WorldState.tick = 0;
  WorldState.pos = Vec3 { x: 0.0, y: 1.7, z: -4.5 };
  WorldState.yaw = 0;
  WorldState.pitch = 0;
  for i in 0..8 {
    let side = if i % 2 == 0 { -3200 } else { 3200 };
    WorldState.pillars[i].x = side;
    WorldState.pillars[i].z = -2000 + (i / 2) * 2600;
    WorldState.pillars[i].h = 4200;
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  WorldState.tick += 1;
  gfx_begin_frame(w, h);

  if m_clicked == 1 && !pointer_locked() { set_pointer_lock(true); }
  if pointer_locked() {
    WorldState.yaw = (WorldState.yaw + mouse_dx() * 2) & 4095;
    WorldState.pitch = clamp_i64(WorldState.pitch - mouse_dy(), -220, 220);
  }
  let yw = WorldState.yaw;
  let fx = fixed_sin(yw);
  let fz = fixed_cos(yw);
  let mut delta = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
  if key_held(KEY_W) { delta.x += fx * 0.09; delta.z += fz * 0.09; }
  if key_held(KEY_S) { delta.x -= fx * 0.09; delta.z -= fz * 0.09; }
  if key_held(KEY_A) { delta.x -= fz * 0.08; delta.z += fx * 0.08; }
  if key_held(KEY_D) { delta.x += fz * 0.08; delta.z -= fx * 0.08; }
  let pos = vec3_add(WorldState.pos, delta);
  WorldState.pos = pos;

  let cam_x = fixed_to_int(pos.x * 1000.0);
  let cam_y = fixed_to_int(pos.y * 1000.0);
  let cam_z = fixed_to_int(pos.z * 1000.0);
  mesh3d_begin(cam_x, cam_y, cam_z, WorldState.yaw, WorldState.pitch, 75, 2, 0);
  mesh3d_add_light(0, 3600, 1500, 12000, 255, 220, 175, 260);
  mesh3d_box(0, -10000, -200, -10000, 10000, 0, 10000, 6, pack_rgba(190, 185, 175, 255), 220, 0, 2000);
  for i in 0..8 {
    let x = WorldState.pillars[i].x;
    let z = WorldState.pillars[i].z;
    let ph = WorldState.pillars[i].h;
    mesh3d_box(0, x - 450, 0, z - 450, x + 450, ph, z + 450, 2, pack_rgba(220, 210, 195, 255), 235, 0, 1400);
  }

  ui_fmt(18, 16, "Pos: ({pos.x:.1}, {pos.z:.1}) m | WASD: Move | Click: Look", 15, 1, pack_rgba(255, 255, 255, 230), 0);
  gfx_end_frame()
}

test "init places 8 pillars and Vec3 camera in meters" {
  let _ = init_app();
  assert(WorldState.pillars[0].x == -3200);
  assert(WorldState.pillars[1].x == 3200);
  let p = WorldState.pos;
  assert(p.y == 1.7);
  assert(vec3_add(p, Vec3 { x: 1.0, y: 0.3, z: 0.5 }).y == 2.0);
}
"#,
        )),
        _ => None,
    }
}

pub(super) const EDITOR_CORE_AXW: &[u8] = include_bytes!("../../../cores/editor_core.ifw");

pub(super) fn editor_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("editor_core.ifw", EDITOR_CORE_AXW))
}

pub(super) fn apply_batch_edits(
    existing: &str,
    edits: &[(String, String)],
) -> Result<String, (usize, String)> {
    if edits.is_empty() {
        return Err((0, "edits array must not be empty".to_string()));
    }
    let src_chars: Vec<u32> = existing.chars().map(|c| c as u32).collect();
    let pool_needed: usize = edits
        .iter()
        .map(|(t, r)| t.chars().count() + r.chars().count())
        .sum();
    if edits.len() > 64 || src_chars.len() > 131_072 || pool_needed > 65_536 {
        return Err((0, "Batch edit exceeds editor_core.ifw buffer limits".to_string()));
    }

    let (engine, module) = editor_core_module();
    let mut store = wasmi::Store::new(engine, ());
    store
        .set_fuel(200_000_000)
        .map_err(|e| (0, e.to_string()))?;
    let linker = wasmi::Linker::new(engine);
    let inst = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
        .map_err(|e| (0, e.to_string()))?;
    let memory = inst
        .get_memory(&store, "memory")
        .ok_or_else(|| (0, "editor_core missing memory".to_string()))?;
    let func = inst
        .get_typed_func::<(), i32>(&store, "apply_batch_edits_ifm")
        .map_err(|e| (0, e.to_string()))?;

    let put_i32 = |mem: &mut [u8], word: usize, val: i32| {
        let off = word * 4;
        if off + 4 <= mem.len() {
            mem[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
    };
    let get_i32 = |mem: &[u8], word: usize| -> i32 {
        let off = word * 4;
        if off + 4 <= mem.len() {
            i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
        } else {
            0
        }
    };

    {
        let mem = memory.data_mut(&mut store);
        put_i32(mem, 4, src_chars.len() as i32);
        put_i32(mem, 5, edits.len() as i32);
        for (i, &cp) in src_chars.iter().enumerate() {
            put_i32(mem, 66_048 + i, cp as i32);
        }
        let mut p_cur = 0usize;
        for (idx, (target, replacement)) in edits.iter().enumerate() {
            let t_off = p_cur;
            let mut t_len = 0usize;
            for ch in target.chars() {
                put_i32(mem, 512 + p_cur, ch as i32);
                p_cur += 1;
                t_len += 1;
            }
            let r_off = p_cur;
            let mut r_len = 0usize;
            for ch in replacement.chars() {
                put_i32(mem, 512 + p_cur, ch as i32);
                p_cur += 1;
                r_len += 1;
            }
            put_i32(mem, 16 + idx, t_off as i32);
            put_i32(mem, 80 + idx, t_len as i32);
            put_i32(mem, 144 + idx, r_off as i32);
            put_i32(mem, 208 + idx, r_len as i32);
        }
    }

    let out_len = func.call(&mut store, ()).map_err(|e| (0, e.to_string()))?;
    let mem = memory.data(&store);
    let err_code = get_i32(mem, 0);
    let err_idx = get_i32(mem, 1).max(0) as usize;
    let err_aux = get_i32(mem, 2).max(0) as usize;

    if out_len < 0 || err_code != 0 {
        let msg = match err_code {
            1 => "edits array must not be empty".to_string(),
            2 => "target must not be empty".to_string(),
            3 => "Exact target string not found in file".to_string(),
            4 => format!(
                "Target matched {err_aux} places; include more surrounding lines to make it unique"
            ),
            5 => format!("Edit #{err_idx} overlaps/conflicts with edit #{err_aux}"),
            _ => "Batch edit output exceeded buffer limit".to_string(),
        };
        return Err((err_idx, msg));
    }

    let n = (out_len as usize).min(131_072);
    let mut out = String::with_capacity(n);
    for i in 0..n {
        let cp = get_i32(mem, 197_120 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            out.push(ch);
        }
    }
    Ok(out)
}

pub(super) fn describe_tool_ru(name: &str, args: &Value) -> String {
    let short_path = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/"))
            .unwrap_or_default()
    };
    match name {
        "list_project_files" => format!("Смотрю файлы {}…", short_path("project_dir")),
        "search_project" => format!(
            "Ищу «{}»…",
            args.get("query").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "get_file_outline" => format!("Сканирую структуру {}…", short_path("path")),
        "read_project_file" => format!("Читаю {}…", short_path("path")),
        "edit_project_file" => format!("Редактирую {}…", short_path("path")),
        "batch_edit_project_file" => format!("Пакетно редактирую {}…", short_path("path")),
        "write_project_file" => format!("Записываю {}…", short_path("path")),
        "verify_infimum_file" => format!("Проверяю Z3/Strata {}…", short_path("path")),
        "compile_infimum_wasm" => format!("Компилирую .ifw {}…", short_path("path")),
        "run_infimum_capsule" => format!("Запускаю капсулу {}…", short_path("path")),
        "read_infimum_guide" => "Читаю справочник Infimum Lang…".to_string(),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю std/{f}…")
        }
        "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Читаю символ {sym}…")
        }
        "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Обновляю символ {sym}…")
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Ищу вызовы {sym}…")
        }
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю шаблон {k}…")
        }
        "read_infimum_example" => {
            let ex = args.get("example").and_then(|v| v.as_str()).unwrap_or("list");
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("");
            if f.is_empty() {
                format!("Читаю пример {ex}…")
            } else {
                format!("Читаю пример {ex}/{f}…")
            }
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Ищу PBR-текстуры «{q}»…")
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Привязываю текстуру {id} → слот {slot}…")
        }
        "run_project_command" => {
            let c: String = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            format!("Выполняю: {c}…")
        }
        _ => format!("Инструмент {name}…"),
    }
}

pub(super) fn short_path_label(raw: &str) -> String {
    let p = raw.trim();
    if p.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        p.to_string()
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

pub(super) fn describe_tool_step(name: &str, args: &Value) -> (i64, String, String) {
    match name {
        "list_project_files" => {
            let d = args.get("project_dir").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Список файлов проекта".to_string(), short_path_label(d))
        }
        "search_project" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск по коду".to_string(), q.chars().take(80).collect())
        }
        "get_file_outline" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Структура файла".to_string(), short_path_label(p))
        }
        "read_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let s = args.get("start_line").and_then(|v| v.as_i64()).unwrap_or(1);
            let e = args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(s + 100);
            (
                STEP_KIND_TOOL,
                "Чтение файла".to_string(),
                format!("{} (L{}..{})", short_path_label(p), s, e),
            )
        }
        "edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Редактирование файла".to_string(), short_path_label(p))
        }
        "batch_edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let n = args.get("edits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            (
                STEP_KIND_TOOL,
                format!("Пакетная правка ({n})"),
                short_path_label(p),
            )
        }
        "write_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Запись файла".to_string(), short_path_label(p))
        }
        "verify_infimum_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Верификация Z3".to_string(), short_path_label(p))
        }
        "compile_infimum_wasm" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Компиляция .ifm → .ifw".to_string(), short_path_label(p))
        }
        "run_infimum_capsule" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(true);
            let title = if headless {
                "Проверка капсулы (headless)"
            } else {
                "Запуск окна infimum-web"
            };
            (STEP_KIND_TOOL, title.to_string(), short_path_label(p))
        }
        "read_infimum_guide" => (
            STEP_KIND_TOOL,
            "Справочник Infimum Lang".to_string(),
            "docs/INFIMUM_LANG_GUIDE.md".to_string(),
        ),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            (
                STEP_KIND_TOOL,
                "Стандартная библиотека".to_string(),
                format!("std/{}", f.trim_start_matches("std/")),
            )
        }
        "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                format!("Символ {sym}"),
                short_path_label(p),
            )
        }
        "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                format!("Замена {sym}"),
                short_path_label(p),
            )
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                "Поиск вызовов".to_string(),
                sym.to_string(),
            )
        }
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            (
                STEP_KIND_TOOL,
                "Шаблон Infimum".to_string(),
                k.to_string(),
            )
        }
        "read_infimum_example" => {
            let ex = args.get("example").and_then(|v| v.as_str()).unwrap_or("list");
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let sub = if f.is_empty() { ex.to_string() } else { format!("{ex}/{f}") };
            (
                STEP_KIND_TOOL,
                "Исходник примера".to_string(),
                sub,
            )
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                "Поиск PBR-текстур".to_string(),
                q.to_string(),
            )
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            (
                STEP_KIND_TOOL,
                "PBR-текстура".to_string(),
                format!("slot {slot} -> {id}"),
            )
        }
        "run_project_command" => {
            let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
            (
                STEP_KIND_CMD,
                "Команда терминала".to_string(),
                format!("$ {}", cmd.chars().take(120).collect::<String>()),
            )
        }
        _ => (STEP_KIND_TOOL, format!("Инструмент {name}"), String::new()),
    }
}

pub(super) const EMBEDDED_INFIMUM_GUIDE: &str = include_str!("../../../docs/INFIMUM_LANG_GUIDE.md");

pub(super) fn studio_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = env::var("INFIMUM_CODE_HOME") {
        if !home.trim().is_empty() {
            roots.push(PathBuf::from(home));
        }
    }
    if let Ok(exe) = env::current_exe() {
        // Monorepo workspace: <monorepo>/target/{release,debug}/infimum-agentd -> <monorepo>/apps/infimum-code
        if let Some(r) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            roots.push(r.join("apps/infimum-code"));
        }
        // Installed layout: <prefix>/bin/infimum-agentd -> <prefix>/
        if let Some(r) = exe.parent().and_then(|p| p.parent()) {
            roots.push(r.to_path_buf());
        }
    }
    roots
}

pub(super) fn read_canonical_infimum_guide() -> String {
    for root in studio_root_candidates() {
        let p = root.join("docs/INFIMUM_LANG_GUIDE.md");
        if let Ok(text) = fs::read_to_string(&p) {
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    EMBEDDED_INFIMUM_GUIDE.to_string()
}

/// Find the most recently modified `.ifw` capsule in `active_project` (depth <= 2) and append a
/// `[PLAY:<rel_path>]` tag to `reply` so Infimum Code renders a clickable Play button at the end
/// of the assistant message (works identically on desktop and in Online Infimum Code).
pub(super) fn append_play_tag_if_axw_present(reply: &mut String, active_project: Option<&str>) {
    if reply.contains("[PLAY:") || reply.trim_start().starts_with("Ошибка") {
        return;
    }
    let Some(proj_str) = active_project.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let workspace = Path::new(proj_str);
    if !workspace.is_dir() {
        return;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let scan_dir = |dir: &Path, prefix: &str, best: &mut Option<(std::time::SystemTime, String)>| {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
            if path.is_file() && name.ends_with(".ifw") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let rel = if prefix.is_empty() {
                            name.to_string()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                            *best = Some((mtime, rel));
                        }
                    }
                }
            }
        }
    };
    scan_dir(workspace, "", &mut best);
    if best.is_none() {
        if let Ok(rd) = fs::read_dir(workspace) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
                if path.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules" {
                    scan_dir(&path, name, &mut best);
                }
            }
        }
    }
    if let Some((_, rel)) = best {
        if !reply.ends_with('\n') {
            reply.push('\n');
        }
        reply.push_str(&format!("[PLAY:{rel}]"));
    }
}

pub(crate) fn resolve_infimum_bin(policy: &CapabilityPolicy) -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    let home = env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Monorepo / side-by-side binary in the same target directory as `infimum-agentd`
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("infimum"));
        }
    }
    if let Some(r) = policy.allowed_fs_roots.first() {
        candidates.push(r.join("target/release/infimum"));
        candidates.push(r.join("target/debug/infimum"));
        candidates.push(r.join("infimum-lang/target/release/infimum"));
        candidates.push(r.join("infimum-lang/target/debug/infimum"));
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            candidates.push(mono.join("target/release/infimum"));
            candidates.push(mono.join("target/debug/infimum"));
        }
        if let Some(ws) = root.parent() {
            candidates.push(ws.join("infimum-lang/target/release/infimum"));
            candidates.push(ws.join("infimum-lang/target/debug/infimum"));
        }
    }
    if !home.is_empty() {
        candidates.push(PathBuf::from(&home).join(".local/bin/infimum"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/infimum"));
    for c in candidates {
        if c.is_file() {
            return c;
        }
    }
    PathBuf::from("infimum")
}

pub(super) fn resolve_infimum_std_dir() -> Option<PathBuf> {
    if let Ok(p) = env::var("INFIMUM_STD_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(p) = env::var("INFIMUM_LANG_DIR") {
        let pb = PathBuf::from(p).join("std");
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(mono) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
        if let Some(ws) = root.parent() {
            let pb = ws.join("infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    None
}

pub(super) fn resolve_z3_bin() -> Option<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    let candidates = [
        PathBuf::from(&home).join(".local/bin/z3"),
        PathBuf::from("/usr/local/bin/z3"),
        PathBuf::from("/usr/bin/z3"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

pub(crate) fn resolve_infimum_web_bin() -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_WEB_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if exe.is_file()
            && exe
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| n == "infimum-web")
                .unwrap_or(false)
        {
            return exe;
        }
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("infimum-web");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            for rel in ["target/release/infimum-web", "target/debug/infimum-web"] {
                let p = mono.join(rel);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    if let Ok(home) = env::var("HOME") {
        let p = PathBuf::from(home).join(".local/bin/infimum-web");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("infimum-web")
}

pub(super) fn execute_native_tool(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    name: &str,
    args: &Value,
    step_idx: Option<usize>,
) -> Value {
    // STRICT SINGLE-FOLDER JAIL: every tool is locked strictly inside the active project folder
    // AND blocked from sensitive secret files (.env, *.key, etc.).
    let jail_roots = [default_proj.to_path_buf()];
    let is_jailed = |p: &Path| -> bool {
        is_safe_path(p, &jail_roots) && !is_sensitive_path(p)
    };
    let resolve = |raw: &str| -> PathBuf {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            default_proj.join(p)
        }
    };

    match name {
        "list_project_files" => {
            let dir = args
                .get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if !is_jailed(&dir) {
                return json!({ "error": "Access denied: path is outside the active project sandbox" });
            }
            let mut files = Vec::new();
            collect_files(&dir, &dir, &mut files, 0);
            json!({ "project_dir": dir.display().to_string(), "files": files })
        }
        "search_project" => {
            let query = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            let dir = args
                .get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if query.is_empty() || !is_jailed(&dir) {
                return json!({ "error": "Invalid search parameters or path outside active project sandbox" });
            }
            match Command::new("grep")
                .args([
                    "-rnI",
                    "--exclude-dir=.git",
                    "--exclude-dir=target",
                    "--exclude-dir=node_modules",
                    "--exclude-dir=.infimum",
                    "--exclude=.env",
                    "--exclude=.env.*",
                    "--exclude=*.pem",
                    "--exclude=*.key",
                    "-m",
                    "40",
                    query,
                    ".",
                ])
                .current_dir(&dir)
                .output()
            {
                Ok(o) => {
                    let s: String = String::from_utf8_lossy(&o.stdout).chars().take(3500).collect();
                    json!({ "ok": true, "matches": s })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "get_file_outline" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied: path is outside active project sandbox or sensitive" });
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "ifm" && ext != "rs" {
                return json!({
                    "ok": false,
                    "error": format!("get_file_outline supports .ifm and .rs files (got '.{ext}')")
                });
            }
            match fs::read_to_string(&p) {
                Ok(txt) => {
                    let total_lines = txt.lines().count();
                    let (mut entries, truncated) = if ext == "ifm" {
                        extract_ifm_outline(&txt)
                    } else {
                        extract_rs_outline(&txt)
                    };
                    let entries_count = entries.len();
                    if truncated {
                        entries.push(format!(
                            "... [truncated at {OUTLINE_MAX_ENTRIES} entries; use search_project or read_project_file for remaining items]"
                        ));
                    }
                    json!({
                        "ok": true,
                        "path": p.display().to_string(),
                        "total_lines": total_lines,
                        "entries_count": entries_count,
                        "truncated": truncated,
                        "outline": entries.join("\n"),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "read_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied: path is outside active project sandbox or sensitive" });
            }
            match fs::read_to_string(&p) {
                Ok(txt) => {
                    let lines: Vec<&str> = txt.lines().collect();
                    let s = args.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1) as usize;
                    let e = args
                        .get("end_line")
                        .and_then(|v| v.as_u64())
                        .unwrap_or(lines.len() as u64) as usize;
                    let s_idx = s.saturating_sub(1).min(lines.len());
                    let e_idx = e.min(lines.len()).max(s_idx);
                    let capped_end = e_idx.min(s_idx + 500);
                    let slice = lines[s_idx..capped_end].join("\n");
                    json!({
                        "path": p.display().to_string(),
                        "start_line": s_idx + 1,
                        "end_line": capped_end,
                        "total_lines": lines.len(),
                        "content": slice
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "edit_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let target = args.get("target").and_then(|s| s.as_str()).unwrap_or("");
            let replacement = args.get("replacement").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Edit denied: path is outside active project sandbox or sensitive" });
            }
            if target.is_empty() {
                return json!({ "error": "target must not be empty" });
            }
            match fs::read_to_string(&p) {
                Ok(existing) => {
                    let count = existing.matches(target).count();
                    if count == 0 {
                        json!({ "ok": false, "error": "Exact target string not found in file" })
                    } else if count > 1 {
                        json!({
                            "ok": false,
                            "error": format!("Target matched {count} places; include more surrounding lines to make it unique")
                        })
                    } else {
                        let updated = existing.replacen(target, replacement, 1);
                        match fs::write(&p, &updated) {
                            Ok(_) => json!({ "ok": true, "replacements": 1, "path": p.display().to_string() }),
                            Err(e) => json!({ "error": e.to_string() }),
                        }
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "batch_edit_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Edit denied: path is outside active project sandbox or sensitive" });
            }
            let Some(edits_arr) = args.get("edits").and_then(|v| v.as_array()) else {
                return json!({
                    "ok": false,
                    "failed_index": 0,
                    "reason": "Missing or invalid 'edits' array",
                    "error": "Missing or invalid 'edits' array"
                });
            };
            let mut parsed_edits: Vec<(String, String)> = Vec::with_capacity(edits_arr.len());
            for (idx, item) in edits_arr.iter().enumerate() {
                let Some(target) = item.get("target").and_then(|v| v.as_str()) else {
                    return json!({
                        "ok": false,
                        "failed_index": idx,
                        "reason": "Missing string field 'target'",
                        "error": format!("Batch edit aborted at index {idx}: Missing string field 'target'")
                    });
                };
                let Some(replacement) = item.get("replacement").and_then(|v| v.as_str()) else {
                    return json!({
                        "ok": false,
                        "failed_index": idx,
                        "reason": "Missing string field 'replacement'",
                        "error": format!("Batch edit aborted at index {idx}: Missing string field 'replacement'")
                    });
                };
                parsed_edits.push((target.to_string(), replacement.to_string()));
            }
            match fs::read_to_string(&p) {
                Ok(existing) => match apply_batch_edits(&existing, &parsed_edits) {
                    Ok(updated) => match fs::write(&p, &updated) {
                        Ok(_) => json!({
                            "ok": true,
                            "path": p.display().to_string(),
                            "applied_count": parsed_edits.len(),
                            "total_lines": updated.lines().count(),
                        }),
                        Err(e) => json!({ "error": e.to_string() }),
                    },
                    Err((failed_index, reason)) => json!({
                        "ok": false,
                        "failed_index": failed_index,
                        "reason": reason,
                        "error": format!("Batch edit aborted at index {failed_index}: {reason}")
                    }),
                },
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "write_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Write denied: path is outside active project sandbox or sensitive" });
            }
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&p, content) {
                Ok(_) => json!({ "ok": true, "written_bytes": content.len(), "path": p.display().to_string() }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "read_infimum_guide" => {
            let guide = read_canonical_infimum_guide();
            json!({
                "path": "docs/INFIMUM_LANG_GUIDE.md",
                "content": guide,
            })
        }
        "read_infimum_std" => {
            const ALLOWED_STD: &[&str] = &[
                "audio.ifm",
                "buf.ifm",
                "color.ifm",
                "fmt.ifm",
                "gfx.ifm",
                "icons.ifm",
                "input.ifm",
                "json.ifm",
                "markdown.ifm",
                "math.ifm",
                "mem.ifm",
                "mesh3d.ifm",
                "prelude.ifm",
                "sys_mem.ifm",
                "text.ifm",
                "text_edit.ifm",
                "theme.ifm",
                "ui.ifm",
            ];
            let raw = args
                .get("file")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            if raw.is_empty() || raw == "list" {
                return json!({
                    "available_files": ALLOWED_STD,
                    "hint": "Call read_infimum_std with file='gfx.ifm' or file='input.ifm' to read any embedded std module."
                });
            }
            let norm = raw.trim_start_matches("std/").trim_start_matches('/');
            let file_name = if norm.ends_with(".ifm") {
                norm.to_string()
            } else {
                format!("{norm}.ifm")
            };
            if !ALLOWED_STD.contains(&file_name.as_str()) {
                return json!({
                    "error": format!("Unknown std module '{raw}'. Allowed modules: {:?}", ALLOWED_STD)
                });
            }
            let Some(std_dir) = resolve_infimum_std_dir() else {
                return json!({ "error": format!("Standard library directory not found (set INFIMUM_LANG_DIR or INFIMUM_STD_DIR)") });
            };
            let std_path = std_dir.join(&file_name);
            match fs::read_to_string(&std_path) {
                Ok(content) => json!({
                    "path": format!("std/{file_name}"),
                    "content": content,
                }),
                Err(e) => json!({ "error": format!("Failed to read std/{file_name}: {e}") }),
            }
        }
        "get_ifm_symbol" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            if sym.is_empty() {
                return json!({ "error": "Missing required 'name' parameter" });
            }
            match fs::read_to_string(&p) {
                Ok(src) => {
                    let spans = parse_ifm_symbol_spans(&src);
                    if let Some(hit) = spans.iter().find(|s| s.name == sym) {
                        let lines: Vec<&str> = src.lines().collect();
                        let s_idx = hit.start_line.saturating_sub(1);
                        let e_idx = hit.end_line.min(lines.len());
                        let snippet = lines[s_idx..e_idx].join("\n");
                        json!({
                            "ok": true,
                            "path": p.display().to_string(),
                            "kind": hit.kind,
                            "name": hit.name,
                            "start_line": hit.start_line,
                            "end_line": hit.end_line,
                            "source": snippet,
                        })
                    } else {
                        let avail: Vec<String> = spans
                            .iter()
                            .take(60)
                            .map(|s| format!("{} {} (L{}-L{})", s.kind, s.name, s.start_line, s.end_line))
                            .collect();
                        json!({
                            "error": format!("Symbol '{sym}' not found in {}", p.display()),
                            "available_symbols": avail,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "replace_ifm_symbol" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
            let new_src = args.get("new_source").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Write denied: path is outside active project sandbox" });
            }
            if sym.is_empty() || new_src.trim().is_empty() {
                return json!({ "error": "Both 'name' and non-empty 'new_source' are required" });
            }
            match fs::read_to_string(&p) {
                Ok(src) => {
                    let spans = parse_ifm_symbol_spans(&src);
                    if let Some(hit) = spans.iter().find(|s| s.name == sym) {
                        let lines: Vec<&str> = src.lines().collect();
                        let s_idx = hit.start_line.saturating_sub(1);
                        let e_idx = hit.end_line.min(lines.len());
                        let mut out_lines: Vec<String> = Vec::with_capacity(lines.len() + 16);
                        for l in &lines[..s_idx] {
                            out_lines.push((*l).to_string());
                        }
                        let new_lines: Vec<&str> = new_src.trim_end_matches('\n').lines().collect();
                        let new_cnt = new_lines.len();
                        for l in new_lines {
                            out_lines.push(l.to_string());
                        }
                        for l in &lines[e_idx..] {
                            out_lines.push((*l).to_string());
                        }
                        let mut updated = out_lines.join("\n");
                        if src.ends_with('\n') {
                            updated.push('\n');
                        }
                        match fs::write(&p, &updated) {
                            Ok(_) => json!({
                                "ok": true,
                                "path": p.display().to_string(),
                                "name": sym,
                                "replaced_lines": [hit.start_line, hit.end_line],
                                "new_line_count": new_cnt,
                            }),
                            Err(e) => json!({ "error": e.to_string() }),
                        }
                    } else {
                        let avail: Vec<String> = spans.iter().take(60).map(|s| s.name.clone()).collect();
                        json!({
                            "error": format!("Symbol '{sym}' not found in {}", p.display()),
                            "available_symbols": avail,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|s| s.as_str()).unwrap_or("").trim();
            let opt_path = args.get("path").and_then(|s| s.as_str()).unwrap_or("").trim();
            if sym.is_empty() {
                return json!({ "error": "Missing required 'symbol' parameter" });
            }
            let mut ifm_files: Vec<PathBuf> = Vec::new();
            if !opt_path.is_empty() {
                let p = resolve(opt_path);
                if !is_jailed(&p) {
                    return json!({ "error": "Access denied outside active project sandbox" });
                }
                if p.is_file() {
                    ifm_files.push(p);
                }
            } else {
                let mut stack = vec![default_proj.to_path_buf()];
                while let Some(dir) = stack.pop() {
                    if ifm_files.len() >= 64 {
                        break;
                    }
                    let Ok(rd) = fs::read_dir(&dir) else {
                        continue;
                    };
                    for ent in rd.flatten() {
                        let ep = ent.path();
                        let fname = ent.file_name().to_string_lossy().to_string();
                        if fname.starts_with('.') || fname == "target" || fname == "node_modules" {
                            continue;
                        }
                        if ep.is_dir() {
                            stack.push(ep);
                        } else if ep.extension().and_then(|e| e.to_str()) == Some("ifm") {
                            ifm_files.push(ep);
                        }
                    }
                }
                ifm_files.sort();
            }
            let mut hits: Vec<Value> = Vec::new();
            const MAX_HITS: usize = 60;
            for fp in &ifm_files {
                if hits.len() >= MAX_HITS {
                    break;
                }
                let Ok(src) = fs::read_to_string(fp) else {
                    continue;
                };
                let rel = fp
                    .strip_prefix(default_proj)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| fp.display().to_string());
                let spans = parse_ifm_symbol_spans(&src);
                for (idx, line) in src.lines().enumerate() {
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                    let ln = idx + 1;
                    if !line_contains_ident(line, sym) {
                        continue;
                    }
                    let enclosing = spans
                        .iter()
                        .find(|s| ln >= s.start_line && ln <= s.end_line);
                    if let Some(enc) = enclosing {
                        if enc.name == sym && ln == enc.start_line {
                            continue; // skip own declaration header
                        }
                    }
                    hits.push(json!({
                        "file": rel,
                        "line": ln,
                        "enclosing": enclosing.map(|e| format!("{} {}", e.kind, e.name)).unwrap_or_else(|| "<top-level>".to_string()),
                        "snippet": line.trim().chars().take(160).collect::<String>(),
                    }));
                }
            }
            json!({
                "symbol": sym,
                "files_scanned": ifm_files.len(),
                "count": hits.len(),
                "callers": hits,
            })
        }
        "read_infimum_template" => {
            let kind = args.get("kind").and_then(|s| s.as_str()).unwrap_or("list").trim();
            if kind.is_empty() || kind == "list" {
                return json!({
                    "available_templates": ["ui", "game2d", "game3d"],
                    "hint": "Call read_infimum_template(kind='ui' | 'game2d' | 'game3d') for a Z3-verified starter capsule."
                });
            }
            match infimum_template_source(kind) {
                Some((title, source)) => json!({
                    "kind": kind,
                    "title": title,
                    "source": source,
                }),
                None => json!({
                    "error": format!("Unknown template '{kind}'. Available: ui, game2d, game3d")
                }),
            }
        }
        "read_infimum_example" => {
            const EXAMPLES: &[(&str, &str, &[&str])] = &[
                (
                    "color-lines",
                    "2D PBR board game (9x9 Color Lines 98: typed structs, BFS pathfinding, animations, undo/redo snapshots, procedural SFX)",
                    &["abi.ifm", "logic.ifm", "render.ifm", "main.ifm"],
                ),
                (
                    "lion-herd",
                    "3D PBR savanna game (Lion Herd: std/mesh3d articulated 3D animals, flocking AI, day sky, typed structs, textures.json Cloud PBR, SFX)",
                    &["abi.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "ceres-drift",
                    "3D 6-DOF Zero-G Sci-Fi Mine Shooter (Ceres Drift: 11-chamber 3-deck mine, 13 octagonal tubes, 3D radar, homing missiles, reactor escape, 9x 1K Cloud PBR)",
                    &["abi.ifm", "maze.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "slipgate",
                    "3D PBR FPS (SlipGate E1M1: 6-zone citadel, 24 point lights, 16 articulated 3D monsters, minimap, typed structs, textures.json Cloud PBR)",
                    &["abi.ifm", "world.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "gomoku",
                    "2D PBR board game (Gomoku 18x18 Five-in-a-Row: wooden board, 3D PBR stones, mod ai, 3 AI difficulties, 2P mode, undo, procedural SFX)",
                    &["main.ifm"],
                ),
                (
                    "lang-demos",
                    "Compiler & language feature demos from infimum-lang (Telos Z3 anti-stub, Forma UI & built-in widgets_rt, Flux payment & secret-leak, Strata framebuffer, std_tests)",
                    &["anti_stub_demo.ifm", "forma_ui_demo.ifm", "payment_demo.ifm", "secret_leak_demo.ifm", "std_tests.ifm", "strata_framebuffer_demo.ifm", "widgets_rt.ifm"],
                ),
                (
                    "web-cores",
                    "Verified embedded runtime cores & text demo of infimum-web (audio_core, passport_core, sanitizer_core, text_demo)",
                    &["audio_core.ifm", "passport_core.ifm", "sanitizer_core.ifm", "text_demo.ifm"],
                ),
                (
                    "infimum-code",
                    "Flagship Infimum Code native IDE & AI studio written 100% in Infimum Lang (Forma UI, Toggle/Slider modals, FrameScratch Markdown tables, file viewer, timeline, daemon HTTP client, and verified agent/editor/outline cores)",
                    &[
                        "abi.ifm", "infimum_code.ifm", "sidebar.ifm", "feed.ifm", "composer.ifm",
                        "modals.ifm", "file_viewer.ifm", "markdown.ifm", "timeline.ifm",
                        "text_buf.ifm", "daemon_client.ifm", "theme.ifm", "icons.ifm",
                        "primitives.ifm", "agent_core.ifm", "editor_core.ifm", "outline_core.ifm",
                    ],
                ),
                (
                    "rust-lang",
                    "Complete Rust source code & integration tests of the infimum-lang compiler (lexer, parser, typechecker, Z3 Telos verifier, Forma UI lower/verifier, Strata Wasm/ELF codegen, eval, wasm_api)",
                    &[
                        "src/abi.rs", "src/ast.rs", "src/canon.rs", "src/codegen.rs", "src/diag.rs",
                        "src/eval.rs", "src/forma/ast.rs", "src/forma/lower.rs", "src/forma/mod.rs",
                        "src/forma/parser.rs", "src/forma/verify_ui.rs", "src/lexer.rs", "src/lib.rs",
                        "src/main.rs", "src/mcp.rs", "src/parser.rs",
                        "src/showcase.rs", "src/store.rs", "src/strata_elf.rs", "src/strata_wasm.rs",
                        "src/typecheck.rs", "src/verify.rs", "src/wasm.rs", "src/wasm_api.rs",
                        "tests/proof_cache_tests.rs", "tests/sprint1_tests.rs", "tests/sprint2_z3_tests.rs",
                        "tests/sprint3_security_tests.rs", "tests/sprint4_codegen_tests.rs",
                        "tests/sprint5_wasm_timetravel_tests.rs", "tests/sprint6_showcase_tests.rs",
                        "tests/sprint7_strata_tests.rs", "tests/sprint8_modern_syntax_tests.rs",
                        "tests/sprint9_forma_tests.rs",
                    ],
                ),
                (
                    "rust-web",
                    "Complete Rust & WGSL shader source code of the infimum-web native Vulkan/wgpu & WebGPU runtime (GPU renderer, shader.wgsl, mesh3d.wgsl, 3D PBR textures, TrueType glyph atlas, procedural audio, wasmi runtime, clipboard, web_player)",
                    &[
                        "atlas.rs", "audio.rs", "browser_abi.rs", "capabilities.rs", "clipboard.rs",
                        "gpu.rs", "lib.rs", "main.rs", "mesh3d.wgsl", "passport.rs", "runtime.rs",
                        "sanitizer.rs", "shader.wgsl", "textures3d.rs", "web_player.rs",
                    ],
                ),
                (
                    "rust-code",
                    "Complete Rust source code of the infimum-code daemon (infimum-agentd: AI agent loop, sandbox policy, HTTP server, hot-reloading core_loader, studio_abi)",
                    &[
                        "agent/mod.rs", "agent/timeline.rs", "agent/conversations.rs",
                        "agent/tools.rs", "agent/providers.rs", "core_loader.rs",
                        "http_server.rs", "main.rs", "policy.rs", "studio_abi.rs",
                    ],
                ),
                (
                    "infra",
                    "Build/CI scripts (.sh, .py, GitHub Actions .yml), Cargo.toml manifests, Forma UI spec, and Infimum Code system prompt",
                    &[
                        "run_checks.sh", "ci_verify_corpus.sh", "build_studio.sh",
                        "generate_cleanroom_icon.py", "ci.yml",
                        "lang-Cargo.toml", "web-Cargo.toml", "daemon-Cargo.toml",
                        "INFIMUM_FORMA_SPEC.md", "system_prompt.md",
                    ],
                ),
                (
                    "web-site",
                    "Cloudflare Worker, Online Infimum Code Studio, WebGPU Player, and infimum-lang.org frontend sources",
                    &[
                        "src/worker.js", "wrangler.toml",
                        "public/code/code.js", "public/code/agent_worker.js", "public/code/z3_worker.js",
                        "public/code/index.html", "public/player/player.js", "public/player/index.html",
                        "public/index.html", "public/app.js", "public/styles.css",
                    ],
                ),
            ];
            let ex = args.get("example").and_then(|s| s.as_str()).unwrap_or("").trim();
            let raw_file = args.get("file").and_then(|s| s.as_str()).unwrap_or("").trim();
            if ex.is_empty() || ex == "list" {
                let list: Vec<Value> = EXAMPLES
                    .iter()
                    .map(|(name, desc, files)| json!({ "example": name, "description": desc, "files": files }))
                    .collect();
                return json!({
                    "available_examples": list,
                    "hint": "Call read_infimum_example(example='rust-web', file='shader.wgsl', start_line=1, end_line=200) or example='infra', file='ci_verify_corpus.sh'."
                });
            }
            let Some((ex_name, ex_desc, allowed_files)) = EXAMPLES.iter().find(|(n, _, _)| *n == ex) else {
                return json!({
                    "error": format!("Unknown example '{ex}'. Available: infimum-code, ceres-drift, gomoku, color-lines, lion-herd, slipgate, lang-demos, web-cores, rust-lang, rust-web, rust-code, infra, web-site")
                });
            };
            if raw_file.is_empty() || raw_file == "list" {
                return json!({
                    "example": ex_name,
                    "description": ex_desc,
                    "files": allowed_files,
                    "hint": format!("Call read_infimum_example(example='{ex_name}', file='{}') to read a source file.", allowed_files.first().copied().unwrap_or("main.ifm"))
                });
            }
            let norm_raw = raw_file.trim_start_matches('/');
            let file_name = if *ex_name == "rust-lang" {
                if allowed_files.contains(&norm_raw) {
                    norm_raw.to_string()
                } else {
                    let with_src = format!("src/{norm_raw}");
                    if allowed_files.contains(&with_src.as_str()) {
                        with_src
                    } else {
                        norm_raw.to_string()
                    }
                }
            } else if *ex_name == "infra" || *ex_name == "web-site" {
                norm_raw.to_string()
            } else {
                let norm = norm_raw
                    .trim_start_matches("daemon/src/")
                    .trim_start_matches("src/");
                if norm.ends_with(".ifm")
                    || norm.ends_with(".json")
                    || norm.ends_with(".rs")
                    || norm.ends_with(".wgsl")
                {
                    norm.to_string()
                } else if ex_name.starts_with("rust-") {
                    format!("{norm}.rs")
                } else {
                    format!("{norm}.ifm")
                }
            };
            if !allowed_files.contains(&file_name.as_str()) {
                return json!({
                    "error": format!("Unknown file '{raw_file}' in example '{ex_name}'. Allowed: {:?}", allowed_files)
                });
            }
            let mut found = None;
            for root in studio_root_candidates() {
                // Monorepo layout: <monorepo>/apps/infimum-code -> <monorepo>/crates/{infimum-lang,infimum-web}
                // and companion website repo at <monorepo>/../infimum-site.
                let monorepo = root.parent().and_then(|p| p.parent());
                let lang_root = monorepo.map(|m| m.join("crates").join("infimum-lang"));
                let web_root = monorepo.map(|m| m.join("crates").join("infimum-web"));
                let site_root = monorepo
                    .and_then(|m| m.parent())
                    .map(|mp| mp.join("infimum-site"));

                let cands: Vec<PathBuf> = match *ex_name {
                    "lang-demos" => lang_root
                        .as_ref()
                        .map(|lr| {
                            vec![
                                lr.join("examples").join(&file_name),
                                lr.join("tests").join(&file_name),
                                lr.join("src").join("forma").join(&file_name),
                            ]
                        })
                        .unwrap_or_default(),
                    "web-cores" => web_root
                        .as_ref()
                        .map(|wr| {
                            vec![
                                wr.join("capsules").join("core").join(&file_name),
                                wr.join("capsules").join("m2_text_demo").join("src").join(&file_name),
                            ]
                        })
                        .unwrap_or_default(),
                    "infimum-code" => vec![
                        root.join("capsule").join("src").join(&file_name),
                        root.join("cores").join(&file_name),
                    ],
                    "rust-lang" => lang_root
                        .as_ref()
                        .map(|lr| vec![lr.join(&file_name)])
                        .unwrap_or_default(),
                    "rust-web" => web_root
                        .as_ref()
                        .map(|wr| vec![wr.join("src").join(&file_name)])
                        .unwrap_or_default(),
                    "rust-code" => vec![root.join("daemon").join("src").join(&file_name)],
                    "infra" => match file_name.as_str() {
                        "run_checks.sh" | "ci_verify_corpus.sh" => lang_root
                            .as_ref()
                            .map(|lr| vec![lr.join("scripts").join(&file_name)])
                            .unwrap_or_default(),
                        "build_studio.sh" | "generate_cleanroom_icon.py" => web_root
                            .as_ref()
                            .map(|wr| vec![wr.join("scripts").join(&file_name)])
                            .unwrap_or_default(),
                        "ci.yml" => monorepo
                            .map(|m| vec![m.join(".github").join("workflows").join("ci.yml")])
                            .unwrap_or_default(),
                        "lang-Cargo.toml" => lang_root
                            .as_ref()
                            .map(|lr| vec![lr.join("Cargo.toml")])
                            .unwrap_or_default(),
                        "web-Cargo.toml" => web_root
                            .as_ref()
                            .map(|wr| vec![wr.join("Cargo.toml")])
                            .unwrap_or_default(),
                        "daemon-Cargo.toml" => vec![root.join("daemon").join("Cargo.toml")],
                        "INFIMUM_FORMA_SPEC.md" => lang_root
                            .as_ref()
                            .map(|lr| vec![lr.join("docs").join("INFIMUM_FORMA_SPEC.md")])
                            .unwrap_or_default(),
                        "system_prompt.md" => vec![root.join("capsule").join("system_prompt.md")],
                        _ => Vec::new(),
                    },
                    "web-site" => site_root
                        .as_ref()
                        .map(|sr| vec![sr.join(&file_name)])
                        .unwrap_or_default(),
                    _ => {
                        let mut v = vec![
                            root.join("examples").join(ex_name).join("src").join(&file_name),
                            root.join("examples").join(ex_name).join(&file_name),
                        ];
                        if let Some(lr) = &lang_root {
                            v.push(lr.join("examples").join("showcase").join(ex_name).join("src").join(&file_name));
                            v.push(lr.join("examples").join("showcase").join(ex_name).join(&file_name));
                        }
                        v
                    }
                };
                if let Some(hit) = cands.into_iter().find(|c| c.is_file()) {
                    found = Some(hit);
                    break;
                }
            }
            let Some(path) = found else {
                return json!({ "error": format!("Source '{ex_name}/{file_name}' not found") });
            };
            match fs::read_to_string(&path) {
                Ok(content) => {
                    let all_lines: Vec<&str> = content.lines().collect();
                    let total_lines = all_lines.len();
                    let start_arg = args.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                    let end_arg = args.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                    if start_arg.is_some() || end_arg.is_some() {
                        let s = start_arg.unwrap_or(1).max(1);
                        let e = end_arg.unwrap_or(total_lines).max(s).min(total_lines);
                        let slice = if s <= total_lines && total_lines > 0 {
                            all_lines[(s - 1)..e].join("\n")
                        } else {
                            String::new()
                        };
                        json!({
                            "example": ex_name,
                            "file": file_name,
                            "path": format!("{ex_name}/{file_name}"),
                            "start_line": s,
                            "end_line": e,
                            "total_lines": total_lines,
                            "content": slice,
                        })
                    } else {
                        json!({
                            "example": ex_name,
                            "file": file_name,
                            "path": format!("{ex_name}/{file_name}"),
                            "total_lines": total_lines,
                            "content": content,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("").trim();
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10).clamp(1, 20);
            let encoded_q: String = q
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
                    b' ' => "+".to_string(),
                    _ => format!("%{:02X}", b),
                })
                .collect();
            let url = format!("https://infimum-lang.org/api/textures/search?q={encoded_q}&limit={limit}");
            match Command::new("curl")
                .args(["-fsSL", "--max-time", "15", &url])
                .output()
            {
                Ok(o) if o.status.success() => {
                    let body = String::from_utf8_lossy(&o.stdout);
                    serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({ "raw": body }))
                }
                Ok(o) => json!({ "error": format!("Texture search failed: {}", String::from_utf8_lossy(&o.stderr).trim()) }),
                Err(e) => json!({ "error": format!("curl failed: {e}") }),
            }
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(-1);
            let tex_id = args
                .get("texture_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !(0..=11).contains(&slot) {
                return json!({ "error": format!("Invalid slot {slot}. Must be 0..11.") });
            }
            if tex_id.is_empty() || !tex_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return json!({ "error": format!("Invalid texture_id '{tex_id}'. Must match ^[a-z0-9_]+$.") });
            }
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let cache_dir = PathBuf::from(&home).join(".cache").join("infimum-web").join("textures").join(&tex_id);
            let proj_tex_dir = resolve("textures");
            if let Err(e) = fs::create_dir_all(&cache_dir).and_then(|_| fs::create_dir_all(&proj_tex_dir)) {
                return json!({ "error": format!("Failed to create texture dirs: {e}") });
            }
            // Remove any previous files bound to this slot in project/textures/
            let slot_prefix = format!("{slot}_");
            if let Ok(entries) = fs::read_dir(&proj_tex_dir) {
                for ent in entries.flatten() {
                    if let Some(fname) = ent.file_name().to_str() {
                        if fname.starts_with(&slot_prefix) {
                            let _ = fs::remove_file(ent.path());
                        }
                    }
                }
            }
            let mut downloaded = Vec::new();
            for kind in ["diff", "norm", "rough"] {
                let cached_file = cache_dir.join(format!("{kind}.jpg"));
                if !cached_file.is_file() || fs::metadata(&cached_file).map(|m| m.len()).unwrap_or(0) == 0 {
                    let url = format!("https://infimum-lang.org/textures/{tex_id}/{kind}.jpg");
                    let tmp = cache_dir.join(format!("{kind}.jpg.tmp"));
                    let ok = Command::new("curl")
                        .args(["-fsSL", "--max-time", "25", "-o", &tmp.display().to_string(), &url])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if ok && fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) > 0 {
                        let _ = fs::rename(&tmp, &cached_file);
                    } else {
                        let _ = fs::remove_file(&tmp);
                    }
                }
                if cached_file.is_file() {
                    let dst = proj_tex_dir.join(format!("{slot}_{tex_id}_{kind}.jpg"));
                    #[cfg(unix)]
                    let linked = std::os::unix::fs::symlink(&cached_file, &dst).is_ok();
                    #[cfg(not(unix))]
                    let linked = false;
                    if linked || fs::copy(&cached_file, &dst).is_ok() {
                        downloaded.push(kind);
                    }
                }
            }
            if downloaded.is_empty() {
                return json!({ "error": format!("Texture '{tex_id}' not found on https://infimum-lang.org/textures/") });
            }
            // Also persist declarative textures.json in project root
            let tex_json_path = resolve("textures.json");
            let mut map = fs::read_to_string(&tex_json_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
                .unwrap_or_default();
            map.insert(slot.to_string(), Value::String(tex_id.clone()));
            if let Ok(pretty) = serde_json::to_string_pretty(&Value::Object(map)) {
                let _ = fs::write(&tex_json_path, format!("{pretty}\n"));
            }
            json!({
                "ok": true,
                "slot": slot,
                "texture_id": tex_id,
                "maps": downloaded,
                "cache_dir": cache_dir.display().to_string(),
            })
        }
        "verify_infimum_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            let bin = resolve_infimum_bin(policy);
            match Command::new(&bin).args(["verify", &p.display().to_string()]).output() {
                Ok(o) => {
                    let out = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    json!({
                        "ok": o.status.success(),
                        "output": out.chars().take(3500).collect::<String>(),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "compile_infimum_wasm" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            let out_p = match args.get("output").and_then(|s| s.as_str()).filter(|s| !s.trim().is_empty()) {
                Some(o) => resolve(o),
                None => p.with_extension("axw"),
            };
            if !is_jailed(&out_p) {
                return json!({ "error": "Output path denied outside active project sandbox" });
            }
            if let Some(parent) = out_p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let bin = resolve_infimum_bin(policy);
            match Command::new(&bin)
                .args(["compile-wasm", &p.display().to_string(), "-o", &out_p.display().to_string()])
                .output()
            {
                Ok(o) => {
                    let out = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    let size_bytes = fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
                    json!({
                        "ok": o.status.success(),
                        "output_path": out_p.display().to_string(),
                        "size_bytes": size_bytes,
                        "output": out.chars().take(3500).collect::<String>(),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "run_infimum_capsule" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            if !p.is_file() {
                return json!({ "error": format!("Capsule file not found: {}", p.display()) });
            }
            let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(true);
            let web_bin = resolve_infimum_web_bin();
            if headless {
                match Command::new(&web_bin)
                    .args(["--headless-check", &p.display().to_string()])
                    .output()
                {
                    Ok(o) => {
                        let out = format!(
                            "{}{}",
                            String::from_utf8_lossy(&o.stdout),
                            String::from_utf8_lossy(&o.stderr)
                        );
                        json!({
                            "ok": o.status.success(),
                            "mode": "headless",
                            "output": out.chars().take(3500).collect::<String>(),
                        })
                    }
                    Err(e) => json!({ "error": e.to_string() }),
                }
            } else {
                match Command::new(&web_bin)
                    .arg(&p)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => json!({
                        "ok": true,
                        "mode": "window",
                        "pid": child.id(),
                        "path": p.display().to_string(),
                        "message": format!("Opened native infimum-web window for {}", p.display()),
                    }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
        }
        "run_project_command" => {
            if !policy.allow_proc {
                return json!({ "error": "Proc capability denied (launch with --allow-proc)" });
            }
            let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
            let cwd = args
                .get("cwd")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if !is_jailed(&cwd) {
                return json!({ "error": "Invalid cwd: outside active project sandbox" });
            }
            let Ok(proj_canon) = fs::canonicalize(default_proj) else {
                return json!({ "error": "Active project directory does not exist" });
            };
            let Ok(cwd_canon) = fs::canonicalize(&cwd) else {
                return json!({ "error": "Working directory does not exist" });
            };
            let bwrap_path = Path::new("/usr/bin/bwrap");
            if !bwrap_path.is_file() {
                return json!({
                    "error": "Sandbox /usr/bin/bwrap not installed — refusing unsandboxed shell execution."
                });
            }
            let home = env::var("HOME").unwrap_or_default();
            let cargo_dir = PathBuf::from(&home).join(".cargo");
            let rustup_dir = PathBuf::from(&home).join(".rustup");
            let infimum_bin = resolve_infimum_bin(policy);
            let z3_bin = resolve_z3_bin();
            let sandbox_path = format!(
                "/tmp/infimum-bin:{}/bin:/usr/local/bin:/usr/bin:/bin",
                cargo_dir.display()
            );
            let wrapped = format!("({cmd}) 2>&1");
            ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);
            let mut command = Command::new(bwrap_path);
            command
                .args([
                    "--die-with-parent",
                    "--unshare-net",
                    "--unshare-ipc",
                    "--clearenv",
                    "--setenv",
                    "PATH",
                    &sandbox_path,
                    "--setenv",
                    "HOME",
                    "/tmp/infimum-sandbox-home",
                    "--setenv",
                    "LANG",
                    "C.UTF-8",
                    "--ro-bind",
                    "/usr",
                    "/usr",
                    "--ro-bind-try",
                    "/bin",
                    "/bin",
                    "--ro-bind-try",
                    "/lib",
                    "/lib",
                    "--ro-bind-try",
                    "/lib64",
                    "/lib64",
                    "--ro-bind-try",
                    "/etc",
                    "/etc",
                    "--dev",
                    "/dev",
                    "--proc",
                    "/proc",
                    "--tmpfs",
                    "/tmp",
                    "--dir",
                    "/tmp/infimum-sandbox-home",
                    "--dir",
                    "/tmp/infimum-bin",
                ]);
            if infimum_bin.is_file() {
                command.args([
                    "--ro-bind",
                    &infimum_bin.display().to_string(),
                    "/tmp/infimum-bin/infimum",
                ]);
            }
            if let Some(ref z3_p) = z3_bin {
                command.args([
                    "--ro-bind",
                    &z3_p.display().to_string(),
                    "/tmp/infimum-bin/z3",
                    "--setenv",
                    "INFIMUM_Z3_BIN",
                    "/tmp/infimum-bin/z3",
                ]);
            }
            if cargo_dir.is_dir() {
                command.args([
                    "--ro-bind",
                    &cargo_dir.display().to_string(),
                    &cargo_dir.display().to_string(),
                    "--setenv",
                    "CARGO_HOME",
                    &cargo_dir.display().to_string(),
                ]);
            }
            if rustup_dir.is_dir() {
                command.args([
                    "--ro-bind",
                    &rustup_dir.display().to_string(),
                    &rustup_dir.display().to_string(),
                    "--setenv",
                    "RUSTUP_HOME",
                    &rustup_dir.display().to_string(),
                ]);
            }
            command
                .args([
                    "--bind",
                    &proj_canon.display().to_string(),
                    &proj_canon.display().to_string(),
                    "--chdir",
                    &cwd_canon.display().to_string(),
                    "--",
                    "bash",
                    "-c",
                    &wrapped,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::piped());
            #[cfg(unix)]
            {
                command.process_group(0);
            }
            match command.spawn() {
                Ok(mut child) => {
                    let pid = child.id();
                    ASYNC_CMD_PID.store(pid, Ordering::SeqCst);
                    let lines_buf: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
                    let stderr_buf: Arc<Mutex<String>> = Arc::new(Mutex::new(String::new()));
                    let stderr_handle = child.stderr.take().map(|mut stderr| {
                        let sbuf = Arc::clone(&stderr_buf);
                        std::thread::spawn(move || {
                            use std::io::Read;
                            let mut tmp = String::new();
                            let _ = stderr.read_to_string(&mut tmp);
                            if let Ok(mut g) = sbuf.lock() {
                                *g = tmp;
                            }
                        })
                    });
                    let reader_handle = child.stdout.take().map(|stdout| {
                        let buf_clone = Arc::clone(&lines_buf);
                        std::thread::spawn(move || {
                            let mut reader = BufReader::new(stdout);
                            let mut line = String::new();
                            while let Ok(n) = reader.read_line(&mut line) {
                                if n == 0 {
                                    break;
                                }
                                let clean = line.trim_end_matches(&['\r', '\n'][..]).to_string();
                                line.clear();
                                let tail_str = {
                                    let mut g = buf_clone.lock().unwrap();
                                    g.push(clean);
                                    let start = g.len().saturating_sub(4);
                                    g[start..]
                                        .iter()
                                        .map(|l| l.chars().take(96).collect::<String>())
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                };
                                if let Some(sidx) = step_idx {
                                    timeline_update_step_tail(sidx, tail_str);
                                }
                            }
                        })
                    });

                    let cmd_start = Instant::now();
                    let mut killed_by_user = false;
                    let mut timed_out = false;
                    let mut exit_code: i32 = -1;
                    loop {
                        match child.try_wait() {
                            Ok(Some(st)) => {
                                exit_code = st.code().unwrap_or(-1);
                                if ASYNC_CMD_KILL_REQUESTED.load(Ordering::SeqCst) {
                                    killed_by_user = true;
                                }
                                break;
                            }
                            Ok(None) => {
                                if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                                    #[cfg(unix)]
                                    {
                                        signal_process_group(pid, SIGKILL_NUM);
                                    }
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    exit_code = -9;
                                    break;
                                }
                                if ASYNC_CMD_KILL_REQUESTED.load(Ordering::SeqCst) {
                                    killed_by_user = true;
                                    #[cfg(unix)]
                                    {
                                        signal_process_group(pid, SIGTERM_NUM);
                                        std::thread::sleep(Duration::from_millis(120));
                                        signal_process_group(pid, SIGKILL_NUM);
                                    }
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    exit_code = -9;
                                    break;
                                }
                                if cmd_start.elapsed() > Duration::from_secs(180) {
                                    timed_out = true;
                                    #[cfg(unix)]
                                    {
                                        signal_process_group(pid, SIGKILL_NUM);
                                    }
                                    let _ = child.kill();
                                    let _ = child.wait();
                                    exit_code = -124;
                                    break;
                                }
                                std::thread::sleep(Duration::from_millis(40));
                            }
                            Err(_) => break,
                        }
                    }
                    ASYNC_CMD_PID.store(0, Ordering::SeqCst);
                    ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);
                    if let Some(h) = reader_handle {
                        let _ = h.join();
                    }
                    if let Some(h) = stderr_handle {
                        let _ = h.join();
                    }
                    let mut all_output = lines_buf
                        .lock()
                        .map(|g| g.join("\n"))
                        .unwrap_or_default();
                    let bwrap_err = stderr_buf
                        .lock()
                        .map(|g| g.trim().to_string())
                        .unwrap_or_default();
                    // On Ubuntu 24.04 launched from GNOME desktop, AppArmor may block unprivileged
                    // user namespaces for `bwrap`. When that happens, fall back to a scrubbed-env
                    // (`env_clear`) bash inside `cwd_canon` with path-escape & network-tool guards.
                    let is_userns_blocked = exit_code != 0
                        && all_output.is_empty()
                        && (bwrap_err.contains("uid map")
                            || bwrap_err.contains("namespace")
                            || bwrap_err.contains("Permission denied")
                            || bwrap_err.contains("Operation not permitted")
                            || bwrap_err.contains("bwrap:"));
                    if is_userns_blocked && !killed_by_user && !timed_out {
                        let lower_cmd = cmd.to_ascii_lowercase();
                        let forbidden = lower_cmd.contains("..")
                            || lower_cmd.contains("/etc/")
                            || lower_cmd.contains("curl ")
                            || lower_cmd.contains("wget ")
                            || lower_cmd.contains("ssh ")
                            || lower_cmd.contains("scp ")
                            || lower_cmd.contains("nc ")
                            || lower_cmd.contains("find /");
                        if forbidden {
                            all_output = format!("Sandbox denied potentially unsafe command in fallback mode: {cmd}");
                            exit_code = 1;
                        } else {
                            let mut fb_path = String::from("/usr/local/bin:/usr/bin:/bin");
                            if let Some(p) = infimum_bin.parent() {
                                fb_path = format!("{}:{fb_path}", p.display());
                            }
                            if let Some(p) = z3_bin.as_ref().and_then(|z| z.parent()) {
                                fb_path = format!("{}:{fb_path}", p.display());
                            }
                            if cargo_dir.is_dir() {
                                fb_path = format!("{}/bin:{fb_path}", cargo_dir.display());
                            }
                            let mut fb = Command::new("bash");
                            fb.arg("-c")
                                .arg(&wrapped)
                                .current_dir(&cwd_canon)
                                .env_clear()
                                .env("PATH", fb_path)
                                .env("HOME", &home)
                                .env("LANG", "C.UTF-8");
                            if let Some(ref z3_p) = z3_bin {
                                fb.env("INFIMUM_Z3_BIN", z3_p);
                            }
                            if cargo_dir.is_dir() {
                                fb.env("CARGO_HOME", &cargo_dir);
                            }
                            if rustup_dir.is_dir() {
                                fb.env("RUSTUP_HOME", &rustup_dir);
                            }
                            if let Ok(out) = fb.output() {
                                exit_code = out.status.code().unwrap_or(-1);
                                all_output = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                                let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
                                if !se.is_empty() {
                                    if !all_output.is_empty() {
                                        all_output.push('\n');
                                    }
                                    all_output.push_str(&se);
                                }
                            }
                        }
                    } else if !bwrap_err.is_empty() {
                        if !all_output.is_empty() {
                            all_output.push('\n');
                        }
                        all_output.push_str(&bwrap_err);
                    }
                    let full = format!("$ {cmd}\n{all_output}");
                    let trunc: String = full.chars().take(5000).collect();
                    if killed_by_user {
                        json!({
                            "exit_code": exit_code,
                            "killed_by_user": true,
                            "output": trunc,
                            "note": "Команда принудительно прервана пользователем через кнопку прерывания."
                        })
                    } else if timed_out {
                        json!({
                            "exit_code": exit_code,
                            "timed_out": true,
                            "output": trunc,
                            "note": "Команда превысила лимит времени (180 с) и была остановлена."
                        })
                    } else {
                        json!({
                            "exit_code": exit_code,
                            "output": trunc,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        _ => json!({ "error": format!("Unknown tool '{name}'") }),
    }
}

