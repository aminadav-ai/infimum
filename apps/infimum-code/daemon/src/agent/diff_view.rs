//! Git status and unified diff helpers for the Studio UI Diff viewer.
//!
//! Supports both single-repo workspaces (e.g. `~/Projects/my-app`) and
//! multi-repo meta-workspaces (e.g. `~/ProjectInfimum` containing `infimum/`
//! and `infimum-site/`).

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::process::Command;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DiffFileEntry {
    /// Single-char badge: `M`, `A`, `D`, `R`, `?`
    pub status: char,
    /// Workspace-relative path (prefixed with subrepo name for nested repos).
    pub path: String,
    pub added: usize,
    pub deleted: usize,
}

#[derive(Clone, Debug)]
pub struct DiffPage {
    pub total_lines: usize,
    pub start_line: usize,
    pub lines: Vec<String>,
    pub truncated: bool,
}

/// Discovers git repositories inside `workspace_root`: either the root itself,
/// or immediate depth-1 subdirectories containing `.git`.
pub fn discover_git_repos(workspace_root: &Path) -> Vec<(String, PathBuf)> {
    if workspace_root.join(".git").exists() {
        return vec![(String::new(), workspace_root.to_path_buf())];
    }
    let mut repos = Vec::new();
    if let Ok(entries) = std::fs::read_dir(workspace_root) {
        let mut children: Vec<PathBuf> = entries
            .filter_map(|e| e.ok().map(|x| x.path()))
            .filter(|p| p.is_dir())
            .collect();
        children.sort();
        for child in children {
            if child.join(".git").exists() {
                if let Some(name) = child.file_name().and_then(|n| n.to_str()) {
                    if !name.starts_with('.') {
                        repos.push((name.to_string(), child));
                    }
                }
            }
        }
    }
    repos
}

fn collect_numstat(repo_dir: &Path, staged: bool) -> BTreeMap<String, (usize, usize)> {
    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir).arg("diff").arg("--numstat");
    if staged {
        cmd.arg("--cached");
    }
    let mut map = BTreeMap::new();
    let Ok(out) = cmd.output() else {
        return map;
    };
    if !out.status.success() {
        return map;
    }
    for line in String::from_utf8_lossy(&out.stdout).lines() {
        let mut parts = line.splitn(3, '\t');
        let add = parts
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        let del = parts
            .next()
            .and_then(|s| s.parse::<usize>().ok())
            .unwrap_or(0);
        if let Some(path) = parts.next() {
            let entry = map.entry(path.to_string()).or_insert((0, 0));
            entry.0 += add;
            entry.1 += del;
        }
    }
    map
}

fn map_porcelain_badge(x: char, y: char) -> char {
    if x == '?' && y == '?' {
        '?'
    } else if x == 'A' || y == 'A' {
        'A'
    } else if x == 'D' || y == 'D' {
        'D'
    } else if x == 'R' || y == 'R' {
        'R'
    } else {
        'M'
    }
}

/// Lists changed files across the workspace (or nested depth-1 git repos).
pub fn collect_git_status_entries(workspace_root: &Path) -> Vec<DiffFileEntry> {
    let repos = discover_git_repos(workspace_root);
    let mut out_entries = Vec::new();

    for (prefix, repo_dir) in repos {
        let Ok(out) = Command::new("git")
            .current_dir(&repo_dir)
            .args(["status", "--porcelain=v1", "-uall"])
            .output()
        else {
            continue;
        };
        if !out.status.success() {
            continue;
        }
        let mut stats = collect_numstat(&repo_dir, false);
        for (k, (a, d)) in collect_numstat(&repo_dir, true) {
            let e = stats.entry(k).or_insert((0, 0));
            e.0 += a;
            e.1 += d;
        }

        for line in String::from_utf8_lossy(&out.stdout).lines() {
            if line.len() < 4 {
                continue;
            }
            let bytes = line.as_bytes();
            let x = bytes[0] as char;
            let y = bytes[1] as char;
            let badge = map_porcelain_badge(x, y);
            let mut raw_path = line[3..].trim().to_string();
            // Rename format: `old -> new`
            if let Some((_, rhs)) = raw_path.split_once(" -> ") {
                raw_path = rhs.to_string();
            }
            // Strip surrounding quotes git may emit for spaces
            if raw_path.starts_with('"') && raw_path.ends_with('"') && raw_path.len() >= 2 {
                raw_path = raw_path[1..raw_path.len() - 1].to_string();
            }

            let (mut added, deleted) = stats.get(&raw_path).copied().unwrap_or((0, 0));
            if badge == '?' && added == 0 {
                if let Ok(content) = std::fs::read_to_string(repo_dir.join(&raw_path)) {
                    added = content.lines().count();
                }
            }

            let display_path = if prefix.is_empty() {
                raw_path
            } else {
                format!("{}/{}", prefix, raw_path)
            };
            out_entries.push(DiffFileEntry {
                status: badge,
                path: display_path,
                added,
                deleted,
            });
            if out_entries.len() >= 120 {
                break;
            }
        }
    }

    out_entries
}

