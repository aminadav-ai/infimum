use crate::policy::CapabilityPolicy;
use serde_json::{json, Value};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

use crate::agent::conversations::{active_conv_cell, conversations_dir, ActiveConversation};

pub(in crate::agent) fn is_safe_path(candidate: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let check_target = if candidate.exists() {
        fs::canonicalize(candidate).ok()
    } else {
        // Walk up to the nearest existing ancestor so creating files inside brand-new
        // nested subdirectories succeeds while still rejecting `..` traversal and symlink escapes.
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
    fname == ".env"
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
}

pub(in crate::agent) fn is_jailed_in(p: &Path, default_proj: &Path) -> bool {
    let jail_roots = [default_proj.to_path_buf()];
    is_safe_path(p, &jail_roots) && !is_sensitive_path(p)
}

pub(in crate::agent) fn resolve_in_project(default_proj: &Path, raw: &str) -> PathBuf {
    let p = PathBuf::from(raw);
    if p.is_absolute() {
        p
    } else {
        default_proj.join(p)
    }
}

pub(in crate::agent) fn workspaces_config_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/infimum-code/workspaces.json")
}

/// Prevents accidentally adding system roots, `$HOME` itself, or hidden `$HOME/.*` secret directories as a workspace.
pub(in crate::agent) fn is_forbidden_workspace_root(canon: &Path) -> bool {
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
        "/" | "/home"
            | "/root"
            | "/etc"
            | "/usr"
            | "/bin"
            | "/sbin"
            | "/lib"
            | "/lib64"
            | "/var"
            | "/boot"
            | "/opt"
            | "/srv"
            | "/proc"
            | "/sys"
            | "/dev"
    )
}

/// Load user-approved workspace folders (strict manual whitelist).
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

pub(in crate::agent) fn resolve_sandboxed_subdir(
    default_proj: &Path,
    sub: Option<&str>,
) -> Result<PathBuf, String> {
    let base = fs::canonicalize(default_proj).unwrap_or_else(|_| default_proj.to_path_buf());
    let Some(s) = sub
        .map(|x| x.trim())
        .filter(|x| !x.is_empty() && *x != ".")
    else {
        return Ok(base);
    };
    let p = Path::new(s);
    if p.is_absolute() {
        let c = fs::canonicalize(p).map_err(|e| format!("Invalid directory '{}': {e}", s))?;
        if !c.starts_with(&base) {
            return Err(format!("Path '{}' escapes active project directory", s));
        }
        return Ok(c);
    }
    let joined = base.join(p);
    let c = fs::canonicalize(&joined).map_err(|e| format!("Invalid directory '{}': {e}", s))?;
    if !c.starts_with(&base) {
        return Err(format!("Path '{}' escapes active project directory", s));
    }
    if !c.is_dir() {
        return Err(format!("'{}' is not a directory", s));
    }
    Ok(c)
}

pub(in crate::agent) fn glob_matches(pattern: &str, text: &str) -> bool {
    let pat = pattern.trim();
    if pat.is_empty() || pat == "*" || pat == "**" || pat == "**/*" {
        return true;
    }
    if !pat.contains('/') {
        let base = text.rsplit('/').next().unwrap_or(text);
        if glob_match_inner(pat.as_bytes(), base.as_bytes()) {
            return true;
        }
    }
    glob_match_inner(pat.as_bytes(), text.as_bytes())
}

fn glob_match_inner(p: &[u8], s: &[u8]) -> bool {
    let (mut pi, mut si) = (0usize, 0usize);
    let mut star_pi: Option<usize> = None;
    let mut star_si = 0usize;
    while si < s.len() {
        if pi < p.len() && p[pi] == b'*' {
            while pi < p.len() && p[pi] == b'*' {
                pi += 1;
            }
            if pi < p.len() && p[pi] == b'/' && pi >= 2 && glob_match_inner(&p[pi + 1..], &s[si..])
            {
                return true;
            }
            star_pi = Some(pi);
            star_si = si;
        } else if pi < p.len() && (p[pi] == b'?' || p[pi] == s[si]) {
            pi += 1;
            si += 1;
        } else if let Some(sp) = star_pi {
            pi = sp;
            star_si += 1;
            si = star_si;
        } else {
            return false;
        }
    }
    while pi < p.len() && p[pi] == b'*' {
        pi += 1;
    }
    pi == p.len()
}

