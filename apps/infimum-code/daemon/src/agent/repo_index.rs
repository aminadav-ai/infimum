// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

//! Live Infimum monorepo + website source index for Infimum Code AI.
//!
//! Two seamless modes:
//! 1. Local dev mode: when a local `infimum` monorepo checkout is detected near the executable or
//!    current directory, reads live files directly from disk (including uncommitted edits) and
//!    optionally mounts a sibling `infimum-site` checkout under `site/`.
//! 2. Standalone client mode: syncs `repo_meta.json` + `repo_bundle.json.gz` (and optional
//!    `site_bundle.json.gz`) from `https://infimum-lang.org/code/` into
//!    `~/.cache/infimum-code/` with atomic writes, 10-minute refresh TTL, and offline fallback.

use std::collections::BTreeMap;
use std::fs;
use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use flate2::read::GzDecoder;
use serde::Deserialize;
use serde_json::{json, Value};

const REMOTE_META_URL: &str = "https://infimum-lang.org/code/repo_meta.json";
const REMOTE_BUNDLE_URL: &str = "https://infimum-lang.org/code/repo_bundle.json.gz";
const REMOTE_SITE_BUNDLE_URL: &str = "https://infimum-lang.org/code/site_bundle.json.gz";
const REMOTE_REFRESH_TTL: Duration = Duration::from_secs(600);
const MAX_FILE_BYTES: usize = 512 * 1024;
const MAX_READ_LINES: usize = 400;

const ALLOWED_EXTS: &[&str] = &[
    "rs", "ifm", "wgsl", "toml", "md", "sh", "yml", "yaml", "json", "js", "html", "css", "py",
    "txt",
];
const ALLOWED_EXACT_NAMES: &[&str] = &[
    "LICENSE",
    "LICENSE-APACHE",
    "LICENSE-MIT",
    "NOTICE",
    "_headers",
    ".gitignore",
];
const SKIP_DIRS: &[&str] = &[".git", "target", "node_modules", ".wrangler"];

