//! Minimal zero-dependency HTTP/1.1 JSON server for `infimum-agentd`.
//!
//! Security model:
//! - Binds strictly to `127.0.0.1:<port>` (loopback only).
//! - Every endpoint except `GET /health` requires `Authorization: Bearer <token>`
//!   verified with constant-time comparison.

use crate::agent;
use crate::policy::CapabilityPolicy;
use crate::studio_abi::LIVE_TIMELINE_MAX_WORDS;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::path::PathBuf;
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Instant;

#[derive(Default)]
struct AsyncAgentState {
    status: i64, // 0=idle, 1=running, 2=done
    started_at: Option<Instant>,
    elapsed_tenths: i64,
    cost_milli_ils: i64,
    total_tokens: i64,
    status_line: String,
    reply: String,
    timeline: Option<agent::TimelineSnapshot>,
}

fn async_agent_cell() -> &'static Mutex<AsyncAgentState> {
    static CELL: OnceLock<Mutex<AsyncAgentState>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(AsyncAgentState::default()))
}

fn cached_conv_paths() -> &'static Mutex<Vec<PathBuf>> {
    static CELL: OnceLock<Mutex<Vec<PathBuf>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Vec::new()))
}

struct CachedLoadedMsg {
    role: i32,
    text: String,
    timeline_words: Vec<i64>,
    images: Vec<agent::AttachedImage>,
}

fn cached_loaded_msgs() -> &'static Mutex<Vec<CachedLoadedMsg>> {
    static CELL: OnceLock<Mutex<Vec<CachedLoadedMsg>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Vec::new()))
}

pub struct ServerContext {
    pub policy: CapabilityPolicy,
    pub token: String,
    pub shutdown: AtomicBool,
}

fn constant_time_eq(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (&x, &y) in a.iter().zip(b.iter()) {
        diff |= x ^ y;
    }
    diff == 0
}

fn runtime_env_overrides() -> &'static Mutex<std::collections::HashMap<String, String>> {
    static CELL: OnceLock<Mutex<std::collections::HashMap<String, String>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(std::collections::HashMap::new()))
}

const PROVIDER_ENV_KEYS: [&str; 7] = [
    "GEMINI_API_KEY",
    "OPENAI_API_KEY",
    "DEEPSEEK_API_KEY",
    "OPENROUTER_API_KEY",
    "GROQ_API_KEY",
    "ZAI_API_KEY",
    "MISTRAL_API_KEY",
];

fn load_env_key_host_only(key_name: &str) -> Option<String> {
    if let Ok(g) = runtime_env_overrides().lock() {
        if let Some(v) = g.get(key_name) {
            if !v.trim().is_empty() {
                return Some(v.trim().to_string());
            } else {
                return None;
            }
        }
    }
    if let Ok(k) = env::var(key_name) {
        if !k.trim().is_empty() {
            return Some(k.trim().to_string());
        }
    }
    let home = env::var("HOME").ok()?;
    let env_path = PathBuf::from(home).join(".env");
    let content = fs::read_to_string(env_path).ok()?;
    let prefix = format!("{key_name}=");
    for line in content.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix(&prefix) {
            let val = rest.trim().trim_matches('"').trim_matches('\'');
            if !val.is_empty() {
                return Some(val.to_string());
            }
        }
    }
    None
}

fn compute_keys_status_mask() -> i64 {
    let mut mask = 0i64;
    for (i, k) in PROVIDER_ENV_KEYS.iter().enumerate() {
        if load_env_key_host_only(k).is_some() {
            mask |= 1i64 << i;
        }
    }
    mask
}

fn save_env_key_to_dotenv(key_name: &str, raw_val: &str) -> Result<(), String> {
    let clean = raw_val
        .trim()
        .trim_matches('"')
        .trim_matches('\'')
        .replace(['\r', '\n'], "");
    let home = env::var("HOME").map_err(|_| "HOME not set".to_string())?;
    let env_path = PathBuf::from(home).join(".env");
    let existing = fs::read_to_string(&env_path).unwrap_or_default();
    let prefix = format!("{key_name}=");
    let mut out_lines: Vec<String> = Vec::new();
    let mut replaced = false;
    for line in existing.lines() {
        if line.trim_start().starts_with(&prefix) {
            if !clean.is_empty() && !replaced {
                out_lines.push(format!("{key_name}={clean}"));
                replaced = true;
            }
        } else {
            out_lines.push(line.to_string());
        }
    }
    if !clean.is_empty() && !replaced {
        out_lines.push(format!("{key_name}={clean}"));
    }
    let mut body = out_lines.join("\n");
    if !body.ends_with('\n') && !body.is_empty() {
        body.push('\n');
    }
    fs::write(&env_path, body).map_err(|e| format!("Failed writing ~/.env: {e}"))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(&env_path, fs::Permissions::from_mode(0o600));
    }
    if let Ok(mut g) = runtime_env_overrides().lock() {
        g.insert(key_name.to_string(), clean);
    }
    Ok(())
}

fn test_provider_key_live(prov_idx: usize) -> String {
    let Some(&key_name) = PROVIDER_ENV_KEYS.get(prov_idx) else {
        return "Ошибка: неизвестный провайдер".to_string();
    };
    let Some(api_key) = load_env_key_host_only(key_name) else {
        return "Ошибка: ключ не задан".to_string();
    };
    if api_key.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0) {
        return "Ошибка: некорректные символы в ключе".to_string();
    }
    let esc_key = api_key.replace('\\', "\\\\").replace('"', "\\\"");
    let (url, header_line) = match prov_idx {
        0 => (
            "https://generativelanguage.googleapis.com/v1beta/models",
            format!("x-goog-api-key: {esc_key}"),
        ),
        1 => (
            "https://api.openai.com/v1/models",
            format!("Authorization: Bearer {esc_key}"),
        ),
        2 => (
            "https://api.deepseek.com/v1/models",
            format!("Authorization: Bearer {esc_key}"),
        ),
        3 => (
            "https://openrouter.ai/api/v1/auth/key",
            format!("Authorization: Bearer {esc_key}"),
        ),
        4 => (
            "https://api.groq.com/openai/v1/models",
            format!("Authorization: Bearer {esc_key}"),
        ),
        5 => (
            "https://api.z.ai/api/coding/paas/v4/models",
            format!("Authorization: Bearer {esc_key}"),
        ),
        6 => (
            "https://api.mistral.ai/v1/models",
            format!("Authorization: Bearer {esc_key}"),
        ),
        _ => return "Ошибка: неизвестный провайдер".to_string(),
    };
    // Pass API key strictly via `--config -` (stdin), never in CLI argv / `ps aux`.
    let cfg = format!("url = \"{url}\"\nheader = \"{header_line}\"\n");
    let mut child = match Command::new("curl")
        .args([
            "-sS",
            "--proto",
            "=https",
            "-m",
            "8",
            "-o",
            "/dev/null",
            "-w",
            "%{http_code}",
            "--config",
            "-",
        ])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(_) => return "Ошибка: не удалось запустить curl".to_string(),
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg.as_bytes());
    }
    match child.wait_with_output() {
        Ok(o) => {
            let code = String::from_utf8_lossy(&o.stdout).trim().to_string();
            match code.as_str() {
                "200" => "OK: Ключ работает (HTTP 200)".to_string(),
                "401" | "403" => format!("Ошибка: неверный ключ (HTTP {code})"),
                "429" => "Ошибка: лимит/баланс исчерпан (HTTP 429)".to_string(),
                "" => "Ошибка: нет ответа от сервера".to_string(),
                other => format!("Ответ сервера: HTTP {other}"),
            }
        }
        Err(_) => "Ошибка: не удалось запустить curl".to_string(),
    }
}

