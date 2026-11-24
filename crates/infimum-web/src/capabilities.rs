//! Pure Browser Capability Host ABI (`env.host_call(op, a0, a1, a2) -> i64`).
//!
//! Enforces `CapabilityPolicy` (`infimum.passport` + CLI flags) on EVERY host call.
//! Contains ONLY universal browser primitives:
//! - Audio synthesis (`OP_AUDIO_PRESET`, `OP_AUDIO_TONE`)
//! - Whitelisted HTTP (`OP_NET_HTTP_REQUEST`, `OP_NET_FETCH_CLEAN_HTML`)
//! - Explicit launch arguments (`OP_CAP_ARG_GET` via `--cap-arg=KEY=VAL`)
//! - Process telemetry (`OP_GET_RSS_TENTHS_MB`)
//!
//! Zero filesystem mutation, zero process spawning, zero LLM or IDE logic.

#[cfg(not(target_arch = "wasm32"))]
use crate::clipboard::{decode_png_to_rgba, PastedImage};
use crate::passport::CapabilityPolicy;
#[cfg(not(target_arch = "wasm32"))]
use crate::sanitizer::sanitize_html;
#[cfg(not(target_arch = "wasm32"))]
use std::fs;
#[cfg(not(target_arch = "wasm32"))]
use std::io::{Read, Write};
#[cfg(not(target_arch = "wasm32"))]
use std::net::TcpStream;
#[cfg(not(target_arch = "wasm32"))]
use std::process::{Command, Stdio};
use std::sync::{Mutex, OnceLock};

pub const OP_NET_FETCH_CLEAN_HTML: i64 = 20;
pub const OP_GET_RSS_TENTHS_MB: i64 = 40;
pub const OP_AUDIO_PRESET: i64 = 50;
pub const OP_AUDIO_TONE: i64 = 51;
pub const OP_NET_HTTP_REQUEST: i64 = 60;
pub const OP_CAP_ARG_GET: i64 = 61;
pub const OP_GPU_LOAD_IMAGE_URL: i64 = 62;
pub const OP_GPU_CLEAR_IMAGES: i64 = 63;
pub const OP_NET_POST_PENDING_IMAGE: i64 = 64;
pub const OP_INPUT_SET_POINTER_LOCK: i64 = 65;

fn pointer_lock_req_cell() -> &'static Mutex<Option<bool>> {
    static CELL: OnceLock<Mutex<Option<bool>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

pub fn take_pointer_lock_request() -> Option<bool> {
    pointer_lock_req_cell().lock().ok().and_then(|mut g| g.take())
}

pub enum GpuImageCmd {
    Clear,
    Add(u32, u32, Vec<u8>),
}

fn gpu_image_cmd_queue() -> &'static Mutex<Vec<GpuImageCmd>> {
    static CELL: OnceLock<Mutex<Vec<GpuImageCmd>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn drain_gpu_image_cmds() -> Vec<GpuImageCmd> {
    gpu_image_cmd_queue()
        .lock()
        .ok()
        .map(|mut g| std::mem::take(&mut *g))
        .unwrap_or_default()
}

#[cfg(not(target_arch = "wasm32"))]
fn browser_pending_images() -> &'static Mutex<Vec<PastedImage>> {
    static CELL: OnceLock<Mutex<Vec<PastedImage>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Vec::new()))
}

#[cfg(not(target_arch = "wasm32"))]
pub fn push_browser_pending_image(img: PastedImage) -> bool {
    if let Ok(mut g) = browser_pending_images().lock() {
        if g.len() < 6 {
            g.push(img);
            return true;
        }
    }
    false
}

#[cfg(not(target_arch = "wasm32"))]
pub fn remove_browser_pending_image(idx: usize) {
    if let Ok(mut g) = browser_pending_images().lock() {
        if idx < g.len() {
            g.remove(idx);
        }
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn clear_browser_pending_images() {
    if let Ok(mut g) = browser_pending_images().lock() {
        g.clear();
    }
}

#[cfg(not(target_arch = "wasm32"))]
pub fn has_browser_pending_images() -> bool {
    browser_pending_images()
        .lock()
        .ok()
        .map(|g| !g.is_empty())
        .unwrap_or(false)
}

pub const E_CAP_DENIED: i64 = -1;
pub const E_INVALID_ARG: i64 = -2;
pub const E_IO_FAILED: i64 = -3;

#[cfg(not(target_arch = "wasm32"))]
fn current_rss_tenths_mb() -> i64 {
    if let Ok(status) = fs::read_to_string("/proc/self/status") {
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                if let Some(kb_str) = rest.split_whitespace().next() {
                    if let Ok(kb) = kb_str.parse::<i64>() {
                        return (kb * 10 + 512) / 1024;
                    }
                }
            }
        }
    }
    0
}