#[derive(Debug, Clone)]
pub struct RepoSnapshot {
    pub sha: String,
    pub files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct BundleJson {
    #[serde(default)]
    sha: String,
    #[serde(default)]
    files: BTreeMap<String, String>,
}

#[derive(Debug, Deserialize)]
struct MetaJson {
    #[serde(default)]
    sha: String,
}

struct CachedRemote {
    snapshot: Arc<RepoSnapshot>,
    checked_at: Instant,
}

static REMOTE_CACHE: OnceLock<Mutex<Option<CachedRemote>>> = OnceLock::new();

pub fn normalize_repo_path(raw: &str) -> Option<String> {
    let trimmed = raw.trim().replace('\\', "/");
    let stripped = trimmed.trim_start_matches('/').trim_end_matches('/');
    if stripped.is_empty() {
        return Some(String::new());
    }
    if stripped.split('/').any(|seg| seg == ".." || seg == ".") {
        return None;
    }
    Some(stripped.to_string())
}

fn is_allowed_file(path: &Path) -> bool {
    if let Some(name) = path.file_name().and_then(|s| s.to_str()) {
        if ALLOWED_EXACT_NAMES.contains(&name) {
            return true;
        }
    }
    path.extension()
        .and_then(|e| e.to_str())
        .map(|e| ALLOWED_EXTS.contains(&e.to_ascii_lowercase().as_str()))
        .unwrap_or(false)
}

fn is_monorepo_root(dir: &Path) -> bool {
    dir.join("crates/infimum-lang/Cargo.toml").is_file()
        && dir.join("apps/infimum-code/daemon/Cargo.toml").is_file()
}

pub fn detect_local_monorepo_root() -> Option<PathBuf> {
    if let Ok(env_dir) = std::env::var("INFIMUM_MONOREPO_ROOT") {
        let p = PathBuf::from(env_dir);
        if is_monorepo_root(&p) {
            return Some(p);
        }
    }
    let mut seeds = Vec::new();
    if let Ok(cwd) = std::env::current_dir() {
        seeds.push(cwd);
    }
    if let Ok(exe) = std::env::current_exe() {
        if let Some(p) = exe.parent() {
            seeds.push(p.to_path_buf());
        }
    }
    for seed in seeds {
        let mut cur: Option<&Path> = Some(seed.as_path());
        for _ in 0..8 {
            let Some(d) = cur else { break };
            if is_monorepo_root(d) {
                return Some(d.to_path_buf());
            }
            cur = d.parent();
        }
    }
    None
}

fn collect_live_repo_files(
    repo_root: &Path,
    prefix: &str,
    out: &mut BTreeMap<String, String>,
) {
    let git_out = Command::new("git")
        .args(["ls-files", "-z"])
        .current_dir(repo_root)
        .output();
    let rel_paths: Vec<PathBuf> = match git_out {
        Ok(o) if o.status.success() => o
            .stdout
            .split(|&b| b == 0)
            .filter(|s| !s.is_empty())
            .filter_map(|s| std::str::from_utf8(s).ok())
            .map(PathBuf::from)
            .collect(),
        _ => {
            let mut acc = Vec::new();
            walk_dir_fallback(repo_root, repo_root, &mut acc);
            acc
        }
    };

    for rel in rel_paths {
        let rel_str = rel.to_string_lossy().replace('\\', "/");
        if prefix == "site"
            && (rel_str.starts_with("public/code/z3/") || rel_str.starts_with("public/code/examples/"))
        {
            continue;
        }
        if !is_allowed_file(&rel) {
            continue;
        }
        let full = repo_root.join(&rel);
        let Ok(bytes) = fs::read(&full) else {
            continue;
        };
        if bytes.len() > MAX_FILE_BYTES || bytes.contains(&0) {
            continue;
        }
        let Ok(text) = String::from_utf8(bytes) else {
            continue;
        };
        let key = if prefix.is_empty() {
            rel_str
        } else {
            format!("{prefix}/{rel_str}")
        };
        out.insert(key, text);
    }
}

fn walk_dir_fallback(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) {
    let Ok(rd) = fs::read_dir(dir) else { return };
    for entry in rd.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let skip = path
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| SKIP_DIRS.contains(&n))
                .unwrap_or(false);
            if !skip {
                walk_dir_fallback(root, &path, out);
            }
        } else if path.is_file() {
            if let Ok(rel) = path.strip_prefix(root) {
                out.push(rel.to_path_buf());
            }
        }
    }
}

fn load_live_local_snapshot(monorepo: &Path) -> RepoSnapshot {
    let sha = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .current_dir(monorepo)
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "local-worktree".to_string());

    let mut files = BTreeMap::new();
    collect_live_repo_files(monorepo, "", &mut files);

    if let Some(site_dir) = monorepo.parent().map(|p| p.join("infimum-site")) {
        if site_dir.join("src/worker.js").is_file() {
            collect_live_repo_files(&site_dir, "site", &mut files);
        }
    }
    RepoSnapshot { sha, files }
}

pub fn load_bundle_from_bytes(gz_bytes: &[u8]) -> Result<RepoSnapshot, String> {
    let mut decoder = GzDecoder::new(gz_bytes);
    let mut json_str = String::new();
    decoder
        .read_to_string(&mut json_str)
        .map_err(|e| format!("Failed to decompress gzip bundle: {e}"))?;
    let parsed: BundleJson =
        serde_json::from_str(&json_str).map_err(|e| format!("Invalid bundle JSON: {e}"))?;
    Ok(RepoSnapshot {
        sha: if parsed.sha.is_empty() {
            "unknown".to_string()
        } else {
            parsed.sha
        },
        files: parsed.files,
    })
}