fn resolve_model_provider_meta(
    model: &str,
) -> (&'static str, &'static str, &'static str, &'static str) {
    if let Some((prov, _)) = model.split_once("::") {
        match prov {
            "openai" => (
                "OpenAI",
                "api.openai.com",
                "OPENAI_API_KEY",
                "https://platform.openai.com/api-keys",
            ),
            "deepseek" => (
                "DeepSeek API",
                "api.deepseek.com",
                "DEEPSEEK_API_KEY",
                "https://platform.deepseek.com/api_keys",
            ),
            "openrouter" => (
                "OpenRouter",
                "openrouter.ai",
                "OPENROUTER_API_KEY",
                "https://openrouter.ai/keys",
            ),
            "groq" => (
                "GroqCloud",
                "api.groq.com",
                "GROQ_API_KEY",
                "https://console.groq.com/keys",
            ),
            "zai" => (
                "Z.ai GLM",
                "api.z.ai",
                "ZAI_API_KEY",
                "https://z.ai/manage-apikey/apikey-list",
            ),
            "mistral" => (
                "Mistral AI",
                "api.mistral.ai",
                "MISTRAL_API_KEY",
                "https://console.mistral.ai/api-keys/",
            ),
            "cerebras" => (
                "Cerebras",
                "api.cerebras.ai",
                "CEREBRAS_API_KEY",
                "https://cloud.cerebras.ai/",
            ),
            "nvidia" => (
                "NVIDIA NIM",
                "integrate.api.nvidia.com",
                "NVIDIA_API_KEY",
                "https://build.nvidia.com/",
            ),
            _ => (
                "Google Gemini",
                "generativelanguage.googleapis.com",
                "GEMINI_API_KEY",
                "https://aistudio.google.com/app/api-keys",
            ),
        }
    } else {
        (
            "Google Gemini",
            "generativelanguage.googleapis.com",
            "GEMINI_API_KEY",
            "https://aistudio.google.com/app/api-keys",
        )
    }
}

fn pick_directory_dialog(initial: Option<&std::path::Path>) -> Option<PathBuf> {
    let mut cmd = Command::new("zenity");
    cmd.args([
        "--file-selection",
        "--directory",
        "--title=Выбрать папку проекта для Infimum Code",
    ]);
    if let Some(dir) = initial {
        cmd.arg(format!("--filename={}/", dir.display()));
    }
    if let Ok(o) = cmd.output() {
        if o.status.success() {
            let s = String::from_utf8_lossy(&o.stdout).trim().to_string();
            if !s.is_empty() {
                return Some(PathBuf::from(s));
            }
        }
    }
    None
}

fn is_viewable_source_file(name: &str) -> bool {
    let lower = name.to_ascii_lowercase();
    lower.ends_with(".ifm")
        || lower.ends_with(".json")
        || lower.ends_with(".md")
        || lower.ends_with(".toml")
        || lower.ends_with(".txt")
}

fn collect_project_files_in_dir(
    root: &std::path::Path,
    dir: &std::path::Path,
    depth: usize,
    out: &mut Vec<String>,
) {
    if depth > 2 || out.len() >= 120 {
        return;
    }
    let Ok(entries) = fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if out.len() >= 120 {
            break;
        }
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.starts_with('.') {
            continue;
        }
        if path.is_dir() {
            if matches!(
                fname.as_str(),
                "target" | "node_modules" | "logs" | "dist" | "build"
            ) {
                continue;
            }
            collect_project_files_in_dir(root, &path, depth + 1, out);
        } else if path.is_file() && is_viewable_source_file(&fname) {
            if let Ok(rel) = path.strip_prefix(root) {
                let rel_str = rel.to_string_lossy().replace('\\', "/");
                out.push(rel_str);
            }
        }
    }
}

fn collect_project_files(ws: &std::path::Path) -> Vec<String> {
    let mut files = Vec::new();
    collect_project_files_in_dir(ws, ws, 0, &mut files);
    files.sort_by(|a, b| {
        let a_main = a == "main.ifm" || a == "src/main.ifm";
        let b_main = b == "main.ifm" || b == "src/main.ifm";
        match (a_main, b_main) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a.cmp(b),
        }
    });
    files
}

fn resolve_project_workspaces(policy: &CapabilityPolicy) -> Vec<PathBuf> {
    if policy.allow_user_workspaces {
        agent::load_allowed_workspaces(policy)
    } else {
        policy
            .allowed_fs_roots
            .iter()
            .filter(|p| p.is_dir())
            .cloned()
            .collect()
    }
}

fn find_workspace_by_name(policy: &CapabilityPolicy, proj_name: &str) -> Option<PathBuf> {
    let target = proj_name.trim();
    if target.is_empty() {
        return None;
    }
    resolve_project_workspaces(policy)
        .into_iter()
        .find(|p| p.file_name().map(|f| f.to_string_lossy()) == Some(target.into()))
}

fn newest_ifm_mtime(dir: &std::path::Path, depth: usize) -> Option<std::time::SystemTime> {
    if depth > 2 {
        return None;
    }
    let mut newest: Option<std::time::SystemTime> = None;
    let Ok(entries) = fs::read_dir(dir) else {
        return None;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let fname = entry.file_name().to_string_lossy().to_string();
        if fname.starts_with('.') || matches!(fname.as_str(), "target" | "node_modules") {
            continue;
        }
        let cand = if path.is_dir() {
            newest_ifm_mtime(&path, depth + 1)
        } else if fname.ends_with(".ifm") {
            entry.metadata().ok().and_then(|m| m.modified().ok())
        } else {
            None
        };
        if let Some(t) = cand {
            if newest.map(|cur| t > cur).unwrap_or(true) {
                newest = Some(t);
            }
        }
    }
    newest
}

fn find_existing_ifw_in_project(ws: &std::path::Path, proj_name: &str) -> Option<PathBuf> {
    let clean_name: String = proj_name
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() || c == '_' { c } else { '_' })
        .collect();
    let preferred = [
        ws.join(format!("{proj_name}.ifw")),
        ws.join(format!("{clean_name}.ifw")),
        ws.join("main.ifw"),
        ws.join("capsule").join(format!("{clean_name}.ifw")),
    ];
    for p in preferred {
        if p.is_file() {
            return Some(p);
        }
    }
    if let Ok(entries) = fs::read_dir(ws) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("ifw") {
                return Some(p);
            }
        }
    }
    None
}