fn read_word(mem: &[u8], word_addr: usize) -> i32 {
    let off = word_addr.saturating_mul(4);
    if off + 4 <= mem.len() {
        i32::from_le_bytes(mem[off..off + 4].try_into().unwrap_or([0; 4]))
    } else {
        0
    }
}

fn write_word(mem: &mut [u8], word_addr: usize, val: i32) {
    let off = word_addr.saturating_mul(4);
    if off + 4 <= mem.len() {
        mem[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }
}

/// Read an Infimum UTF-32 string (`[len, cp0, cp1, ...]`) from Wasm linear memory.
pub fn read_ifm_string(mem: &[u8], word_ptr: i64, max_chars: usize) -> Option<String> {
    if word_ptr <= 0 {
        return None;
    }
    let base = word_ptr as usize;
    let len = read_word(mem, base);
    if len < 0 {
        return None;
    }
    let n = (len as usize).min(max_chars);
    let mut s = String::with_capacity(n);
    for i in 0..n {
        let cp = read_word(mem, base + 1 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            s.push(ch);
        }
    }
    Some(s)
}

/// Write a Rust `&str` as an Infimum UTF-32 string (`[len, cp0, cp1, ...]`) into Wasm linear memory.
pub fn write_ifm_string(mem: &mut [u8], word_ptr: i64, s: &str, max_chars: usize) -> i64 {
    if word_ptr <= 0 {
        return E_INVALID_ARG;
    }
    let base = word_ptr as usize;
    let mut written = 0usize;
    for ch in s.chars().take(max_chars) {
        let off = (base + 1 + written).saturating_mul(4);
        if off + 4 > mem.len() {
            break;
        }
        write_word(mem, base + 1 + written, ch as i32);
        written += 1;
    }
    write_word(mem, base, written as i32);
    written as i64
}

fn has_crlf_or_nul(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
}

fn is_valid_http_method(method: &str) -> bool {
    !method.is_empty()
        && method.len() <= 16
        && method.bytes().all(|b| b.is_ascii_uppercase())
}

fn escape_curl_config_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 4);
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '"' => out.push_str("\\\""),
            _ => out.push(ch),
        }
    }
    out
}

fn parse_url_authority(url: &str) -> Option<(&str, &str, Option<&str>)> {
    if url
        .bytes()
        .any(|b| b.is_ascii_control() || b.is_ascii_whitespace() || b == b'\\')
    {
        return None;
    }
    let rest = url
        .strip_prefix("https://")
        .or_else(|| url.strip_prefix("http://"))?;
    let auth_end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
    let authority = &rest[..auth_end];
    if authority.is_empty() || authority.contains('@') {
        return None;
    }
    let (host, port_opt) = match authority.split_once(':') {
        Some((h, p)) => {
            if h.is_empty()
                || p.is_empty()
                || !p.bytes().all(|b| b.is_ascii_digit())
                || p.parse::<u16>().is_err()
            {
                return None;
            }
            (h, Some(p))
        }
        None => (authority, None),
    };
    if host.is_empty()
        || !host
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'.' || b == b'-')
    {
        return None;
    }
    Some((authority, host, port_opt))
}

pub fn policy_allows_url(policy: &CapabilityPolicy, url: &str) -> bool {
    let Some((authority, host_raw, _port_opt)) = parse_url_authority(url) else {
        return false;
    };
    let host_port = authority.to_ascii_lowercase();
    if policy.allowed_net_domains.contains(&host_port) {
        return true;
    }
    let host = host_raw.to_ascii_lowercase();
    policy.allowed_net_domains.contains(&host)
}

