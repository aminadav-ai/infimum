//! Developer Mode & structured dev tools for `infimum-agentd`:
//! - `git_tool`: structured, safe git operations (local always; network ops in Dev Mode)
//! - `gh_cli`: GitHub CLI wrapper (Dev Mode)
//! - `wrangler_cli`: Cloudflare Wrangler CLI wrapper (Dev Mode)
//! - `web_fetch`: SSRF-hardened HTTP(S) fetcher with HTML->compact-text conversion (Dev Mode)
//! - `web_search`: Gemini Search grounding (`google_search` tool via `gemini-3.8-flash` with fallback, Dev Mode)
//! - `run_subprocess_managed`: timeout + live-tail + instant cancellation helper shared with shell runner

use std::env;
use std::io::Read;
use std::net::{IpAddr, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::timeline::{
    signal_process_group, timeline_update_last_step_tail, ASYNC_CANCELLED,
    ASYNC_CMD_KILL_REQUESTED, ASYNC_CMD_PID, SIGKILL_NUM, SIGTERM_NUM,
};
use super::tools::resolve_sandboxed_subdir;

/// Build a clean, reliable PATH that always includes `~/.cargo/bin` and `~/.local/bin`
/// plus standard Unix bins, preserving any extra user tool directories (e.g. nvm/node).
pub(super) fn build_clean_path() -> String {
    let home = env::var("HOME").unwrap_or_default();
    let mut parts: Vec<String> = Vec::new();
    if !home.is_empty() {
        parts.push(format!("{home}/.cargo/bin"));
        parts.push(format!("{home}/.local/bin"));
    }
    if let Ok(existing) = env::var("PATH") {
        for p in existing.split(':') {
            if !p.is_empty() && !parts.iter().any(|x| x == p) {
                parts.push(p.to_string());
            }
        }
    }
    for std_dir in ["/usr/local/bin", "/usr/bin", "/bin"] {
        if !parts.iter().any(|x| x == std_dir) {
            parts.push(std_dir.to_string());
        }
    }
    parts.join(":")
}

/// UTF-8-safe head+tail truncation so compiler errors at the end of output are never lost.
pub(super) fn truncate_head_tail(s: &str, max_bytes: usize) -> (String, bool) {
    if s.len() <= max_bytes {
        return (s.to_string(), false);
    }
    if max_bytes < 128 {
        let mut end = max_bytes.min(s.len());
        while end > 0 && !s.is_char_boundary(end) {
            end -= 1;
        }
        return (s[..end].to_string(), true);
    }
    let head_budget = (max_bytes * 38) / 100;
    let tail_budget = max_bytes.saturating_sub(head_budget).saturating_sub(64);
    let mut head_end = head_budget.min(s.len());
    while head_end > 0 && !s.is_char_boundary(head_end) {
        head_end -= 1;
    }
    // Prefer snapping head to a newline boundary if close
    if let Some(nl) = s[..head_end].rfind('\n') {
        if nl > head_end / 2 {
            head_end = nl;
        }
    }
    let mut tail_start = s.len().saturating_sub(tail_budget);
    while tail_start < s.len() && !s.is_char_boundary(tail_start) {
        tail_start += 1;
    }
    if let Some(nl) = s[tail_start..].find('\n') {
        if nl < tail_budget / 3 {
            tail_start += nl + 1;
        }
    }
    let omitted = tail_start.saturating_sub(head_end);
    let out = format!(
        "{}\n... [omitted {} bytes] ...\n{}",
        &s[..head_end],
        omitted,
        &s[tail_start..]
    );
    (out, true)
}

/// Execute a subprocess with timeout, live tail streaming, and instant kill/cancel support.
/// Works identically with or without `bwrap`.
pub(super) fn run_subprocess_managed(
    mut cmd: Command,
    timeout_secs: u64,
    live_prefix: &str,
) -> Result<(i32, String, bool), String> {
    ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);
    cmd.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        cmd.process_group(0);
    }

    let mut child = cmd
        .spawn()
        .map_err(|e| format!("Failed to spawn process: {e}"))?;
    let pid = child.id();
    ASYNC_CMD_PID.store(pid, Ordering::SeqCst);

    let out_buf = Arc::new(Mutex::new(Vec::<u8>::new()));
    let prefix_owned = live_prefix.to_string();

    let spawn_reader = |mut pipe: Box<dyn Read + Send>,
                        buf: Arc<Mutex<Vec<u8>>>,
                        prefix: String| {
        std::thread::spawn(move || {
            let mut chunk = [0u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        let tail_snap = {
                            let mut g = buf.lock().unwrap();
                            // Cap raw memory at 512KB per command while keeping the tail fresh
                            if g.len() + n > 512 * 1024 {
                                let keep = 256 * 1024;
                                if g.len() > keep {
                                    let drain_to = g.len() - keep;
                                    g.drain(..drain_to);
                                }
                            }
                            g.extend_from_slice(&chunk[..n]);
                            let start = g.len().saturating_sub(1400);
                            String::from_utf8_lossy(&g[start..]).into_owned()
                        };
                        if !prefix.is_empty() {
                            timeline_update_last_step_tail(format!("{prefix}\n{tail_snap}"));
                        } else {
                            timeline_update_last_step_tail(tail_snap);
                        }
                    }
                }
            }
        })
    };

    let t_out = child
        .stdout
        .take()
        .map(|p| spawn_reader(Box::new(p), Arc::clone(&out_buf), prefix_owned.clone()));
    let t_err = child
        .stderr
        .take()
        .map(|p| spawn_reader(Box::new(p), Arc::clone(&out_buf), prefix_owned));

    let started = Instant::now();
    let deadline = Duration::from_secs(timeout_secs.clamp(5, 300));
    let mut timed_out = false;
    let mut killed_by_user = false;

    let exit_code = loop {
        match child.try_wait() {
            Ok(Some(status)) => {
                if ASYNC_CANCELLED.load(Ordering::SeqCst)
                    || ASYNC_CMD_KILL_REQUESTED.load(Ordering::SeqCst)
                {
                    killed_by_user = true;
                    break -9;
                }
                break status.code().unwrap_or(-1);
            }
            Ok(None) => {
                if ASYNC_CANCELLED.load(Ordering::SeqCst)
                    || ASYNC_CMD_KILL_REQUESTED.load(Ordering::SeqCst)
                {
                    killed_by_user = true;
                    signal_process_group(pid, SIGTERM_NUM);
                    std::thread::sleep(Duration::from_millis(60));
                    signal_process_group(pid, SIGKILL_NUM);
                    let _ = child.kill();
                    let _ = child.wait();
                    break -9;
                }
                if started.elapsed() >= deadline {
                    timed_out = true;
                    signal_process_group(pid, SIGTERM_NUM);
                    std::thread::sleep(Duration::from_millis(60));
                    signal_process_group(pid, SIGKILL_NUM);
                    let _ = child.kill();
                    let _ = child.wait();
                    break -124;
                }
                std::thread::sleep(Duration::from_millis(25));
            }
            Err(e) => {
                signal_process_group(pid, SIGKILL_NUM);
                let _ = child.kill();
                ASYNC_CMD_PID.store(0, Ordering::SeqCst);
                return Err(format!("Process wait error: {e}"));
            }
        }
    };

    if let Some(h) = t_out {
        let _ = h.join();
    }
    if let Some(h) = t_err {
        let _ = h.join();
    }
    ASYNC_CMD_PID.store(0, Ordering::SeqCst);

    let mut text = String::from_utf8_lossy(&out_buf.lock().unwrap()).into_owned();
    if killed_by_user {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str("[terminated by user]");
    } else if timed_out {
        if !text.ends_with('\n') && !text.is_empty() {
            text.push('\n');
        }
        text.push_str(&format!("[timed out after {}s]", deadline.as_secs()));
    }
    Ok((exit_code, text, timed_out))
}