fn find_entry_ifm_in_project(ws: &std::path::Path) -> Option<PathBuf> {
    for rel in ["main.ifm", "src/main.ifm"] {
        let cand = ws.join(rel);
        if cand.is_file() {
            return Some(cand);
        }
    }
    if let Ok(entries) = fs::read_dir(ws) {
        for entry in entries.flatten() {
            let p = entry.path();
            if p.is_file() && p.extension().and_then(|e| e.to_str()) == Some("ifm") {
                return Some(p);
            }
        }
    }
    None
}

fn decode_base64_bytes(input: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(input.len() * 3 / 4);
    let mut buf = 0u32;
    let mut bits = 0u32;
    for b in input.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\r' | b'\n' | b' ' | b'\t' => continue,
            _ => return None,
        };
        buf = (buf << 6) | (v as u32);
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push(((buf >> bits) & 0xFF) as u8);
        }
    }
    Some(out)
}

fn write_http_bytes(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    payload: &[u8],
) {
    let header = format!(
        "HTTP/1.1 {status} {reason}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        payload.len()
    );
    let _ = stream.write_all(header.as_bytes());
    let _ = stream.write_all(payload);
    let _ = stream.flush();
}

fn write_http_response(
    stream: &mut TcpStream,
    status: u16,
    reason: &str,
    content_type: &str,
    payload: &str,
) {
    write_http_bytes(stream, status, reason, content_type, payload.as_bytes());
}