/// Perform a minimal HTTP/1.1 request over `TcpStream` (for `http://`) or `curl` (for `https://`).
#[cfg(not(target_arch = "wasm32"))]
fn perform_http_request(
    method: &str,
    url: &str,
    auth_header: &str,
    content_type: &str,
    body: &str,
) -> Result<(i64, String), ()> {
    if !is_valid_http_method(method)
        || has_crlf_or_nul(auth_header)
        || has_crlf_or_nul(content_type)
    {
        return Err(());
    }
    let (authority, _, _) = parse_url_authority(url).ok_or(())?;

    if let Some(rest) = url.strip_prefix("http://") {
        let after_auth = &rest[authority.len()..];
        let after_auth_no_frag = after_auth.split('#').next().unwrap_or("");
        let path_owned: String;
        let path = if after_auth_no_frag.is_empty() {
            "/"
        } else if after_auth_no_frag.starts_with('/') {
            after_auth_no_frag
        } else {
            path_owned = format!("/{after_auth_no_frag}");
            &path_owned
        };
        let addr = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        let mut stream = TcpStream::connect(&addr).map_err(|_| ())?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(30)));
        let _ = stream.set_write_timeout(Some(std::time::Duration::from_secs(10)));

        let mut req = format!(
            "{method} {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\nContent-Length: {}\r\n",
            body.len()
        );
        if !content_type.is_empty() {
            req.push_str(&format!("Content-Type: {content_type}\r\n"));
        }
        if !auth_header.is_empty() {
            req.push_str(&format!("Authorization: {auth_header}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).map_err(|_| ())?;
        if !body.is_empty() {
            stream.write_all(body.as_bytes()).map_err(|_| ())?;
        }
        stream.flush().map_err(|_| ())?;

        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|_| ())?;
        let raw_str = String::from_utf8_lossy(&raw);
        let (hdr, resp_body) = raw_str.split_once("\r\n\r\n").ok_or(())?;
        let status = hdr
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<i64>().ok())
            .unwrap_or(200);
        return Ok((status, resp_body.to_string()));
    }

    // Pass sensitive headers via `--config -` (stdin) so Authorization tokens never appear in argv / `ps aux`.
    let mut cfg_text = format!(
        "url = \"{}\"\n",
        escape_curl_config_str(url)
    );
    if !content_type.is_empty() {
        cfg_text.push_str(&format!(
            "header = \"Content-Type: {}\"\n",
            escape_curl_config_str(content_type)
        ));
    }
    if !auth_header.is_empty() {
        cfg_text.push_str(&format!(
            "header = \"Authorization: {}\"\n",
            escape_curl_config_str(auth_header)
        ));
    }

    let mut body_tmp: Option<std::path::PathBuf> = None;
    let mut cmd = Command::new("curl");
    cmd.args([
        "-sS",
        "--proto",
        "=http,https",
        "-X",
        method,
        "--max-time",
        "20",
        "-w",
        "\n%{http_code}",
        "--config",
        "-",
    ]);
    if !body.is_empty() {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0);
        let tmp_path = std::env::temp_dir().join(format!(
            "infimum_req_{}_{}.tmp",
            std::process::id(),
            nanos
        ));
        std::fs::write(&tmp_path, body.as_bytes()).map_err(|_| ())?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = std::fs::set_permissions(&tmp_path, std::fs::Permissions::from_mode(0o600));
        }
        cmd.args(["--data-binary", &format!("@{}", tmp_path.display())]);
        body_tmp = Some(tmp_path);
    }
    cmd.stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let spawn_res = cmd.spawn();
    let mut child = match spawn_res {
        Ok(c) => c,
        Err(_) => {
            if let Some(p) = body_tmp {
                let _ = std::fs::remove_file(p);
            }
            return Err(());
        }
    };
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg_text.as_bytes());
    }
    let out_res = child.wait_with_output();
    if let Some(p) = body_tmp {
        let _ = std::fs::remove_file(p);
    }
    let out = out_res.map_err(|_| ())?;
    let s = String::from_utf8_lossy(&out.stdout);
    let (resp_body, code_s) = s.rsplit_once('\n').unwrap_or((&s, "200"));
    let status = code_s.trim().parse::<i64>().unwrap_or(200);
    Ok((status, resp_body.to_string()))
}