fn parse_string_args(args: &Value) -> Vec<String> {
    args.get("args")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default()
}

fn find_executable(candidates: &[&str]) -> Option<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    for name in candidates {
        let p = Path::new(name);
        if p.is_absolute() && p.is_file() {
            return Some(p.to_path_buf());
        }
        if !home.is_empty() {
            let local = PathBuf::from(&home).join(".local/bin").join(name);
            if local.is_file() {
                return Some(local);
            }
            let cargo = PathBuf::from(&home).join(".cargo/bin").join(name);
            if cargo.is_file() {
                return Some(cargo);
            }
        }
        for dir in build_clean_path().split(':') {
            if dir.is_empty() {
                continue;
            }
            let cand = Path::new(dir).join(name);
            if cand.is_file() {
                return Some(cand);
            }
        }
    }
    None
}

// =============================================================================
// 1. Structured `git_tool`
// =============================================================================

pub(super) fn tool_git(args: &Value, default_proj: &Path, dev_mode: bool) -> Value {
    let sub = args
        .get("subcommand")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if sub.is_empty() {
        return json!({ "error": "Missing required 'subcommand' parameter" });
    }

    const LOCAL_SUBS: &[&str] = &[
        "status", "diff", "log", "branch", "show", "add", "commit", "stash",
        "checkout", "switch", "restore", "tag", "rev-parse", "ls-files",
        "blame", "shortlog", "describe", "rev-list", "merge-base",
    ];
    const NET_SUBS: &[&str] = &["fetch", "pull", "push", "remote", "ls-remote"];

    let is_local = LOCAL_SUBS.contains(&sub);
    let is_net = NET_SUBS.contains(&sub);
    if !is_local && !is_net {
        return json!({
            "error": format!(
                "Disallowed git subcommand '{}'. Allowed local: {:?}; Dev Mode network: {:?}",
                sub, LOCAL_SUBS, NET_SUBS
            )
        });
    }
    if is_net && !dev_mode {
        return json!({
            "error": format!(
                "git '{}' requires Developer Mode (enable Developer Mode in Settings or launch with --dev-mode)",
                sub
            )
        });
    }

    let extra_args = parse_string_args(args);

    // Block destructive force-push and dangerous config overrides
    if sub == "push" {
        let has_force = extra_args.iter().any(|a| {
            a == "-f"
                || a == "--force"
                || a.starts_with("--force-")
                || a.starts_with('+')
        });
        if has_force {
            return json!({
                "error": "Force-push (-f / --force) is blocked in git_tool for safety. Run it explicitly if truly intended."
            });
        }
    }
    if extra_args.iter().any(|a| a == "-c" || a.starts_with("--git-dir") || a.starts_with("--work-tree")) {
        return json!({
            "error": "Overriding git-dir/work-tree/config flags is not allowed in git_tool"
        });
    }

    let workdir = match resolve_sandboxed_subdir(default_proj, args.get("cwd").and_then(|v| v.as_str())) {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };

    let Some(git_bin) = find_executable(&["git", "/usr/bin/git"]) else {
        return json!({ "error": "git binary not found on host" });
    };

    let mut cmd = Command::new(git_bin);
    cmd.current_dir(&workdir)
        .env("PATH", build_clean_path())
        .env("GIT_TERMINAL_PROMPT", "0")
        .env("GIT_PAGER", "cat")
        .env("LC_ALL", "C.UTF-8")
        .arg(sub)
        .args(&extra_args);

    let prefix = format!("$ git {} {}", sub, extra_args.join(" "));
    let timeout = if is_net { 90 } else { 45 };
    match run_subprocess_managed(cmd, timeout, &prefix) {
        Ok((code, raw, timed_out)) => {
            let (out, truncated) = truncate_head_tail(&raw, 7000);
            json!({
                "ok": code == 0 && !timed_out,
                "exit_code": code,
                "timed_out": timed_out,
                "truncated": truncated,
                "output": out,
            })
        }
        Err(e) => json!({ "error": e }),
    }
}

