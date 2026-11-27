// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use crate::vault::zeroize_bytes;
use crate::RtError;
use std::collections::BTreeSet;
use std::io::Write;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, ToSocketAddrs};
use std::process::{Command, Stdio};

/// Strict URL authority parser rejecting control chars, whitespace, backslashes, and userinfo `@`
/// (prevents `https://allowed.com:443@evil.com` SSRF).
/// Returns `(authority, host, optional_port)`.
pub fn parse_url_authority(url: &str) -> Option<(&str, &str, Option<&str>)> {
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

/// Returns `true` if `ip` is loopback, private (RFC 1918), link-local / cloud metadata
/// (`169.254.169.254` AWS/GCP/Azure IMDS), CGNAT, broadcast, documentation, or IPv6 ULA/loopback.
pub fn is_forbidden_ip(ip: IpAddr) -> bool {
    match ip {
        IpAddr::V4(v4) => is_forbidden_v4(v4),
        IpAddr::V6(v6) => is_forbidden_v6(v6),
    }
}

fn is_forbidden_v4(v4: Ipv4Addr) -> bool {
    let [a, b, _, _] = v4.octets();
    v4.is_loopback()          // 127.0.0.0/8
        || v4.is_private()    // 10/8, 172.16/12, 192.168/16
        || v4.is_link_local() // 169.254.0.0/16 (AWS/GCP/Azure IMDS 169.254.169.254)
        || v4.is_unspecified() // 0.0.0.0
        || v4.is_broadcast()
        || v4.is_multicast()
        || (a == 100 && (64..=127).contains(&b)) // 100.64.0.0/10 CGNAT
        || a == 0
}

fn is_forbidden_v6(v6: Ipv6Addr) -> bool {
    if let Some(mapped) = v6.to_ipv4_mapped() {
        return is_forbidden_v4(mapped);
    }
    let seg0 = v6.segments()[0];
    v6.is_loopback()            // ::1
        || v6.is_unspecified()  // ::
        || v6.is_multicast()    // ff00::/8
        || (seg0 & 0xfe00) == 0xfc00 // fc00::/7 Unique Local Address (ULA)
        || (seg0 & 0xffc0) == 0xfe80 // fe80::/10 Link-Local
}

/// Validate that `url` is HTTPS-only, free of SSRF tricks, in `allowed_domains`,
/// does not target a literal private/IMDS IP, and (when `resolve_dns` is true) resolves only
/// to public routable IPs (preventing DNS rebinding attacks against cloud metadata services).
///
/// Returns `(host_lc, port, pinned_public_ip)` so callers can pin `curl --resolve` to the exact
/// validated IP and eliminate DNS rebinding TOCTOU windows.
pub fn validate_outbound_url(
    url: &str,
    allowed_domains: &BTreeSet<String>,
    resolve_dns: bool,
) -> Result<(String, u16, Option<IpAddr>), RtError> {
    if !url.starts_with("https://") {
        return Err(RtError::NetworkBlocked(format!(
            "Only HTTPS URLs are permitted in B2B headless runtime: '{url}'"
        )));
    }
    let Some((authority, host_raw, port_opt)) = parse_url_authority(url) else {
        return Err(RtError::NetworkBlocked(format!(
            "Malformed or unsafe URL authority: '{url}'"
        )));
    };
    let host_lc = host_raw.to_ascii_lowercase();
    let auth_lc = authority.to_ascii_lowercase();

    // Block literal private / loopback / IMDS IPs immediately even before allowlist
    if let Ok(ip) = host_lc.parse::<IpAddr>() {
        if is_forbidden_ip(ip) {
            return Err(RtError::NetworkBlocked(format!(
                "Direct private/loopback/metadata IP '{ip}' is forbidden"
            )));
        }
    }
    if host_lc == "localhost" || host_lc.ends_with(".localhost") || host_lc.ends_with(".internal") {
        return Err(RtError::NetworkBlocked(format!(
            "Internal hostname '{host_lc}' is forbidden"
        )));
    }

    if !allowed_domains.contains(&auth_lc) && !allowed_domains.contains(&host_lc) {
        return Err(RtError::NetworkBlocked(format!(
            "Domain '{host_lc}' is not in the effective allowed_net_domains set"
        )));
    }

    let port: u16 = match port_opt {
        Some(p) => p.parse::<u16>().map_err(|_| {
            RtError::NetworkBlocked(format!("Invalid port '{p}' in URL '{url}'"))
        })?,
        None => 443,
    };

    let mut pinned_ip: Option<IpAddr> = None;
    if resolve_dns {
        let addrs = (host_lc.as_str(), port).to_socket_addrs().map_err(|e| {
            RtError::NetworkBlocked(format!("DNS resolution failed for '{host_lc}': {e}"))
        })?;
        for sa in addrs {
            let ip = sa.ip();
            if is_forbidden_ip(ip) {
                return Err(RtError::NetworkBlocked(format!(
                    "DNS for '{host_lc}' resolved to forbidden private/metadata IP {ip}"
                )));
            }
            if pinned_ip.is_none() {
                pinned_ip = Some(ip);
            }
        }
        if pinned_ip.is_none() {
            return Err(RtError::NetworkBlocked(format!(
                "DNS for '{host_lc}' returned no addresses"
            )));
        }
    }

    Ok((host_lc, port, pinned_ip))
}

/// Format a curl `--resolve` entry (`host:port:ip` or `host:port:[ipv6]`) for DNS-rebinding pinning.
pub fn format_curl_resolve_entry(host_lc: &str, port: u16, ip: IpAddr) -> String {
    match ip {
        IpAddr::V4(v4) => format!("{host_lc}:{port}:{v4}"),
        IpAddr::V6(v6) => format!("{host_lc}:{port}:[{v6}]"),
    }
}

fn has_crlf_or_nul(s: &str) -> bool {
    s.bytes().any(|b| b == b'\r' || b == b'\n' || b == 0)
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

/// Perform a hardened HTTPS request via `curl --proto =https --config -` (secrets/headers passed
/// strictly via stdin, never argv) after passing `validate_outbound_url` + DNS anti-SSRF checks.
pub fn safe_https_request(
    method: &str,
    url: &str,
    auth_header: Option<&str>,
    body: Option<&str>,
    allowed_domains: &BTreeSet<String>,
    timeout_ms: u64,
    max_response_bytes: usize,
) -> Result<Vec<u8>, RtError> {
    if method.is_empty()
        || method.len() > 16
        || !method.bytes().all(|b| b.is_ascii_uppercase())
    {
        return Err(RtError::NetworkBlocked(format!(
            "Invalid HTTP method '{method}'"
        )));
    }
    if let Some(h) = auth_header {
        if has_crlf_or_nul(h) {
            return Err(RtError::NetworkBlocked(
                "CRLF/NUL injection detected in Authorization header".to_string(),
            ));
        }
    }
    let (host_lc, port, pinned_ip) = validate_outbound_url(url, allowed_domains, true)?;

    let timeout_secs = (timeout_ms.max(1000)).div_ceil(1000);
    let mut cfg = String::new();
    cfg.push_str(&format!("url = \"{}\"\n", escape_curl_config_str(url)));
    cfg.push_str(&format!("request = \"{method}\"\n"));
    cfg.push_str("proto = \"=https\"\n");
    cfg.push_str("proto-redir = \"=https\"\n");
    cfg.push_str("max-redirs = 0\n");
    if let Some(ip) = pinned_ip {
        let resolve_val = format_curl_resolve_entry(&host_lc, port, ip);
        cfg.push_str(&format!(
            "resolve = \"{}\"\n",
            escape_curl_config_str(&resolve_val)
        ));
    }
    cfg.push_str(&format!("max-time = {timeout_secs}\n"));
    cfg.push_str(&format!("max-filesize = {max_response_bytes}\n"));
    if let Some(h) = auth_header {
        if !h.is_empty() {
            cfg.push_str(&format!(
                "header = \"Authorization: {}\"\n",
                escape_curl_config_str(h)
            ));
        }
    }
    if let Some(b) = body {
        if !b.is_empty() {
            cfg.push_str("header = \"Content-Type: application/json\"\n");
            cfg.push_str(&format!("data-raw = \"{}\"\n", escape_curl_config_str(b)));
        }
    }

    let spawn_res = Command::new("curl")
        .args(["-sS", "--fail-with-body", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn();

    let mut child = match spawn_res {
        Ok(c) => c,
        Err(e) => {
            // SAFETY: zeroizing all bytes to 0x00 leaves valid UTF-8 before drop.
            unsafe { zeroize_bytes(cfg.as_bytes_mut()) };
            return Err(RtError::NetworkBlocked(format!(
                "Failed to spawn curl: {e}"
            )));
        }
    };

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(cfg.as_bytes());
    }
    // Zeroize stdin config buffer immediately (contains Authorization Bearer secret)
    // SAFETY: writing 0x00 bytes produces valid UTF-8 before drop.
    unsafe { zeroize_bytes(cfg.as_bytes_mut()) };

    let output = child
        .wait_with_output()
        .map_err(|e| RtError::NetworkBlocked(format!("curl failed: {e}")))?;

    if output.stdout.len() > max_response_bytes {
        return Err(RtError::NetworkBlocked(format!(
            "HTTP response ({} bytes) exceeded max_http_response_bytes ({max_response_bytes})",
            output.stdout.len()
        )));
    }
    if !output.status.success() {
        return Err(RtError::NetworkBlocked(format!(
            "HTTPS request failed (exit {:?})",
            output.status.code()
        )));
    }
    Ok(output.stdout)
}