/// Convert a JSON response from `handle_request` into the compact plain-text wire format
/// (`?format=ifm`) consumed directly by `infimum_code.ifm` over `OP_NET_HTTP_REQUEST`.
fn format_ifm_payload(method: &str, path: &str, v: &Value) -> String {
    match (method, path) {
        ("GET", "/workspaces") | ("POST", "/workspaces") => v
            .get("names")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        ("DELETE", p) if p.starts_with("/workspaces/") => v
            .get("names")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        ("GET", "/conversations") | ("POST", "/conversations/delete") => v
            .get("labels")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        ("GET", "/files") => v
            .get("files")
            .and_then(|a| a.as_array())
            .map(|arr| {
                arr.iter()
                    .filter_map(|x| x.as_str())
                    .collect::<Vec<_>>()
                    .join("\n")
            })
            .unwrap_or_default(),
        ("POST", "/files/open") => {
            let total = v.get("total_lines").and_then(|x| x.as_u64()).unwrap_or(1);
            let ws = v.get("win_start").and_then(|x| x.as_u64()).unwrap_or(0);
            let wc = v.get("win_count").and_then(|x| x.as_u64()).unwrap_or(0);
            let content = v.get("content").and_then(|s| s.as_str()).unwrap_or("");
            format!("{total} {ws} {wc}\n{content}")
        }
        ("GET", "/keys/status") | ("POST", "/keys/save") => v
            .get("mask")
            .and_then(|x| x.as_i64())
            .unwrap_or(0)
            .to_string(),
        ("POST", "/keys/test") => v
            .get("message")
            .and_then(|s| s.as_str())
            .unwrap_or("Ошибка проверки")
            .to_string(),
        ("POST", "/conversations/new") => v
            .get("header")
            .and_then(|s| s.as_str())
            .unwrap_or("")
            .to_string(),
        ("POST", "/conversations/load") => {
            let hdr = v.get("header").and_then(|s| s.as_str()).unwrap_or("");
            let count = v
                .get("messages")
                .and_then(|a| a.as_array())
                .map(|a| a.len())
                .unwrap_or(0);
            format!("{}\n{count}\n", hdr.replace('\n', " "))
        }
        ("GET", p) if p.starts_with("/conversations/msg/") => {
            let role = v.get("role").and_then(|x| x.as_i64()).unwrap_or(0);
            let text = v.get("text").and_then(|x| x.as_str()).unwrap_or("");
            let char_len = text.chars().count();
            let imgs_n = v.get("images_count").and_then(|x| x.as_i64()).unwrap_or(0);
            let tl = v
                .get("timeline_words")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            let mut out = format!("{role} {} {char_len} {imgs_n}\n", tl.len());
            if !tl.is_empty() {
                let s: Vec<String> = tl
                    .iter()
                    .filter_map(|w| w.as_i64().map(|n| n.to_string()))
                    .collect();
                out.push_str(&s.join(" "));
            }
            out.push('\n');
            out.push_str(text);
            out
        }
        ("GET", "/settings") => {
            if v.get("found").and_then(|b| b.as_bool()).unwrap_or(false) {
                format!(
                    "1 {} {} {} {}",
                    v.get("theme").and_then(|x| x.as_i64()).unwrap_or(0),
                    v.get("font_scale_pct").and_then(|x| x.as_i64()).unwrap_or(120),
                    v.get("currency_id").and_then(|x| x.as_i64()).unwrap_or(0),
                    v.get("model_idx").and_then(|x| x.as_i64()).unwrap_or(0),
                )
            } else {
                "0".to_string()
            }
        }
        ("PUT", "/settings")
        | ("POST", "/agent/start")
        | ("POST", "/agent/cancel")
        | ("POST", "/apps/play")
        | ("POST", "/apps/play_project") => {
            if v.get("ok").and_then(|b| b.as_bool()).unwrap_or(false) {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        ("POST", "/agent/kill-cmd") => {
            if v.get("killing").and_then(|b| b.as_bool()).unwrap_or(false) {
                "1".to_string()
            } else {
                "0".to_string()
            }
        }
        ("GET", "/agent/poll") => {
            let st = v.get("status").and_then(|x| x.as_i64()).unwrap_or(0);
            if st == 0 {
                return "0".to_string();
            }
            let tl = v
                .get("timeline_words")
                .and_then(|a| a.as_array())
                .cloned()
                .unwrap_or_default();
            let tl_str: Vec<String> = tl
                .iter()
                .filter_map(|w| w.as_i64().map(|n| n.to_string()))
                .collect();
            if st == 1 {
                let el = v.get("elapsed_tenths").and_then(|x| x.as_i64()).unwrap_or(0);
                let kil = v.get("killing").and_then(|x| x.as_i64()).unwrap_or(0);
                let line = v
                    .get("status_line")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .replace('\n', " ");
                format!("1 {el} {kil} {}\n{line}\n{}", tl.len(), tl_str.join(" "))
            } else {
                let el = v.get("elapsed_tenths").and_then(|x| x.as_i64()).unwrap_or(0);
                let cm = v.get("cost_milli_ils").and_then(|x| x.as_i64()).unwrap_or(0);
                let tok = v.get("total_tokens").and_then(|x| x.as_i64()).unwrap_or(0);
                let hdr = v
                    .get("header")
                    .and_then(|s| s.as_str())
                    .unwrap_or("")
                    .replace('\n', " ");
                let reply = v.get("reply").and_then(|s| s.as_str()).unwrap_or("");
                format!(
                    "2 {el} {cm} {tok} {}\n{hdr}\n{}\n{reply}",
                    tl.len(),
                    tl_str.join(" ")
                )
            }
        }
        _ => v.to_string(),
    }
}

pub fn handle_request(
    ctx: &ServerContext,
    method: &str,
    path: &str,
    auth_header: Option<&str>,
    body: &[u8],
) -> (u16, &'static str, Value) {
    if method == "GET" && path == "/health" {
        return (
            200,
            "OK",
            json!({
                "ok": true,
                "service": "infimum-agentd",
                "version": env!("CARGO_PKG_VERSION"),
            }),
        );
    }

    // Enforce constant-time Bearer token check on every non-health endpoint.
    let expected = format!("Bearer {}", ctx.token);
    let authorized = auth_header
        .map(|h| constant_time_eq(h.trim().as_bytes(), expected.as_bytes()))
        .unwrap_or(false);
    if !authorized {
        return (
            401,
            "Unauthorized",
            json!({ "ok": false, "error": "unauthorized" }),
        );
    }

    let body_json: Value = if body.is_empty() {
        Value::Null
    } else if let Ok(v) = serde_json::from_slice::<Value>(body) {
        v
    } else {
        // Plain-text body fallback for compact `?format=ifm` capsule requests
        let text = String::from_utf8_lossy(body);
        match (method, path) {
            ("POST", "/agent/start") => {
                let mut lines = text.splitn(3, '\n');
                let model = lines.next().unwrap_or("").trim();
                let proj = lines.next().unwrap_or("").trim();
                let prompt = lines.next().unwrap_or("");
                json!({ "model": model, "project": proj, "prompt": prompt })
            }
            ("POST", "/conversations/new") => json!({ "project": text.trim() }),
            ("POST", "/conversations/load") | ("POST", "/conversations/delete") => {
                let idx = text.trim().parse::<u64>().unwrap_or(0);
                json!({ "index": idx })
            }
            ("POST", "/keys/save") => {
                let mut lines = text.splitn(2, '\n');
                let idx = lines.next().unwrap_or("0").trim().parse::<u64>().unwrap_or(0);
                let key = lines.next().unwrap_or("").trim();
                json!({ "index": idx, "key": key })
            }
            ("POST", "/keys/test") => {
                let idx = text.trim().parse::<u64>().unwrap_or(0);
                json!({ "index": idx })
            }
            ("POST", "/files/open") => {
                let mut lines = text.splitn(3, '\n');
                let proj = lines.next().unwrap_or("").trim();
                let file = lines.next().unwrap_or("").trim();
                let mut nums = lines.next().unwrap_or("").split_whitespace();
                let start_line = nums.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
                let max_lines = nums.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(250);
                json!({
                    "project": proj,
                    "file": file,
                    "start_line": start_line,
                    "max_lines": max_lines,
                })
            }
            ("POST", "/apps/play_project") => json!({ "project": text.trim() }),
            ("PUT", "/settings") => {
                let nums: Vec<i64> = text
                    .split_whitespace()
                    .filter_map(|s| s.parse::<i64>().ok())
                    .collect();
                json!({
                    "theme": nums.first().copied().unwrap_or(0),
                    "font_scale_pct": nums.get(1).copied().unwrap_or(120),
                    "currency_id": nums.get(2).copied().unwrap_or(0),
                    "model_idx": nums.get(3).copied().unwrap_or(0),
                })
            }
            _ => Value::Null,
        }
    };

    match (method, path) {
        ("POST", "/shutdown") => {
            ctx.shutdown.store(true, Ordering::SeqCst);
            (200, "OK", json!({ "ok": true }))
        }
        ("GET", "/workspaces") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let names: Vec<String> = agent::load_allowed_workspaces(&ctx.policy)
                .into_iter()
                .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                .collect();
            (200, "OK", json!({ "ok": true, "names": names }))
        }
        ("POST", "/workspaces") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let dir_opt = body_json
                .get("path")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(PathBuf::from)
                .or_else(|| {
                    let home = env::var("HOME").ok().map(PathBuf::from);
                    let init = ctx
                        .policy
                        .allowed_fs_roots
                        .first()
                        .cloned()
                        .or(home);
                    pick_directory_dialog(init.as_deref())
                });
            let Some(dir) = dir_opt else {
                return (200, "OK", json!({ "ok": false, "cancelled": true }));
            };
            match agent::add_allowed_workspace(&ctx.policy, &dir) {
                Ok(_) => {
                    let names: Vec<String> = agent::load_allowed_workspaces(&ctx.policy)
                        .into_iter()
                        .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                        .collect();
                    (200, "OK", json!({ "ok": true, "names": names }))
                }
                Err(e) => (400, "Bad Request", json!({ "ok": false, "error": e })),
            }
        }
        ("DELETE", p) if p.starts_with("/workspaces/") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let Ok(idx) = p.trim_start_matches("/workspaces/").parse::<usize>() else {
                return (400, "Bad Request", json!({ "ok": false, "error": "invalid_index" }));
            };
            let list = agent::remove_allowed_workspace(&ctx.policy, idx);
            let names: Vec<String> = list
                .into_iter()
                .filter_map(|p| p.file_name().map(|f| f.to_string_lossy().to_string()))
                .collect();
            (200, "OK", json!({ "ok": true, "names": names }))
        }
        ("GET", "/conversations") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let (paths, labels) = agent::list_saved_conversations();
            if let Ok(mut g) = cached_conv_paths().lock() {
                *g = paths;
            }
            (200, "OK", json!({ "ok": true, "labels": labels }))
        }
        ("GET", "/files") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let mut entries: Vec<String> = Vec::new();
            for ws in resolve_project_workspaces(&ctx.policy) {
                let Some(proj_name) = ws.file_name().map(|f| f.to_string_lossy().to_string()) else {
                    continue;
                };
                for rel in collect_project_files(&ws) {
                    entries.push(format!("{proj_name}\t{rel}"));
                }
            }
            (200, "OK", json!({ "ok": true, "files": entries }))
        }
        ("POST", "/files/open") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let proj = body_json
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            let rel_file = body_json
                .get("file")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim();
            if proj.is_empty() || rel_file.is_empty() {
                return (400, "Bad Request", json!({ "ok": false, "error": "missing_project_or_file" }));
            }
            let Some(ws) = find_workspace_by_name(&ctx.policy, proj) else {
                return (404, "Not Found", json!({ "ok": false, "error": "project_not_found" }));
            };
            let Ok(ws_canon) = fs::canonicalize(&ws) else {
                return (404, "Not Found", json!({ "ok": false, "error": "workspace_not_found" }));
            };
            let candidate = ws_canon.join(rel_file);
            let Ok(file_canon) = fs::canonicalize(&candidate) else {
                return (404, "Not Found", json!({ "ok": false, "error": "file_not_found" }));
            };
            if !file_canon.starts_with(&ws_canon) || !file_canon.is_file() {
                return (403, "Forbidden", json!({ "ok": false, "error": "file_outside_workspace" }));
            }
            let start_line = body_json
                .get("start_line")
                .and_then(|v| v.as_u64())
                .unwrap_or(0) as usize;
            let max_lines = body_json
                .get("max_lines")
                .and_then(|v| v.as_u64())
                .unwrap_or(250)
                .clamp(1, 400) as usize;
            match fs::File::open(&file_canon) {
                Ok(f) => {
                    let mut reader = BufReader::new(f);
                    let mut line_buf = Vec::new();
                    let mut total_lines: usize = 0;
                    let mut win_lines: Vec<String> = Vec::new();
                    let mut win_chars: usize = 0;
                    let end_line = start_line.saturating_add(max_lines);
                    loop {
                        line_buf.clear();
                        match reader.read_until(b'\n', &mut line_buf) {
                            Ok(0) => break,
                            Ok(_) => {
                                if total_lines >= start_line && total_lines < end_line {
                                    let s = String::from_utf8_lossy(&line_buf);
                                    let clean = s.trim_end_matches(['\r', '\n']);
                                    let char_n = clean.chars().count();
                                    if win_chars + char_n + 1 <= 64500 {
                                        win_lines.push(clean.to_string());
                                        win_chars += char_n + 1;
                                    }
                                }
                                total_lines += 1;
                            }
                            Err(e) => {
                                return (
                                    500,
                                    "Internal Server Error",
                                    json!({ "ok": false, "error": format!("read_failed: {e}") }),
                                );
                            }
                        }
                    }
                    let total_out = total_lines.max(1);
                    let win_start = start_line.min(total_out.saturating_sub(1));
                    let win_count = win_lines.len();
                    let content = win_lines.join("\n");
                    (
                        200,
                        "OK",
                        json!({
                            "ok": true,
                            "total_lines": total_out,
                            "win_start": win_start,
                            "win_count": win_count,
                            "content": content,
                        }),
                    )
                }
                Err(e) => (
                    500,
                    "Internal Server Error",
                    json!({ "ok": false, "error": format!("read_failed: {e}") }),
                ),
            }
        }
        ("POST", "/conversations/delete") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let idx = body_json
                .get("index")
                .and_then(|v| v.as_u64())
                .or_else(|| body_json.as_u64())
                .unwrap_or(usize::MAX as u64) as usize;
            let path = cached_conv_paths()
                .lock()
                .ok()
                .and_then(|g| g.get(idx).cloned());
            if let Some(p) = path {
                if p.extension().and_then(|e| e.to_str()) == Some("json")
                    && p.to_string_lossy().contains("conversations")
                {
                    let _ = fs::remove_file(&p);
                }
            }
            let (paths, labels) = agent::list_saved_conversations();
            if let Ok(mut g) = cached_conv_paths().lock() {
                *g = paths;
            }
            (200, "OK", json!({ "ok": true, "labels": labels }))
        }
        ("GET", "/keys/status") => {
            let mask = compute_keys_status_mask();
            (200, "OK", json!({ "ok": true, "mask": mask }))
        }
        ("POST", "/keys/save") => {
            let idx = body_json
                .get("index")
                .and_then(|v| v.as_u64())
                .unwrap_or(99) as usize;
            let key_val = body_json
                .get("key")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            let Some(&key_name) = PROVIDER_ENV_KEYS.get(idx) else {
                return (400, "Bad Request", json!({ "ok": false, "error": "invalid_provider" }));
            };
            match save_env_key_to_dotenv(key_name, key_val) {
                Ok(()) => {
                    let mask = compute_keys_status_mask();
                    (200, "OK", json!({ "ok": true, "mask": mask }))
                }
                Err(e) => (500, "Internal Server Error", json!({ "ok": false, "error": e })),
            }
        }
        ("POST", "/keys/test") => {
            let idx = body_json
                .get("index")
                .and_then(|v| v.as_u64())
                .or_else(|| body_json.as_u64())
                .unwrap_or(99) as usize;
            let msg = test_provider_key_live(idx);
            let ok = msg.starts_with("OK:");
            (200, "OK", json!({ "ok": ok, "message": msg }))
        }
        ("POST", "/conversations/new") => {
            let proj = body_json
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("");
            agent::start_new_conversation(proj);
            let hdr = agent::active_conv_header();
            (200, "OK", json!({ "ok": true, "header": hdr }))
        }
        ("POST", "/conversations/load") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let idx = body_json
                .get("index")
                .and_then(|v| v.as_u64())
                .or_else(|| body_json.as_u64())
                .unwrap_or(0) as usize;
            let path = cached_conv_paths()
                .lock()
                .ok()
                .and_then(|g| g.get(idx).cloned());
            let Some(p) = path else {
                return (404, "Not Found", json!({ "ok": false, "error": "not_found" }));
            };
            let Some(msgs) = agent::load_conversation_from_path(&p) else {
                return (404, "Not Found", json!({ "ok": false, "error": "load_failed" }));
            };
            let mut cached = Vec::with_capacity(msgs.len());
            let mut serialized = Vec::with_capacity(msgs.len());
            for (role, text, tl, imgs) in &msgs {
                let tl_words = tl
                    .as_ref()
                    .map(|s| agent::encode_timeline_words(s, LIVE_TIMELINE_MAX_WORDS as usize))
                    .unwrap_or_default();
                serialized.push(json!({
                    "role": role,
                    "text": text,
                    "timeline_words": tl_words,
                    "images_count": imgs.len(),
                }));
                cached.push(CachedLoadedMsg {
                    role: *role,
                    text: text.clone(),
                    timeline_words: tl_words,
                    images: imgs.clone(),
                });
            }
            if let Ok(mut g) = cached_loaded_msgs().lock() {
                *g = cached;
            }
            let hdr = agent::active_conv_header();
            (
                200,
                "OK",
                json!({ "ok": true, "header": hdr, "messages": serialized }),
            )
        }
        ("GET", p) if p.starts_with("/conversations/msg/") => {
            let Ok(idx) = p.trim_start_matches("/conversations/msg/").parse::<usize>() else {
                return (400, "Bad Request", json!({ "ok": false, "error": "invalid_index" }));
            };
            let msg_opt = cached_loaded_msgs().lock().ok().and_then(|g| {
                g.get(idx).map(|m| {
                    json!({
                        "ok": true,
                        "role": m.role,
                        "text": m.text,
                        "timeline_words": m.timeline_words,
                        "images_count": m.images.len(),
                    })
                })
            });
            match msg_opt {
                Some(v) => (200, "OK", v),
                None => (404, "Not Found", json!({ "ok": false, "error": "not_found" })),
            }
        }
        ("GET", "/settings") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            if let Some(s) = agent::load_native_settings() {
                (
                    200,
                    "OK",
                    json!({
                        "ok": true,
                        "found": true,
                        "theme": s.theme,
                        "font_scale_pct": s.font_scale_pct,
                        "currency_id": s.currency_id,
                        "model_idx": s.model_idx,
                    }),
                )
            } else {
                (200, "OK", json!({ "ok": true, "found": false }))
            }
        }
        ("PUT", "/settings") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let s = agent::NativeSettings {
                theme: body_json.get("theme").and_then(|v| v.as_i64()).unwrap_or(0) as i32,
                font_scale_pct: body_json
                    .get("font_scale_pct")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(120) as i32,
                currency_id: body_json
                    .get("currency_id")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
                model_idx: body_json
                    .get("model_idx")
                    .and_then(|v| v.as_i64())
                    .unwrap_or(0) as i32,
            };
            if agent::save_native_settings(s) {
                (200, "OK", json!({ "ok": true }))
            } else {
                (500, "Internal Server Error", json!({ "ok": false, "error": "io_failed" }))
            }
        }
        ("POST", "/agent/start") => {
            let Some(prompt) = body_json
                .get("prompt")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
            else {
                return (400, "Bad Request", json!({ "ok": false, "error": "missing_prompt" }));
            };
            let model = body_json
                .get("model")
                .and_then(|v| v.as_str())
                .filter(|m| !m.trim().is_empty())
                .unwrap_or("gemini-4-neon")
                .to_string();
            let active_proj = body_json
                .get("project")
                .and_then(|v| v.as_str())
                .filter(|s| !s.trim().is_empty())
                .map(|s| s.to_string());

            let (prov_name, target_host, env_var, signup_url) = resolve_model_provider_meta(&model);
            if !ctx.policy.allowed_net_domains.is_empty()
                && !ctx.policy.allowed_net_domains.contains(target_host)
            {
                if let Ok(mut st) = async_agent_cell().lock() {
                    st.status = 2;
                    st.elapsed_tenths = 0;
                    st.cost_milli_ils = 0;
                    st.total_tokens = 0;
                    st.status_line.clear();
                    st.reply = format!(
                        "Домен `{target_host}` ({prov_name}) не разрешён политикой `infimum-agentd`."
                    );
                }
                return (200, "OK", json!({ "ok": true }));
            }
            let Some(api_key) = load_env_key_host_only(env_var) else {
                if let Ok(mut st) = async_agent_cell().lock() {
                    st.status = 2;
                    st.elapsed_tenths = 0;
                    st.cost_milli_ils = 0;
                    st.total_tokens = 0;
                    st.status_line.clear();
                    st.reply = format!(
                        "Ключ `{env_var}` для **{prov_name}** не найден в `~/.env`.\n\nДобавьте строку в `~/.env`:\n`{env_var}=ваш_ключ`\nПолучить ключ: {signup_url}"
                    );
                }
                return (200, "OK", json!({ "ok": true }));
            };

            if let Ok(mut st) = async_agent_cell().lock() {
                st.status = 1;
                st.started_at = Some(Instant::now());
                st.elapsed_tenths = 0;
                st.cost_milli_ils = 0;
                st.total_tokens = 0;
                st.status_line = format!("Думаю ({model})…");
                st.reply.clear();
                st.timeline = None;
            }
            let policy_clone = ctx.policy.clone();
            std::thread::spawn(move || {
                let res = agent::run_native_agent_loop(
                    &policy_clone,
                    &api_key,
                    &model,
                    &prompt,
                    active_proj.as_deref(),
                    |status_txt| {
                        if let Ok(mut st) = async_agent_cell().lock() {
                            st.status_line = status_txt;
                        }
                    },
                );
                if let Ok(mut st) = async_agent_cell().lock() {
                    st.status = 2;
                    st.elapsed_tenths = res.elapsed_tenths;
                    st.cost_milli_ils = res.cost_milli_ils;
                    st.total_tokens = res.total_tokens;
                    st.status_line.clear();
                    st.reply = res.reply;
                    st.timeline = res.timeline;
                }
            });
            (200, "OK", json!({ "ok": true }))
        }
        ("GET", "/agent/poll") => {
            if let Ok(mut st) = async_agent_cell().lock() {
                if st.status == 1 {
                    let elapsed = st
                        .started_at
                        .map(|s| (s.elapsed().as_millis() / 100) as i64)
                        .unwrap_or(0);
                    let status_line = st.status_line.clone();
                    drop(st);
                    let tl_words =
                        agent::get_live_timeline_words(LIVE_TIMELINE_MAX_WORDS as usize);
                    let killing = if agent::ASYNC_CMD_KILL_REQUESTED.load(Ordering::SeqCst) {
                        1
                    } else {
                        0
                    };
                    return (
                        200,
                        "OK",
                        json!({
                            "ok": true,
                            "status": 1,
                            "elapsed_tenths": elapsed,
                            "status_line": status_line,
                            "killing": killing,
                            "timeline_words": tl_words,
                        }),
                    );
                }
                if st.status == 2 {
                    let reply = std::mem::take(&mut st.reply);
                    let tl_snap = st.timeline.take();
                    let el = st.elapsed_tenths;
                    let cm = st.cost_milli_ils;
                    let tok = st.total_tokens;
                    st.status = 0;
                    drop(st);
                    let tl_words = tl_snap
                        .as_ref()
                        .map(|s| agent::encode_timeline_words(s, LIVE_TIMELINE_MAX_WORDS as usize))
                        .unwrap_or_default();
                    let hdr = agent::active_conv_header();
                    return (
                        200,
                        "OK",
                        json!({
                            "ok": true,
                            "status": 2,
                            "elapsed_tenths": el,
                            "cost_milli_ils": cm,
                            "total_tokens": tok,
                            "reply": reply,
                            "header": hdr,
                            "killing": 0,
                            "timeline_words": tl_words,
                        }),
                    );
                }
            }
            (200, "OK", json!({ "ok": true, "status": 0 }))
        }
        ("POST", "/agent/cancel") => {
            agent::ASYNC_CANCELLED.store(true, Ordering::SeqCst);
            let pid = agent::ASYNC_GEMINI_PID.swap(0, Ordering::SeqCst);
            if pid > 1 {
                #[cfg(unix)]
                agent::signal_pid(pid, agent::SIGKILL_NUM);
            }
            let cpid = agent::ASYNC_CMD_PID.swap(0, Ordering::SeqCst);
            if cpid > 1 {
                #[cfg(unix)]
                {
                    agent::signal_process_group(cpid, agent::SIGKILL_NUM);
                    agent::signal_pid(cpid, agent::SIGKILL_NUM);
                }
            }
            (200, "OK", json!({ "ok": true }))
        }
        ("POST", "/agent/kill-cmd") => {
            let killing = agent::kill_active_shell_command();
            (200, "OK", json!({ "ok": true, "killing": killing }))
        }
        ("POST", "/apps/play") => {
            let body_str = String::from_utf8_lossy(body);
            let mut lines = body_str.lines();
            let ws_raw = body_json
                .get("workspace")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| lines.next().unwrap_or("").trim().to_string());
            let app_raw = body_json
                .get("app")
                .and_then(|v| v.as_str())
                .map(|s| s.to_string())
                .unwrap_or_else(|| lines.next().unwrap_or("").trim().to_string());

            let ws_path = if !ws_raw.is_empty() {
                PathBuf::from(&ws_raw)
            } else {
                ctx.policy
                    .allowed_fs_roots
                    .first()
                    .cloned()
                    .unwrap_or_else(|| PathBuf::from("."))
            };

            if app_raw.is_empty() || !app_raw.ends_with(".ifw") {
                return (400, "Bad Request", json!({ "ok": false, "error": "invalid_ifw_name" }));
            }

            let Ok(ws_canon) = fs::canonicalize(&ws_path) else {
                return (404, "Not Found", json!({ "ok": false, "error": "workspace_not_found" }));
            };
            let candidate = ws_canon.join(&app_raw);
            let Ok(ifw_canon) = fs::canonicalize(&candidate) else {
                return (404, "Not Found", json!({ "ok": false, "error": "ifw_not_found" }));
            };
            if !ifw_canon.starts_with(&ws_canon) || !ifw_canon.is_file() {
                return (403, "Forbidden", json!({ "ok": false, "error": "ifw_outside_workspace" }));
            }

            let web_bin = agent::resolve_infimum_web_bin();
            let launch_dir = ifw_canon.parent().unwrap_or(&ws_canon);
            match std::process::Command::new(&web_bin)
                .arg(&ifw_canon)
                .current_dir(launch_dir)
                .stdin(std::process::Stdio::null())
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .spawn()
            {
                Ok(_) => (200, "OK", json!({ "ok": true, "launched": app_raw })),
                Err(e) => (
                    500,
                    "Internal Server Error",
                    json!({ "ok": false, "error": format!("spawn_failed: {e}") }),
                ),
            }
        }
        ("POST", "/apps/play_project") => {
            if !ctx.policy.fs_enabled() {
                return (403, "Forbidden", json!({ "ok": false, "error": "fs_denied" }));
            }
            let proj = body_json
                .get("project")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .trim()
                .to_string();
            let Some(ws) = find_workspace_by_name(&ctx.policy, &proj) else {
                return (404, "Not Found", json!({ "ok": false, "error": "project_not_found" }));
            };
            let Ok(ws_canon) = fs::canonicalize(&ws) else {
                return (404, "Not Found", json!({ "ok": false, "error": "workspace_not_found" }));
            };
            let existing_ifw = find_existing_ifw_in_project(&ws_canon, &proj);
            let entry_ifm = find_entry_ifm_in_project(&ws_canon);
            if existing_ifw.is_none() && entry_ifm.is_none() {
                return (404, "Not Found", json!({ "ok": false, "error": "no_ifw_or_ifm" }));
            }

            let needs_compile = match (&existing_ifw, &entry_ifm) {
                (None, Some(_)) => true,
                (Some(ifw), Some(_)) => {
                    let ifw_t = fs::metadata(ifw).and_then(|m| m.modified()).ok();
                    let src_t = newest_ifm_mtime(&ws_canon, 0);
                    matches!((ifw_t, src_t), (Some(a), Some(b)) if b > a)
                }
                _ => false,
            };

            let policy_clone = ctx.policy.clone();
            let proj_clone = proj.clone();
            std::thread::spawn(move || {
                let target_ifw = if needs_compile {
                    if let Some(entry) = entry_ifm {
                        let clean_name: String = proj_clone
                            .chars()
                            .map(|c| {
                                if c.is_ascii_alphanumeric() || c == '_' {
                                    c
                                } else {
                                    '_'
                                }
                            })
                            .collect();
                        let out_ifw = existing_ifw
                            .clone()
                            .unwrap_or_else(|| ws_canon.join(format!("{clean_name}.ifw")));
                        let infimum_bin = agent::resolve_infimum_bin(&policy_clone);
                        let status = std::process::Command::new(&infimum_bin)
                            .args([
                                "compile-wasm",
                                &entry.display().to_string(),
                                "-o",
                                &out_ifw.display().to_string(),
                            ])
                            .current_dir(&ws_canon)
                            .stdin(std::process::Stdio::null())
                            .stdout(std::process::Stdio::null())
                            .stderr(std::process::Stdio::null())
                            .status();
                        if status.map(|s| s.success()).unwrap_or(false) && out_ifw.is_file() {
                            Some(out_ifw)
                        } else {
                            existing_ifw
                        }
                    } else {
                        existing_ifw
                    }
                } else {
                    existing_ifw
                };

                if let Some(ifw_path) = target_ifw {
                    if let Ok(ifw_canon) = fs::canonicalize(&ifw_path) {
                        if ifw_canon.starts_with(&ws_canon) && ifw_canon.is_file() {
                            let web_bin = agent::resolve_infimum_web_bin();
                            let launch_dir = ifw_canon.parent().unwrap_or(&ws_canon);
                            let _ = std::process::Command::new(&web_bin)
                                .arg(&ifw_canon)
                                .current_dir(launch_dir)
                                .stdin(std::process::Stdio::null())
                                .stdout(std::process::Stdio::null())
                                .stderr(std::process::Stdio::null())
                                .spawn();
                        }
                    }
                }
            });
            (200, "OK", json!({ "ok": true, "launched": proj }))
        }
        ("POST", "/images") => {
            let Some(data) = body_json
                .get("base64")
                .and_then(|v| v.as_str())
                .filter(|s| !s.is_empty())
            else {
                return (400, "Bad Request", json!({ "ok": false, "error": "missing_base64" }));
            };
            let mime = body_json
                .get("mime")
                .and_then(|v| v.as_str())
                .unwrap_or("image/png")
                .to_string();
            let width = body_json.get("w").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let height = body_json.get("h").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            let added = agent::add_pending_image(agent::AttachedImage {
                mime_type: mime,
                data: data.to_string(),
                width,
                height,
            });
            let count = agent::pending_images_meta().len();
            (
                200,
                "OK",
                json!({ "ok": added.is_some(), "index": added, "count": count }),
            )
        }
        ("DELETE", "/images") => {
            agent::clear_pending_images();
            (200, "OK", json!({ "ok": true, "count": 0 }))
        }
        ("DELETE", p) if p.starts_with("/images/") => {
            let Ok(idx) = p.trim_start_matches("/images/").parse::<usize>() else {
                return (400, "Bad Request", json!({ "ok": false, "error": "invalid_index" }));
            };
            agent::remove_pending_image(idx);
            let count = agent::pending_images_meta().len();
            (200, "OK", json!({ "ok": true, "count": count }))
        }
        ("GET", "/images") => {
            let metas = agent::pending_images_meta();
            let list: Vec<Value> = metas
                .iter()
                .map(|(w, h, mime)| json!({ "w": w, "h": h, "mime": mime }))
                .collect();
            (
                200,
                "OK",
                json!({ "ok": true, "count": list.len(), "images": list }),
            )
        }
        _ => (404, "Not Found", json!({ "ok": false, "error": "not_found" })),
    }
}