// =============================================================================
// 2. GitHub CLI (`gh_cli`) — Developer Mode
// =============================================================================

pub(super) fn tool_gh_cli(args: &Value, default_proj: &Path, dev_mode: bool) -> Value {
    if !dev_mode {
        return json!({
            "error": "gh_cli requires Developer Mode (enable Developer Mode in Settings or launch with --dev-mode)"
        });
    }
    let gh_args = parse_string_args(args);
    if gh_args.is_empty() {
        return json!({ "error": "Missing required 'args' array for gh_cli" });
    }
    let workdir = match resolve_sandboxed_subdir(default_proj, args.get("cwd").and_then(|v| v.as_str())) {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };
    let Some(gh_bin) = find_executable(&["gh"]) else {
        return json!({ "error": "gh CLI not found in PATH or ~/.local/bin" });
    };

    let mut cmd = Command::new(gh_bin);
    cmd.current_dir(&workdir)
        .env("PATH", build_clean_path())
        .env("GH_PROMPT_DISABLED", "1")
        .env("NO_COLOR", "1")
        .env("PAGER", "cat")
        .args(&gh_args);

    let prefix = format!("$ gh {}", gh_args.join(" "));
    match run_subprocess_managed(cmd, 60, &prefix) {
        Ok((code, raw, timed_out)) => {
            let (out, truncated) = truncate_head_tail(&raw, 8000);
            json!({
                "ok": code == 0 && !timed_out,
                "exit_code": code,
                "timed_out": timed_out,
                "truncated": truncated,
                "output": out,
            })
        }
        Err(e) => json!({ "error": e }),
    }
}