fn cache_dir() -> PathBuf {
    if let Ok(xdg) = std::env::var("XDG_CACHE_HOME") {
        if !xdg.trim().is_empty() {
            return PathBuf::from(xdg).join("infimum-code");
        }
    }
    if let Ok(home) = std::env::var("HOME") {
        if !home.trim().is_empty() {
            return PathBuf::from(home).join(".cache").join("infimum-code");
        }
    }
    if let Ok(local_app) = std::env::var("LOCALAPPDATA") {
        if !local_app.trim().is_empty() {
            return PathBuf::from(local_app).join("infimum-code");
        }
    }
    std::env::temp_dir().join("infimum-code-cache")
}

fn curl_download_bytes(url: &str, max_time_secs: &str) -> Result<Vec<u8>, String> {
    let out = Command::new("curl")
        .args([
            "-fsSL",
            "--proto",
            "=https",
            "--max-time",
            max_time_secs,
            url,
        ])
        .output()
        .map_err(|e| format!("curl failed to spawn: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "curl HTTP error fetching {url}: {}",
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    Ok(out.stdout)
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).map_err(|e| format!("Failed to create cache dir: {e}"))?;
    }
    let tmp = path.with_extension("tmp");
    fs::write(&tmp, bytes).map_err(|e| format!("Failed to write temp cache file: {e}"))?;
    fs::rename(&tmp, path).map_err(|e| format!("Failed to rename temp cache file: {e}"))?;
    Ok(())
}

fn load_or_sync_remote_snapshot() -> Result<Arc<RepoSnapshot>, String> {
    let cell = REMOTE_CACHE.get_or_init(|| Mutex::new(None));
    if let Ok(guard) = cell.lock() {
        if let Some(ref cached) = *guard {
            if cached.checked_at.elapsed() < REMOTE_REFRESH_TTL {
                return Ok(Arc::clone(&cached.snapshot));
            }
        }
    }

    let dir = cache_dir();
    let meta_path = dir.join("repo_meta.json");
    let bundle_path = dir.join("repo_bundle.json.gz");
    let site_bundle_path = dir.join("site_bundle.json.gz");

    let local_sha = fs::read_to_string(&meta_path)
        .ok()
        .and_then(|s| serde_json::from_str::<MetaJson>(&s).ok())
        .map(|m| m.sha)
        .unwrap_or_default();

    let mut need_download = !bundle_path.is_file();
    let mut remote_sha = String::new();
    let mut remote_meta_bytes: Option<Vec<u8>> = None;

    if let Ok(meta_bytes) = curl_download_bytes(REMOTE_META_URL, "3") {
        if let Ok(meta) = serde_json::from_slice::<MetaJson>(&meta_bytes) {
            if !meta.sha.is_empty() {
                remote_sha = meta.sha;
                remote_meta_bytes = Some(meta_bytes);
                if remote_sha != local_sha {
                    need_download = true;
                }
            }
        }
    }

    if need_download {
        let url = if remote_sha.is_empty() {
            REMOTE_BUNDLE_URL.to_string()
        } else {
            format!("{REMOTE_BUNDLE_URL}?sha={remote_sha}")
        };
        if let Ok(gz_bytes) = curl_download_bytes(&url, "15") {
            let _ = atomic_write(&bundle_path, &gz_bytes);
            if let Some(ref mb) = remote_meta_bytes {
                let _ = atomic_write(&meta_path, mb);
            }
        }
        if let Ok(site_bytes) = curl_download_bytes(REMOTE_SITE_BUNDLE_URL, "10") {
            let _ = atomic_write(&site_bundle_path, &site_bytes);
        }
    }

    let gz_bytes = fs::read(&bundle_path).map_err(|_| {
        "Repository snapshot unavailable (offline and no cached ~/.cache/infimum-code/repo_bundle.json.gz)".to_string()
    })?;
    let mut snap = load_bundle_from_bytes(&gz_bytes)?;

    if let Ok(site_gz) = fs::read(&site_bundle_path) {
        if let Ok(site_snap) = load_bundle_from_bytes(&site_gz) {
            for (k, v) in site_snap.files {
                snap.files.insert(k, v);
            }
        }
    }

    let arc = Arc::new(snap);
    if let Ok(mut guard) = cell.lock() {
        *guard = Some(CachedRemote {
            snapshot: Arc::clone(&arc),
            checked_at: Instant::now(),
        });
    }
    Ok(arc)
}