#[cfg(not(target_arch = "wasm32"))]
fn fetch_http_bytes(url: &str, auth_header: &str) -> Result<Vec<u8>, ()> {
    if has_crlf_or_nul(auth_header) {
        return Err(());
    }
    let (authority, _, _) = parse_url_authority(url).ok_or(())?;
    if let Some(rest) = url.strip_prefix("http://") {
        let after_auth = &rest[authority.len()..];
        let after_auth_no_frag = after_auth.split('#').next().unwrap_or("");
        let path_owned: String;
        let path = if after_auth_no_frag.is_empty() {
            "/"
        } else if after_auth_no_frag.starts_with('/') {
            after_auth_no_frag
        } else {
            path_owned = format!("/{after_auth_no_frag}");
            &path_owned
        };
        let addr = if authority.contains(':') {
            authority.to_string()
        } else {
            format!("{authority}:80")
        };
        let mut stream = TcpStream::connect(&addr).map_err(|_| ())?;
        let _ = stream.set_read_timeout(Some(std::time::Duration::from_secs(15)));
        let mut req = format!("GET {path} HTTP/1.1\r\nHost: {authority}\r\nConnection: close\r\n");
        if !auth_header.is_empty() {
            req.push_str(&format!("Authorization: {auth_header}\r\n"));
        }
        req.push_str("\r\n");
        stream.write_all(req.as_bytes()).map_err(|_| ())?;
        stream.flush().map_err(|_| ())?;
        let mut raw = Vec::new();
        stream.read_to_end(&mut raw).map_err(|_| ())?;
        let sep = raw.windows(4).position(|w| w == b"\r\n\r\n").ok_or(())?;
        let hdr = String::from_utf8_lossy(&raw[..sep]);
        let status = hdr
            .lines()
            .next()
            .and_then(|l| l.split_whitespace().nth(1))
            .and_then(|c| c.parse::<i64>().ok())
            .unwrap_or(500);
        if status != 200 {
            return Err(());
        }
        return Ok(raw[sep + 4..].to_vec());
    }
    Err(())
}

#[cfg(target_arch = "wasm32")]
pub fn dispatch_host_call(
    policy: &CapabilityPolicy,
    mem: &mut [u8],
    op: i64,
    a0: i64,
    a1: i64,
    a2: i64,
) -> i64 {
    match op {
        OP_AUDIO_PRESET => {
            crate::audio::play_preset(a0, a1, a2);
            1
        }
        OP_AUDIO_TONE => {
            crate::audio::play_tone(a0, a1, a2);
            1
        }
        OP_INPUT_SET_POINTER_LOCK => {
            if let Ok(mut g) = pointer_lock_req_cell().lock() {
                *g = Some(a0 != 0);
            }
            1
        }
        OP_GPU_CLEAR_IMAGES => {
            if let Ok(mut q) = gpu_image_cmd_queue().lock() {
                q.push(GpuImageCmd::Clear);
            }
            1
        }
        OP_GET_RSS_TENTHS_MB => 0,
        OP_CAP_ARG_GET => {
            let Some(key) = read_ifm_string(mem, a0, 128) else {
                return E_INVALID_ARG;
            };
            let max_chars = (a2.max(1) as usize).min(4096);
            match policy.cap_args.get(key.trim()) {
                Some(val) => write_ifm_string(mem, a1, val, max_chars),
                None => E_INVALID_ARG,
            }
        }
        _ => E_CAP_DENIED,
    }
}