// =============================================================================
// 3. Cloudflare Wrangler CLI (`wrangler_cli`) — Developer Mode
// =============================================================================

pub(super) fn tool_wrangler_cli(args: &Value, default_proj: &Path, dev_mode: bool) -> Value {
    if !dev_mode {
        return json!({
            "error": "wrangler_cli requires Developer Mode (enable Developer Mode in Settings or launch with --dev-mode)"
        });
    }
    let w_args = parse_string_args(args);
    if w_args.is_empty() {
        return json!({ "error": "Missing required 'args' array for wrangler_cli" });
    }
    let workdir = match resolve_sandboxed_subdir(default_proj, args.get("cwd").and_then(|v| v.as_str())) {
        Ok(p) => p,
        Err(e) => return json!({ "error": e }),
    };

    let local_bin = workdir.join("node_modules/.bin/wrangler");
    let mut cmd = if local_bin.is_file() {
        let mut c = Command::new(local_bin);
        c.args(&w_args);
        c
    } else if let Some(global_bin) = find_executable(&["wrangler"]) {
        let mut c = Command::new(global_bin);
        c.args(&w_args);
        c
    } else if let Some(npx_bin) = find_executable(&["npx"]) {
        let mut c = Command::new(npx_bin);
        c.arg("--no-install").arg("wrangler").args(&w_args);
        c
    } else {
        return json!({ "error": "Neither wrangler nor npx was found on host" });
    };

    cmd.current_dir(&workdir)
        .env("PATH", build_clean_path())
        .env("CI", "1")
        .env("NO_COLOR", "1")
        .env("WRANGLER_SEND_METRICS", "false");

    let prefix = format!("$ wrangler {}", w_args.join(" "));
    match run_subprocess_managed(cmd, 120, &prefix) {
        Ok((code, raw, timed_out)) => {
            let (out, truncated) = truncate_head_tail(&raw, 8000);
            json!({
                "ok": code == 0 && !timed_out,
                "exit_code": code,
                "timed_out": timed_out,
                "truncated": truncated,
                "output": out,
            })
        }
        Err(e) => json!({ "error": e }),
    }
}

// =============================================================================
// 4. SSRF-hardened `web_fetch` — Developer Mode
// =============================================================================