#[allow(clippy::too_many_arguments)]
pub(in crate::agent) fn collect_files_filtered(
    root: &Path,
    dir: &Path,
    out: &mut Vec<String>,
    total_found: &mut usize,
    depth: usize,
    max_depth: usize,
    limit: usize,
    pattern: Option<&str>,
) {
    if depth > max_depth {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.file_name());
    for ent in ents {
        let name = ent.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git"
                | "target"
                | "node_modules"
                | ".infimum"
                | "logs"
                | "dist"
                | ".wrangler"
                | "__pycache__"
                | ".cache"
        ) {
            continue;
        }
        let p = ent.path();
        if is_sensitive_path(&p) {
            continue;
        }
        if p.is_dir() {
            collect_files_filtered(
                root,
                &p,
                out,
                total_found,
                depth + 1,
                max_depth,
                limit,
                pattern,
            );
        } else if let Ok(rel) = p.strip_prefix(root) {
            let rel_str = rel.display().to_string();
            let matches_pat = pattern.map(|pat| glob_matches(pat, &rel_str)).unwrap_or(true);
            if matches_pat {
                *total_found += 1;
                if out.len() < limit {
                    out.push(rel_str);
                }
            }
        }
    }
}

/// Smarter fallback-mode command guard: inspects command tokens rather than naive substrings,
/// so `which rg curl` or `rg "curl"` is allowed while invoking `curl`, `wget`, `ssh`, `nc`
/// directly or escaping the workspace via `../` or `/etc/` is blocked.
pub(in crate::agent) fn is_unsafe_fallback_command(cmd: &str) -> bool {
    let lower = cmd.to_ascii_lowercase();
    if lower.contains("/etc/") || lower.contains("find /") {
        return true;
    }
    for tok in lower.split_whitespace() {
        let t = tok.trim_matches(|c| c == '"' || c == '\'' || c == ';');
        if t == ".." || t.starts_with("../") || t.contains("/../") || t.ends_with("/..") {
            return true;
        }
    }
    let normalized = lower
        .replace("&&", "\n")
        .replace("||", "\n")
        .replace(';', "\n")
        .replace('|', "\n")
        .replace("$(", "\n")
        .replace('`', "\n");
    for seg in normalized.lines() {
        let mut iter = seg.split_whitespace().peekable();
        while let Some(&w) = iter.peek() {
            if w.contains('=') && !w.starts_with('-') {
                iter.next();
            } else if matches!(w, "env" | "time" | "command" | "nohup" | "nice") {
                iter.next();
            } else {
                break;
            }
        }
        if let Some(bin) = iter.next() {
            let base = bin.rsplit('/').next().unwrap_or(bin);
            if matches!(
                base,
                "sudo"
                    | "su"
                    | "doas"
                    | "curl"
                    | "wget"
                    | "ssh"
                    | "scp"
                    | "sftp"
                    | "nc"
                    | "ncat"
                    | "netcat"
                    | "socat"
                    | "telnet"
            ) {
                return true;
            }
            if base == "rm" {
                let args: Vec<&str> = iter.collect();
                let destructive_target = args.iter().any(|a| {
                    let t = a.trim_matches(|c| c == '"' || c == '\'');
                    matches!(t, "/" | "/*" | "~" | "~/" | "$home" | "${home}")
                });
                if destructive_target {
                    return true;
                }
            }
        }
    }
    false
}