/// Dispatch a capability-checked host call from Wasm (`env.host_call`).
#[cfg(not(target_arch = "wasm32"))]
pub fn dispatch_host_call(
    policy: &CapabilityPolicy,
    mem: &mut [u8],
    op: i64,
    a0: i64,
    a1: i64,
    a2: i64,
) -> i64 {
    match op {
        OP_AUDIO_PRESET => {
            crate::audio::play_preset(a0, a1, a2);
            1
        }
        OP_AUDIO_TONE => {
            crate::audio::play_tone(a0, a1, a2);
            1
        }
        OP_GET_RSS_TENTHS_MB => current_rss_tenths_mb(),
        OP_CAP_ARG_GET => {
            let Some(key) = read_ifm_string(mem, a0, 128) else {
                return E_INVALID_ARG;
            };
            let max_chars = (a2.max(1) as usize).min(4096);
            match policy.cap_args.get(key.trim()) {
                Some(val) => write_ifm_string(mem, a1, val, max_chars),
                None => E_INVALID_ARG,
            }
        }
        OP_NET_HTTP_REQUEST => {
            // Request format in `a0`: "METHOD\nURL\nAUTHORIZATION\nCONTENT_TYPE\nBODY"
            let Some(req_str) = read_ifm_string(mem, a0, 65_536) else {
                return E_INVALID_ARG;
            };
            let mut lines = req_str.splitn(5, '\n');
            let method = lines.next().unwrap_or("").trim();
            let url = lines.next().unwrap_or("").trim();
            let auth = lines.next().unwrap_or("").trim();
            let ctype = lines.next().unwrap_or("").trim();
            let body = lines.next().unwrap_or("");
            if method.is_empty() || url.is_empty() {
                return E_INVALID_ARG;
            }
            if !policy_allows_url(policy, url) {
                return E_CAP_DENIED;
            }
            let max_chars = (a2.max(64) as usize).min(65_536);
            match perform_http_request(method, url, auth, ctype, body) {
                Ok((status, resp_body)) => {
                    let _ = write_ifm_string(mem, a1, &resp_body, max_chars);
                    status
                }
                Err(_) => E_IO_FAILED,
            }
        }
        OP_INPUT_SET_POINTER_LOCK => {
            if let Ok(mut g) = pointer_lock_req_cell().lock() {
                *g = Some(a0 != 0);
            }
            1
        }
        OP_GPU_CLEAR_IMAGES => {
            if let Ok(mut q) = gpu_image_cmd_queue().lock() {
                q.push(GpuImageCmd::Clear);
            }
            1
        }
        OP_GPU_LOAD_IMAGE_URL => {
            // `a0`: "URL\nAUTHORIZATION" -> decodes PNG/JPEG and queues GPU texture upload
            let Some(req_str) = read_ifm_string(mem, a0, 2048) else {
                return E_INVALID_ARG;
            };
            let mut lines = req_str.splitn(2, '\n');
            let url = lines.next().unwrap_or("").trim();
            let auth = lines.next().unwrap_or("").trim();
            if url.is_empty() {
                return E_INVALID_ARG;
            }
            if !policy_allows_url(policy, url) {
                return E_CAP_DENIED;
            }
            let Ok(bytes) = fetch_http_bytes(url, auth) else {
                return 0;
            };
            let Some((w, h, rgba)) = decode_png_to_rgba(&bytes) else {
                return 0;
            };
            if let Ok(mut q) = gpu_image_cmd_queue().lock() {
                q.push(GpuImageCmd::Add(w, h, rgba));
            }
            ((w as i64) << 16) | ((h as i64) & 0xFFFF)
        }
        OP_NET_POST_PENDING_IMAGE => {
            // `a0`: "URL\nAUTHORIZATION", `a1`: pending index, `a2`: 1 to clear after upload
            let Some(req_str) = read_ifm_string(mem, a0, 2048) else {
                return E_INVALID_ARG;
            };
            let mut lines = req_str.splitn(2, '\n');
            let url = lines.next().unwrap_or("").trim();
            let auth = lines.next().unwrap_or("").trim();
            if url.is_empty() {
                return E_INVALID_ARG;
            }
            if !policy_allows_url(policy, url) {
                return E_CAP_DENIED;
            }
            let idx = a1.max(0) as usize;
            let img_opt = browser_pending_images()
                .lock()
                .ok()
                .and_then(|g| g.get(idx).cloned());
            let Some(img) = img_opt else {
                return 0;
            };
            let payload = serde_json::json!({
                "base64": img.base64_data,
                "mime": img.mime_type,
                "w": img.width,
                "h": img.height,
            })
            .to_string();
            let status = match perform_http_request("POST", url, auth, "application/json", &payload)
            {
                Ok((st, _)) => st,
                Err(_) => E_IO_FAILED,
            };
            if a2 == 1 {
                clear_browser_pending_images();
            }
            status
        }
        OP_NET_FETCH_CLEAN_HTML => {
            let Some(url) = read_ifm_string(mem, a0, 1024) else {
                return E_INVALID_ARG;
            };
            if !policy_allows_url(policy, &url) {
                return E_CAP_DENIED;
            }
            let max_chars = (a2.max(128) as usize).min(32_768);
            let out = Command::new("curl")
                .args(["-sS", "--proto", "=http,https", "--max-time", "12", "--", &url])
                .output();
            let Ok(o) = out else {
                return E_IO_FAILED;
            };
            let html = String::from_utf8_lossy(&o.stdout);
            let doc = sanitize_html(&html);
            let header = format!(
                "[Sanitized: stripped {} script(s), {} iframe(s), {} HTML tag(s)]\n{}",
                doc.stripped_scripts, doc.stripped_iframes, doc.stripped_tags, doc.clean_text
            );
            write_ifm_string(mem, a1, &header, max_chars)
        }
        _ => E_INVALID_ARG,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blocks_http_without_allowed_net_domain() {
        let policy = CapabilityPolicy::default();
        let mut mem = vec![0u8; 65536];
        write_ifm_string(&mut mem, 100, "GET\nhttp://127.0.0.1:43210/health\n\n\n", 1024);
        assert_eq!(
            dispatch_host_call(&policy, &mut mem, OP_NET_HTTP_REQUEST, 100, 1000, 4096),
            E_CAP_DENIED
        );
    }

    #[test]
    fn exact_port_allowlist_blocks_other_localhost_ports() {
        let mut policy = CapabilityPolicy::default();
        policy
            .allowed_net_domains
            .insert("127.0.0.1:43210".to_string());
        assert!(policy_allows_url(&policy, "http://127.0.0.1:43210/health"));
        assert!(!policy_allows_url(&policy, "http://127.0.0.1:8080/secret"));
        assert!(!policy_allows_url(&policy, "https://evil.example.com/"));
    }

    #[test]
    fn blocks_userinfo_ssrf_and_crlf_injection() {
        let mut policy = CapabilityPolicy::default();
        policy
            .allowed_net_domains
            .insert("api.openai.com".to_string());
        assert!(policy_allows_url(&policy, "https://api.openai.com/v1/models"));
        assert!(policy_allows_url(&policy, "https://api.openai.com:443/v1/models"));
        assert!(policy_allows_url(&policy, "https://api.openai.com?foo=bar"));
        // SSRF via userinfo `@`, backslash, non-numeric port, whitespace, CRLF
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com:443@evil.com/steal"
        ));
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com@evil.com/steal"
        ));
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com\\@evil.com/steal"
        ));
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com:443:80/v1"
        ));
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com/v1\r\nX-Injected: 1"
        ));
        assert!(!policy_allows_url(
            &policy,
            "https://api.openai.com/v1 HTTP/1.1"
        ));
        // CRLF in headers/method must fail perform_http_request immediately
        assert!(perform_http_request(
            "GET\r\nX: 1",
            "http://127.0.0.1:1/",
            "",
            "",
            ""
        )
        .is_err());
        assert!(perform_http_request(
            "GET",
            "http://127.0.0.1:1/",
            "Bearer ok\r\nX-Evil: 1",
            "",
            ""
        )
        .is_err());
    }

    #[test]
    fn cap_arg_returns_only_explicitly_passed_args() {
        let mut policy = CapabilityPolicy::default();
        policy
            .cap_args
            .insert("AGENT_URL".to_string(), "http://127.0.0.1:43210".to_string());
        let mut mem = vec![0u8; 65536];
        write_ifm_string(&mut mem, 100, "AGENT_URL", 64);
        let n = dispatch_host_call(&policy, &mut mem, OP_CAP_ARG_GET, 100, 200, 256);
        assert!(n > 0);
        assert_eq!(
            read_ifm_string(&mem, 200, 256).as_deref(),
            Some("http://127.0.0.1:43210")
        );

        // Must NOT expose OS env vars like HOME or PATH
        write_ifm_string(&mut mem, 100, "HOME", 64);
        assert_eq!(
            dispatch_host_call(&policy, &mut mem, OP_CAP_ARG_GET, 100, 200, 256),
            E_INVALID_ARG
        );
    }
}