pub(super) fn is_private_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private()
                || v4.is_link_local()
                || v4.is_broadcast()
                || v4.is_unspecified()
                || v4.is_multicast()
                || (o[0] == 100 && (o[1] & 0b1100_0000) == 64) // CGNAT 100.64.0.0/10
        }
        IpAddr::V6(v6) => {
            let seg0 = v6.segments()[0];
            v6.is_loopback()
                || v6.is_unspecified()
                || v6.is_multicast()
                || (seg0 & 0xfe00) == 0xfc00 // ULA fc00::/7
                || (seg0 & 0xffc0) == 0xfe80 // link-local fe80::/10
        }
    }
}

pub(super) fn validate_web_url(url: &str) -> Result<(String, u16), String> {
    let trimmed = url.trim();
    if trimmed.is_empty() || trimmed.len() > 2048 {
        return Err("Invalid URL length".to_string());
    }
    if trimmed.bytes().any(|b| b < 0x20 || b == 0x7f) {
        return Err("URL contains control characters".to_string());
    }
    let (default_port, rest) = if let Some(r) = trimmed.strip_prefix("https://") {
        (443u16, r)
    } else if let Some(r) = trimmed.strip_prefix("http://") {
        (80u16, r)
    } else {
        return Err("Only http:// and https:// URLs are allowed".to_string());
    };

    let authority = rest.split(['/', '?', '#']).next().unwrap_or("");
    if authority.is_empty() || authority.contains('@') {
        return Err("URL authority is missing or contains credentials".to_string());
    }

    let (host, port) = if let Some(bracket_end) = authority.strip_prefix('[').and_then(|s| s.find(']')) {
        let h = &authority[1..1 + bracket_end];
        let after = &authority[2 + bracket_end..];
        let p = after
            .strip_prefix(':')
            .and_then(|ps| ps.parse::<u16>().ok())
            .unwrap_or(default_port);
        (h.to_string(), p)
    } else if let Some((h, ps)) = authority.rsplit_once(':') {
        let p = ps.parse::<u16>().unwrap_or(default_port);
        (h.to_string(), p)
    } else {
        (authority.to_string(), default_port)
    };

    let lower = host.to_ascii_lowercase();
    if lower.is_empty()
        || lower == "localhost"
        || lower.ends_with(".localhost")
        || lower.ends_with(".local")
        || lower.ends_with(".internal")
        || lower == "metadata.google.internal"
    {
        return Err(format!("Blocked local/internal host '{host}' (SSRF protection)"));
    }

    if let Ok(ip) = host.parse::<IpAddr>() {
        if is_private_ip(ip) {
            return Err(format!("Blocked private/loopback IP '{ip}' (SSRF protection)"));
        }
    }

    Ok((host, port))
}