fn current_snapshot() -> Result<Arc<RepoSnapshot>, String> {
    if let Some(mono) = detect_local_monorepo_root() {
        return Ok(Arc::new(load_live_local_snapshot(&mono)));
    }
    load_or_sync_remote_snapshot()
}

pub fn list_snapshot(snap: &RepoSnapshot, raw_prefix: &str) -> Value {
    let Some(norm) = normalize_repo_path(raw_prefix) else {
        return json!({ "error": "Invalid path prefix (must not contain '..' or '.')" });
    };
    let pref = if norm.is_empty() {
        String::new()
    } else {
        format!("{norm}/")
    };
    let matching: Vec<&String> = snap
        .files
        .keys()
        .filter(|p| pref.is_empty() || p.starts_with(&pref) || p.as_str() == norm.as_str())
        .collect();
    if matching.is_empty() {
        return json!({ "error": format!("No files found under prefix '{norm}'") });
    }
    let short_sha: String = snap.sha.chars().take(12).collect();
    let header = format!(
        "[infimum@{short_sha} | prefix='{}' | {} file(s)]",
        if norm.is_empty() { "/" } else { &norm },
        matching.len()
    );
    if matching.len() <= 80 {
        let body = matching
            .iter()
            .map(|s| s.as_str())
            .collect::<Vec<_>>()
            .join("\n");
        return json!({
            "sha": short_sha,
            "count": matching.len(),
            "output": format!("{header}\n{body}"),
        });
    }
    let mut dirs: BTreeMap<String, usize> = BTreeMap::new();
    let mut direct_files: Vec<&str> = Vec::new();
    for p in &matching {
        let rest = if pref.is_empty() {
            p.as_str()
        } else {
            &p[pref.len()..]
        };
        if let Some(slash) = rest.find('/') {
            let sub = &rest[..slash];
            let dir_key = if pref.is_empty() {
                format!("{sub}/")
            } else {
                format!("{pref}{sub}/")
            };
            *dirs.entry(dir_key).or_insert(0) += 1;
        } else {
            direct_files.push(p.as_str());
        }
    }
    let mut lines = vec![header];
    for (d, count) in dirs {
        lines.push(format!("  {d} ({count} files)"));
    }
    for f in direct_files {
        lines.push(format!("  {f}"));
    }
    lines.push(
        "Tip: call list_infimum_repo(prefix='crates/infimum-lang/src') to drill into a subdirectory."
            .to_string(),
    );
    json!({
        "sha": short_sha,
        "count": matching.len(),
        "output": lines.join("\n"),
    })
}