fn synthetic_untracked_diff(rel_path: &str, full_path: &Path) -> String {
    match std::fs::read_to_string(full_path) {
        Ok(content) => {
            let mut out = String::new();
            let lines: Vec<&str> = content.lines().collect();
            out.push_str(&format!("diff --git a/{0} b/{0}\n", rel_path));
            out.push_str("new file\n");
            out.push_str("--- /dev/null\n");
            out.push_str(&format!("+++ b/{}\n", rel_path));
            out.push_str(&format!("@@ -0,0 +1,{} @@\n", lines.len()));
            for (idx, line) in lines.iter().enumerate() {
                if idx >= 400 {
                    out.push_str(&format!(
                        "... ({} more lines truncated)\n",
                        lines.len() - idx
                    ));
                    break;
                }
                out.push('+');
                out.push_str(line);
                out.push('\n');
            }
            out
        }
        Err(_) => format!(
            "diff --git a/{0} b/{0}\n(binary or unreadable untracked file)\n",
            rel_path
        ),
    }
}

fn run_git_diff_for_repo(repo_dir: &Path, file_in_repo: Option<&str>) -> String {
    let mut out = String::new();

    // Check if HEAD exists (brand new repo with 0 commits has no HEAD).
    let has_head = Command::new("git")
        .current_dir(repo_dir)
        .args(["rev-parse", "--verify", "HEAD"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false);

    let mut cmd = Command::new("git");
    cmd.current_dir(repo_dir)
        .args(["diff", "--no-color", "-U3"]);
    if has_head {
        cmd.arg("HEAD");
    }
    if let Some(f) = file_in_repo {
        cmd.arg("--").arg(f);
    }
    if let Ok(res) = cmd.output() {
        if res.status.success() {
            out.push_str(&String::from_utf8_lossy(&res.stdout));
        }
    }

    // If single file requested and git diff was empty, it may be an untracked file.
    if out.trim().is_empty() {
        if let Some(f) = file_in_repo {
            let full = repo_dir.join(f);
            if full.is_file() {
                out = synthetic_untracked_diff(f, &full);
            }
        }
    }
    out
}

/// Computes unified diff for a specific workspace-relative file, or all changed
/// files across discovered repos if `file_rel` is `None` or empty.
pub fn collect_git_diff_page(
    workspace_root: &Path,
    file_rel: Option<&str>,
    start_line: usize,
    max_lines: usize,
) -> DiffPage {
    let repos = discover_git_repos(workspace_root);
    let target = file_rel.map(|s| s.trim()).filter(|s| !s.is_empty());

    let mut combined = String::new();

    if let Some(rel) = target {
        // Security: reject path traversal
        if rel.contains("..") || Path::new(rel).is_absolute() {
            combined.push_str("Invalid relative path.\n");
        } else {
            let mut matched = false;
            for (prefix, repo_dir) in &repos {
                if prefix.is_empty() {
                    combined.push_str(&run_git_diff_for_repo(repo_dir, Some(rel)));
                    matched = true;
                    break;
                } else if let Some(inner) = rel.strip_prefix(&format!("{}/", prefix)) {
                    combined.push_str(&run_git_diff_for_repo(repo_dir, Some(inner)));
                    matched = true;
                    break;
                }
            }
            if !matched {
                combined.push_str("No matching git repository for selected file.\n");
            }
        }
    } else {
        for (prefix, repo_dir) in &repos {
            let part = run_git_diff_for_repo(repo_dir, None);
            if !part.trim().is_empty() {
                if !prefix.is_empty() {
                    combined.push_str(&format!("=== repo: {} ===\n", prefix));
                }
                combined.push_str(&part);
                if !combined.ends_with('\n') {
                    combined.push('\n');
                }
            }
        }
    }

    if combined.trim().is_empty() {
        combined.push_str("(Нет незакоммиченных изменений в git)\n");
    }

    let all_lines: Vec<String> = combined.lines().map(|l| l.to_string()).collect();
    let total_lines = all_lines.len();
    let clamped_start = start_line.min(total_lines);
    let limit = max_lines.clamp(1, 200);
    let end = (clamped_start + limit).min(total_lines);
    let page_lines = all_lines[clamped_start..end].to_vec();

    DiffPage {
        total_lines,
        start_line: clamped_start,
        lines: page_lines,
        truncated: end < total_lines,
    }
}