/// Convert HTML into clean, token-efficient plain text (strips script/style/svg/nav/tags).
pub(super) fn html_to_compact_text(html: &str) -> String {
    // 1. Drop <script>, <style>, <noscript>, <svg>, <head> blocks (case-insensitive)
    let lower = html.to_ascii_lowercase();
    let bytes = html.as_bytes();
    let mut cleaned = String::with_capacity(html.len().min(65536));
    let mut i = 0usize;
    let skip_tags = ["script", "style", "noscript", "svg", "head", "nav"];

    while i < bytes.len() {
        if bytes[i] == b'<' {
            // HTML comment <!-- ... -->
            if lower[i..].starts_with("<!--") {
                if let Some(end) = lower[i + 4..].find("-->") {
                    i += 4 + end + 3;
                    continue;
                } else {
                    break;
                }
            }
            // Block skip tags
            let mut skipped = false;
            for tag in skip_tags {
                let open = format!("<{tag}");
                if lower[i..].starts_with(&open) {
                    let next_ch = lower.as_bytes().get(i + open.len()).copied().unwrap_or(b' ');
                    if next_ch == b'>' || next_ch.is_ascii_whitespace() || next_ch == b'/' {
                        let close = format!("</{tag}>");
                        if let Some(end) = lower[i + open.len()..].find(&close) {
                            i += open.len() + end + close.len();
                        } else {
                            i = bytes.len();
                        }
                        skipped = true;
                        break;
                    }
                }
            }
            if skipped {
                continue;
            }
            // Any other tag: replace block-level tags with newline, others with space
            if let Some(gt) = html[i..].find('>') {
                let tag_inner = lower[i + 1..i + gt].trim_start_matches('/');
                let tag_name = tag_inner
                    .split(|c: char| c.is_ascii_whitespace() || c == '/')
                    .next()
                    .unwrap_or("");
                if matches!(
                    tag_name,
                    "p" | "div" | "br" | "li" | "tr" | "h1" | "h2" | "h3" | "h4" | "h5" | "h6"
                        | "section" | "article" | "header" | "footer" | "pre" | "blockquote" | "hr"
                ) {
                    cleaned.push('\n');
                } else {
                    cleaned.push(' ');
                }
                i += gt + 1;
                continue;
            }
        }
        // Copy next UTF-8 char
        let ch = html[i..].chars().next().unwrap();
        cleaned.push(ch);
        i += ch.len_utf8();
    }

    // 2. Decode common HTML entities
    let decoded = cleaned
        .replace("&nbsp;", " ")
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&#39;", "'")
        .replace("&apos;", "'")
        .replace("&mdash;", "—")
        .replace("&ndash;", "–");

    // 3. Collapse whitespace and blank lines
    let mut out = String::with_capacity(decoded.len().min(32768));
    let mut prev_blank = true;
    for line in decoded.lines() {
        let collapsed: String = line.split_whitespace().collect::<Vec<_>>().join(" ");
        if collapsed.is_empty() {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
        } else {
            out.push_str(&collapsed);
            out.push('\n');
            prev_blank = false;
        }
    }
    out.trim().to_string()
}

pub(super) fn tool_web_fetch(args: &Value, dev_mode: bool) -> Value {
    if !dev_mode {
        return json!({
            "error": "web_fetch requires Developer Mode (enable Developer Mode in Settings or launch with --dev-mode)"
        });
    }
    let url = args
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    let max_chars = args
        .get("max_chars")
        .and_then(|v| v.as_u64())
        .unwrap_or(6000)
        .clamp(500, 20000) as usize;
    let mode = args
        .get("mode")
        .and_then(|v| v.as_str())
        .unwrap_or("text")
        .trim();

    let (host, port) = match validate_web_url(url) {
        Ok(hp) => hp,
        Err(e) => return json!({ "error": e }),
    };

    // Resolve DNS and verify no target IP is private/loopback (anti-DNS-rebinding SSRF check)
    match (host.as_str(), port).to_socket_addrs() {
        Ok(addrs) => {
            let mut any = false;
            for addr in addrs {
                any = true;
                if is_private_ip(addr.ip()) {
                    return json!({
                        "error": format!("Host '{}' resolved to private IP '{}' (SSRF blocked)", host, addr.ip())
                    });
                }
            }
            if !any {
                return json!({ "error": format!("Could not resolve host '{}'", host) });
            }
        }
        Err(e) => return json!({ "error": format!("DNS resolution failed for '{}': {}", host, e) }),
    }

    let Some(curl_bin) = find_executable(&["curl", "/usr/bin/curl"]) else {
        return json!({ "error": "curl binary not found on host" });
    };

    let mut cmd = Command::new(curl_bin);
    cmd.args([
        "-sSL",
        "--fail-with-body",
        "--max-time",
        "20",
        "--max-redirs",
        "5",
        "--max-filesize",
        "2097152",
        "-A",
        "InfimumCode/0.3 (+https://infimum-lang.org)",
        url,
    ]);

    let prefix = format!("$ web_fetch {url}");
    match run_subprocess_managed(cmd, 25, &prefix) {
        Ok((code, raw, timed_out)) => {
            let looks_html = raw
                .trim_start()
                .get(..256)
                .map(|h| {
                    let l = h.to_ascii_lowercase();
                    l.contains("<!doctype html") || l.contains("<html") || l.contains("<head")
                })
                .unwrap_or(false);
            let processed = if mode == "raw" || !looks_html {
                raw
            } else {
                html_to_compact_text(&raw)
            };
            let (content, truncated) = truncate_head_tail(&processed, max_chars);
            json!({
                "ok": code == 0 && !timed_out,
                "url": url,
                "mode": if mode == "raw" || !looks_html { "raw" } else { "text" },
                "exit_code": code,
                "truncated": truncated,
                "chars": content.len(),
                "content": content,
            })
        }
        Err(e) => json!({ "error": e }),
    }
}