pub fn read_snapshot(
    snap: &RepoSnapshot,
    raw_path: &str,
    start_line: Option<usize>,
    end_line: Option<usize>,
) -> Value {
    let Some(p) = normalize_repo_path(raw_path).filter(|s| !s.is_empty()) else {
        return json!({ "error": "Invalid repository path (must be relative without '..')" });
    };
    let Some(content) = snap.files.get(&p) else {
        let hints: Vec<&str> = snap
            .files
            .keys()
            .filter(|k| k.ends_with(&format!("/{p}")) || k.contains(&p))
            .take(8)
            .map(String::as_str)
            .collect();
        return json!({
            "error": format!("File '{p}' not found in repository snapshot"),
            "suggestions": hints,
        });
    };
    let lines: Vec<&str> = content.lines().collect();
    let total = lines.len();
    let s = start_line.unwrap_or(1).max(1);
    let req_end = end_line.unwrap_or_else(|| (s + MAX_READ_LINES - 1).min(total));
    let e = req_end.max(s).min(s + MAX_READ_LINES - 1).min(total);
    let mut numbered = Vec::with_capacity(e.saturating_sub(s) + 1);
    if total > 0 && s <= total {
        for (idx, line) in lines[(s - 1)..e].iter().enumerate() {
            numbered.push(format!("{}: {line}", s + idx));
        }
    }
    let short_sha: String = snap.sha.chars().take(12).collect();
    let mut out = format!("[{p} lines {s}-{e} of {total} | sha {short_sha}]\n{}", numbered.join("\n"));
    if e < total && end_line.is_none() {
        out.push_str(&format!(
            "\n... [truncated at {MAX_READ_LINES} lines; pass start_line={}, end_line={} to continue]",
            e + 1,
            (e + MAX_READ_LINES).min(total)
        ));
    }
    json!({
        "path": p,
        "sha": short_sha,
        "start_line": s,
        "end_line": e,
        "total_lines": total,
        "content": out,
    })
}

pub fn search_snapshot(
    snap: &RepoSnapshot,
    query: &str,
    raw_prefix: &str,
    case_sensitive: bool,
    max_results: usize,
) -> Value {
    if query.is_empty() {
        return json!({ "error": "query must be non-empty" });
    }
    let Some(norm) = normalize_repo_path(raw_prefix) else {
        return json!({ "error": "Invalid path prefix" });
    };
    let pref = if norm.is_empty() {
        String::new()
    } else {
        format!("{norm}/")
    };
    let limit = max_results.clamp(1, 100);
    let needle = if case_sensitive {
        query.to_string()
    } else {
        query.to_lowercase()
    };

    let mut matches = Vec::new();
    let mut total_hits = 0usize;
    for (path, text) in &snap.files {
        if !pref.is_empty() && !path.starts_with(&pref) && path != &norm {
            continue;
        }
        let hay_full = if case_sensitive {
            std::borrow::Cow::Borrowed(text.as_str())
        } else {
            std::borrow::Cow::Owned(text.to_lowercase())
        };
        if !hay_full.contains(&needle) {
            continue;
        }
        for (i, line) in text.lines().enumerate() {
            let line_hit = if case_sensitive {
                line.contains(&needle)
            } else {
                line.to_lowercase().contains(&needle)
            };
            if line_hit {
                total_hits += 1;
                if matches.len() < limit {
                    let snippet: String = if line.chars().count() > 180 {
                        format!("{}...", line.chars().take(177).collect::<String>())
                    } else {
                        line.to_string()
                    };
                    matches.push(format!("{path}:{}: {snippet}", i + 1));
                }
            }
        }
    }
    let short_sha: String = snap.sha.chars().take(12).collect();
    if matches.is_empty() {
        return json!({
            "sha": short_sha,
            "total_hits": 0,
            "output": format!("No matches for '{query}'"),
        });
    }
    let mut out = format!(
        "[search '{query}' | {}/{} match(es) | sha {short_sha}]\n{}",
        matches.len(),
        total_hits,
        matches.join("\n")
    );
    if total_hits > matches.len() {
        out.push_str(&format!(
            "\n... ({} more match(es) omitted; narrow prefix or raise max_results)",
            total_hits - matches.len()
        ));
    }
    json!({
        "sha": short_sha,
        "total_hits": total_hits,
        "output": out,
    })
}

pub fn tool_list_infimum_repo(args: &Value) -> Value {
    let prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
    match current_snapshot() {
        Ok(snap) => list_snapshot(&snap, prefix),
        Err(e) => json!({ "error": e }),
    }
}

pub fn tool_read_infimum_repo(args: &Value) -> Value {
    let path = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
    let start_line = args
        .get("start_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    let end_line = args
        .get("end_line")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize);
    match current_snapshot() {
        Ok(snap) => read_snapshot(&snap, path, start_line, end_line),
        Err(e) => json!({ "error": e }),
    }
}