fn handle_client(mut stream: TcpStream, ctx: Arc<ServerContext>) {
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(10)));
    let mut reader = BufReader::new(match stream.try_clone() {
        Ok(s) => s,
        Err(_) => return,
    });

    let mut request_line = String::new();
    if reader.read_line(&mut request_line).unwrap_or(0) == 0 {
        return;
    }
    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 {
        return;
    }
    let method = parts[0].to_string();
    let (path_raw, query) = parts[1].split_once('?').unwrap_or((parts[1], ""));
    let path = path_raw.to_string();
    let is_ifm_format = query.split('&').any(|kv| kv == "format=ifm");

    let mut content_length: usize = 0;
    let mut auth_header: Option<String> = None;
    loop {
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            break;
        }
        if let Some((k, v)) = trimmed.split_once(':') {
            if k.eq_ignore_ascii_case("content-length") {
                content_length = v.trim().parse::<usize>().unwrap_or(0).min(16 * 1024 * 1024);
            } else if k.eq_ignore_ascii_case("authorization") {
                auth_header = Some(v.trim().to_string());
            }
        }
    }

    let mut body = vec![0u8; content_length];
    if content_length > 0 && reader.read_exact(&mut body).is_err() {
        return;
    }

    // Binary image route: GET /conversations/img/<msg_idx>/<img_idx>
    if method == "GET" && path.starts_with("/conversations/img/") {
        let expected = format!("Bearer {}", ctx.token);
        let authorized = auth_header
            .as_deref()
            .map(|h| constant_time_eq(h.trim().as_bytes(), expected.as_bytes()))
            .unwrap_or(false);
        if !authorized {
            write_http_response(
                &mut stream,
                401,
                "Unauthorized",
                "application/json; charset=utf-8",
                r#"{"ok":false,"error":"unauthorized"}"#,
            );
            return;
        }
        let rest = path.trim_start_matches("/conversations/img/");
        let mut sp = rest.split('/');
        let msg_idx = sp.next().and_then(|s| s.parse::<usize>().ok());
        let img_idx = sp.next().and_then(|s| s.parse::<usize>().ok());
        if let (Some(mi), Some(ii)) = (msg_idx, img_idx) {
            let found = cached_loaded_msgs().lock().ok().and_then(|g| {
                g.get(mi)
                    .and_then(|m| m.images.get(ii))
                    .and_then(|im| decode_base64_bytes(&im.data).map(|b| (im.mime_type.clone(), b)))
            });
            if let Some((mime, bytes)) = found {
                write_http_bytes(&mut stream, 200, "OK", &mime, &bytes);
                return;
            }
        }
        write_http_response(
            &mut stream,
            404,
            "Not Found",
            "application/json; charset=utf-8",
            r#"{"ok":false,"error":"not_found"}"#,
        );
        return;
    }

    let (status, reason, resp) =
        handle_request(&ctx, &method, &path, auth_header.as_deref(), &body);
    if is_ifm_format && status == 200 {
        let payload = format_ifm_payload(&method, &path, &resp);
        write_http_response(
            &mut stream,
            status,
            reason,
            "text/plain; charset=utf-8",
            &payload,
        );
    } else {
        let payload = resp.to_string();
        write_http_response(
            &mut stream,
            status,
            reason,
            "application/json; charset=utf-8",
            &payload,
        );
    }
}