// =============================================================================
// 5. Gemini Search Grounding (`web_search`) — Developer Mode
// =============================================================================

pub(super) fn parse_gemini_grounding_response(body: &str, max_results: usize) -> Result<Value, String> {
    let parsed: Value = serde_json::from_str(body)
        .map_err(|e| format!("Invalid JSON from Gemini Search grounding: {e}"))?;

    if let Some(err) = parsed.get("error") {
        let msg = err
            .get("message")
            .and_then(|v| v.as_str())
            .unwrap_or("Gemini API error");
        return Err(msg.to_string());
    }

    let cand = parsed
        .get("candidates")
        .and_then(|c| c.as_array())
        .and_then(|arr| arr.first())
        .ok_or_else(|| "No candidates returned by Gemini Search grounding".to_string())?;

    let mut summary = String::new();
    if let Some(parts) = cand
        .get("content")
        .and_then(|c| c.get("parts"))
        .and_then(|p| p.as_array())
    {
        for p in parts {
            if let Some(t) = p.get("text").and_then(|x| x.as_str()) {
                if !summary.is_empty() {
                    summary.push('\n');
                }
                summary.push_str(t);
            }
        }
    }

    let mut sources: Vec<Value> = Vec::new();
    let mut seen_urls = std::collections::BTreeSet::new();
    if let Some(chunks) = cand
        .get("groundingMetadata")
        .and_then(|g| g.get("groundingChunks"))
        .and_then(|c| c.as_array())
    {
        for ch in chunks {
            if sources.len() >= max_results {
                break;
            }
            if let Some(web) = ch.get("web") {
                let uri = web.get("uri").and_then(|v| v.as_str()).unwrap_or("").trim();
                let title = web.get("title").and_then(|v| v.as_str()).unwrap_or("").trim();
                if !uri.is_empty() && seen_urls.insert(uri.to_string()) {
                    sources.push(json!({
                        "title": title,
                        "url": uri,
                    }));
                }
            }
        }
    }

    let queries_used: Vec<String> = cand
        .get("groundingMetadata")
        .and_then(|g| g.get("webSearchQueries"))
        .and_then(|q| q.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|x| x.as_str().map(|s| s.to_string()))
                .collect()
        })
        .unwrap_or_default();

    let (compact_summary, _) = truncate_head_tail(summary.trim(), 3500);
    Ok(json!({
        "ok": true,
        "summary": compact_summary,
        "sources": sources,
        "queries_used": queries_used,
    }))
}