pub fn tool_search_infimum_repo(args: &Value) -> Value {
    let query = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
    let prefix = args.get("prefix").and_then(|v| v.as_str()).unwrap_or("");
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let max_results = args
        .get("max_results")
        .and_then(|v| v.as_u64())
        .map(|v| v as usize)
        .unwrap_or(40);
    match current_snapshot() {
        Ok(snap) => search_snapshot(&snap, query, prefix, case_sensitive, max_results),
        Err(e) => json!({ "error": e }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use flate2::write::GzEncoder;
    use flate2::Compression;
    use std::io::Write;

    fn sample_snapshot() -> RepoSnapshot {
        let mut files = BTreeMap::new();
        files.insert("LICENSE".to_string(), "GNU AGPLv3\nLine 2\nLine 3\n".to_string());
        files.insert(
            "crates/infimum-lang/src/verify.rs".to_string(),
            "fn prove_z3() {}\n// Z3 bounds check\nfn other() {}\n".to_string(),
        );
        files.insert(
            "site/src/worker.js".to_string(),
            "export default { fetch() {} }\n".to_string(),
        );
        RepoSnapshot {
            sha: "abcdef1234567890".to_string(),
            files,
        }
    }

    #[test]
    fn gzip_bundle_roundtrip_and_queries() {
        let payload = json!({
            "version": 1,
            "repo": "aminadav-ai/infimum",
            "sha": "1234567890abcdef",
            "files": {
                "LICENSE": "AGPL-3.0\n",
                "crates/infimum-lang/Cargo.toml": "[package]\nname = \"infimum-lang\"\n",
            }
        });
        let mut enc = GzEncoder::new(Vec::new(), Compression::best());
        enc.write_all(payload.to_string().as_bytes()).unwrap();
        let gz = enc.finish().unwrap();

        let snap = load_bundle_from_bytes(&gz).expect("gunzip bundle");
        assert_eq!(snap.sha, "1234567890abcdef");
        assert_eq!(snap.files.len(), 2);
        assert!(snap.files.contains_key("LICENSE"));
    }

    #[test]
    fn path_traversal_is_rejected() {
        let snap = sample_snapshot();
        assert_eq!(normalize_repo_path("../etc/passwd"), None);
        assert_eq!(normalize_repo_path("a/../b"), None);
        assert_eq!(normalize_repo_path("/LICENSE").as_deref(), Some("LICENSE"));
        let r = read_snapshot(&snap, "../etc/passwd", None, None);
        assert!(r.get("error").is_some());
        let r2 = read_snapshot(&snap, "/etc/passwd", None, None);
        assert!(r2.get("error").is_some());
    }

    #[test]
    fn list_read_and_search_work() {
        let snap = sample_snapshot();
        let listed = list_snapshot(&snap, "crates/infimum-lang");
        assert_eq!(listed["count"], 1);
        assert!(listed["output"].as_str().unwrap().contains("verify.rs"));

        let read = read_snapshot(&snap, "LICENSE", Some(2), Some(3));
        assert_eq!(read["start_line"], 2);
        assert_eq!(read["end_line"], 3);
        let c = read["content"].as_str().unwrap();
        assert!(c.contains("2: Line 2"));
        assert!(c.contains("3: Line 3"));
        assert!(!c.contains("1: GNU"));

        let search = search_snapshot(&snap, "z3", "crates/infimum-lang", false, 10);
        assert_eq!(search["total_hits"], 2);
        let out = search["output"].as_str().unwrap();
        assert!(out.contains("verify.rs:1:"));
        assert!(out.contains("verify.rs:2:"));
    }

    #[test]
    fn detects_local_monorepo_root() {
        let root = detect_local_monorepo_root().expect("should detect local monorepo");
        assert!(root.join("LICENSE").is_file());
        assert!(root.join("crates/infimum-lang/Cargo.toml").is_file());
    }
}