pub fn serve(listener: TcpListener, ctx: Arc<ServerContext>) {
    let _ = listener.set_nonblocking(true);
    loop {
        if ctx.shutdown.load(Ordering::SeqCst) {
            break;
        }
        match listener.accept() {
            Ok((stream, _)) => {
                let c = Arc::clone(&ctx);
                std::thread::spawn(move || handle_client(stream, c));
            }
            Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            Err(_) => break,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_ctx() -> ServerContext {
        let mut policy = CapabilityPolicy::default();
        policy.allow_user_workspaces = true;
        ServerContext {
            policy,
            token: "secret-test-token-123".to_string(),
            shutdown: AtomicBool::new(false),
        }
    }

    #[test]
    fn health_is_public_and_other_routes_require_bearer_token() {
        let ctx = test_ctx();
        let (code, _, body) = handle_request(&ctx, "GET", "/health", None, &[]);
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
        assert_eq!(body["service"], "infimum-agentd");

        // Missing token -> 401
        let (code, _, _) = handle_request(&ctx, "GET", "/workspaces", None, &[]);
        assert_eq!(code, 401);

        // Wrong token -> 401
        let (code, _, _) =
            handle_request(&ctx, "GET", "/workspaces", Some("Bearer wrong-token"), &[]);
        assert_eq!(code, 401);

        // Valid token -> 200
        let (code, _, body) = handle_request(
            &ctx,
            "GET",
            "/workspaces",
            Some("Bearer secret-test-token-123"),
            &[],
        );
        assert_eq!(code, 200);
        assert_eq!(body["ok"], true);
    }

    #[test]
    fn agent_poll_idle_and_shutdown_flag() {
        let ctx = test_ctx();
        let auth = Some("Bearer secret-test-token-123");

        let (code, _, body) = handle_request(&ctx, "GET", "/agent/poll", auth, &[]);
        assert_eq!(code, 200);
        assert_eq!(body["status"], 0);

        let (code, _, _) = handle_request(&ctx, "POST", "/shutdown", auth, &[]);
        assert_eq!(code, 200);
        assert!(ctx.shutdown.load(Ordering::SeqCst));
    }

    #[test]
    fn files_list_open_and_play_project_validation() {
        let tmp = std::env::temp_dir().join(format!(
            "infimum_code_http_test_{}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&tmp);
        fs::create_dir_all(tmp.join("src")).unwrap();
        fs::write(tmp.join("main.ifm"), "fn main() -> i64 { 42 }\n").unwrap();
        fs::write(tmp.join("src/helper.ifm"), "fn helper() -> i64 { 1 }\n").unwrap();
        fs::write(tmp.join("textures.json"), "{}\n").unwrap();

        let files = collect_project_files(&tmp);
        assert_eq!(files[0], "main.ifm");
        assert!(files.contains(&"src/helper.ifm".to_string()));
        assert!(files.contains(&"textures.json".to_string()));

        let mut policy = CapabilityPolicy::default();
        policy.allow_user_workspaces = false;
        policy.allowed_fs_roots = vec![tmp.clone()];
        let ctx = ServerContext {
            policy,
            token: "secret-test-token-123".to_string(),
            shutdown: AtomicBool::new(false),
        };
        let auth = Some("Bearer secret-test-token-123");
        let proj_name = tmp.file_name().unwrap().to_string_lossy().to_string();

        // GET /files returns project\tfile entries
        let (code, _, body) = handle_request(&ctx, "GET", "/files", auth, &[]);
        assert_eq!(code, 200);
        let list = body["files"].as_array().unwrap();
        assert!(list.iter().any(|v| v.as_str() == Some(&format!("{proj_name}\tmain.ifm"))));

        // POST /files/open (plain-text ?format=ifm body) reads paged file window
        let req_body = format!("{proj_name}\nmain.ifm\n0 250");
        let (code, _, body) =
            handle_request(&ctx, "POST", "/files/open", auth, req_body.as_bytes());
        assert_eq!(code, 200);
        assert_eq!(body["total_lines"], 1);
        assert_eq!(body["win_start"], 0);
        assert_eq!(body["win_count"], 1);
        assert_eq!(body["content"], "fn main() -> i64 { 42 }");

        // Path traversal is rejected
        let bad_body = format!("{proj_name}\n../etc/passwd");
        let (code, _, _) =
            handle_request(&ctx, "POST", "/files/open", auth, bad_body.as_bytes());
        assert!(code == 403 || code == 404);

        // POST /apps/play_project with unknown project -> 404
        let (code, _, _) =
            handle_request(&ctx, "POST", "/apps/play_project", auth, b"nonexistent_proj");
        assert_eq!(code, 404);

        let _ = fs::remove_dir_all(&tmp);
    }
}