pub(super) fn tool_web_search(args: &Value, dev_mode: bool) -> Value {
    if !dev_mode {
        return json!({
            "error": "web_search requires Developer Mode (enable Developer Mode in Settings or launch with --dev-mode)"
        });
    }
    let query = args
        .get("query")
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .trim();
    if query.is_empty() {
        return json!({ "error": "Missing required 'query' parameter" });
    }
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .unwrap_or(5)
        .clamp(1, 10) as usize;

    let Some(api_key) = crate::http_server::load_env_key_host_only("GEMINI_API_KEY") else {
        return json!({
            "error": "GEMINI_API_KEY is not configured (required for Gemini Search grounding)"
        });
    };

    let Some(curl_bin) = find_executable(&["curl", "/usr/bin/curl"]) else {
        return json!({ "error": "curl binary not found on host" });
    };

    let payload = json!({
        "contents": [{
            "role": "user",
            "parts": [{
                "text": format!(
                    "Search the web and answer concisely with exact technical facts, versions, and URLs if relevant. Query: {}",
                    query
                )
            }]
        }],
        "tools": [{ "google_search": {} }],
        "generationConfig": {
            "temperature": 0.2,
            "maxOutputTokens": 1024
        }
    });

    let body_str = match serde_json::to_string(&payload) {
        Ok(s) => s,
        Err(e) => return json!({ "error": format!("JSON encode error: {e}") }),
    };

    // Pass API key via temp header config file (0o600) so it never appears in `ps` args.
    let tmp_dir = env::temp_dir();
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_nanos())
        .unwrap_or(0);
    let pid = std::process::id();
    let cfg_path = tmp_dir.join(format!("infimum_gsearch_{pid}_{nonce}.cfg"));
    let body_path = tmp_dir.join(format!("infimum_gsearch_{pid}_{nonce}.json"));

    let esc_key = api_key.replace('\\', "\\\\").replace('"', "\\\"");
    let cfg_content = format!(
        "header = \"x-goog-api-key: {esc_key}\"\nheader = \"Content-Type: application/json\"\n"
    );
    if std::fs::write(&cfg_path, cfg_content).is_err() || std::fs::write(&body_path, body_str).is_err() {
        let _ = std::fs::remove_file(&cfg_path);
        let _ = std::fs::remove_file(&body_path);
        return json!({ "error": "Failed to prepare temporary request files" });
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = std::fs::set_permissions(&cfg_path, std::fs::Permissions::from_mode(0o600));
    }

    let mut candidate_models: Vec<String> = Vec::new();
    if let Ok(env_model) = env::var("INFIMUM_SEARCH_MODEL") {
        let m = env_model.trim();
        if !m.is_empty() {
            candidate_models.push(m.to_string());
        }
    }
    for fallback in ["gemini-3.8-flash", "gemini-2.5-flash", "gemini-2.5-flash-lite"] {
        if !candidate_models.iter().any(|m| m == fallback) {
            candidate_models.push(fallback.to_string());
        }
    }

    let mut last_err = "web_search failed".to_string();
    let mut final_res: Option<Value> = None;

    for model in &candidate_models {
        let url = format!(
            "https://generativelanguage.googleapis.com/v1beta/models/{model}:generateContent"
        );
        let mut cmd = Command::new(&curl_bin);
        cmd.args([
            "-sS",
            "--max-time",
            "25",
            "-K",
            cfg_path.to_str().unwrap_or(""),
            "-X",
            "POST",
            "--data-binary",
            &format!("@{}", body_path.to_string_lossy()),
            &url,
        ]);

        match run_subprocess_managed(cmd, 30, &format!("$ web_search ({model}) {query}")) {
            Ok((code, raw, timed_out)) => {
                if timed_out {
                    last_err = "web_search timed out".to_string();
                    break;
                }
                if code != 0 && raw.trim().is_empty() {
                    last_err = format!("curl failed with exit code {code}");
                    break;
                }
                match parse_gemini_grounding_response(&raw, max_results) {
                    Ok(mut v) => {
                        v["query"] = json!(query);
                        v["model"] = json!(model);
                        final_res = Some(v);
                        break;
                    }
                    Err(e) => {
                        let lower = e.to_ascii_lowercase();
                        let model_unavailable = lower.contains("no longer available")
                            || lower.contains("not found")
                            || lower.contains("not supported")
                            || lower.contains("deprecated");
                        last_err = e;
                        if !model_unavailable {
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                last_err = e;
                break;
            }
        }
    }

    let _ = std::fs::remove_file(&cfg_path);
    let _ = std::fs::remove_file(&body_path);

    final_res.unwrap_or_else(|| json!({ "error": last_err }))
}
