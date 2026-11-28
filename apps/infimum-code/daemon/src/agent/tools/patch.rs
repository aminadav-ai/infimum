//! Atomic multi-file unified-diff patch engine (`apply_patch`).
//! Supports creating, modifying, and deleting files within the project sandbox.
//! Every file and hunk is validated and applied in memory before anything is written to disk.

use serde_json::{json, Value};
use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};

use super::sandbox::{is_jailed_in, resolve_in_project};

#[derive(Debug, Clone)]
enum HunkLine {
    Context(String),
    Remove(String),
    Add(String),
}

#[derive(Debug, Clone)]
struct DiffHunk {
    old_start: usize, // 1-based (0 for newly created files)
    lines: Vec<HunkLine>,
}

#[derive(Debug, Clone)]
struct FilePatch {
    old_path: Option<String>, // None => /dev/null (create)
    new_path: Option<String>, // None => /dev/null (delete)
    hunks: Vec<DiffHunk>,
}

fn strip_diff_path_header(raw: &str) -> Option<String> {
    // Strip optional timestamp after tab (e.g. `--- a/foo.rs\t2026-01-01 ...`)
    let s = raw.split('\t').next().unwrap_or(raw).trim();
    // Strip surrounding quotes if present
    let s = s
        .strip_prefix('"')
        .and_then(|x| x.strip_suffix('"'))
        .unwrap_or(s);
    if s.is_empty() || s == "/dev/null" {
        return None;
    }
    let clean = s
        .strip_prefix("a/")
        .or_else(|| s.strip_prefix("b/"))
        .unwrap_or(s);
    if clean.is_empty() {
        None
    } else {
        Some(clean.to_string())
    }
}

fn parse_hunk_header(line: &str) -> Option<usize> {
    // Format: `@@ -old_start[,old_count] +new_start[,new_count] @@ ...`
    let rest = line.strip_prefix("@@")?;
    let end = rest.find("@@")?;
    let header = rest[..end].trim();
    let old_part = header.split_whitespace().next()?;
    let old_num = old_part
        .strip_prefix('-')?
        .split(',')
        .next()?;
    old_num.parse::<usize>().ok()
}

/// Parse a unified diff (supporting standard `---`/`+++`/`@@` multi-file patches,
/// optional `diff --git` / `index` lines, and `\ No newline at end of file` markers).
fn parse_unified_diff(patch_text: &str) -> Result<Vec<FilePatch>, String> {
    let lines: Vec<&str> = patch_text.lines().collect();
    let mut patches: Vec<FilePatch> = Vec::new();
    let mut i = 0usize;

    while i < lines.len() {
        let line = lines[i];
        // Skip preamble lines (`diff --git ...`, `index ...`, `new file mode ...`, blank lines)
        if let Some(old_raw) = line.strip_prefix("--- ") {
            let Some(new_raw) = lines.get(i + 1).and_then(|l| l.strip_prefix("+++ ")) else {
                return Err(format!(
                    "Malformed unified diff at line {}: '--- ' not followed by '+++ '",
                    i + 1
                ));
            };
            let old_path = strip_diff_path_header(old_raw);
            let new_path = strip_diff_path_header(new_raw);
            if old_path.is_none() && new_path.is_none() {
                return Err(format!(
                    "Malformed unified diff at line {}: both old and new paths are /dev/null",
                    i + 1
                ));
            }
            i += 2;
            let mut hunks: Vec<DiffHunk> = Vec::new();
            while i < lines.len() {
                let l = lines[i];
                if l.starts_with("diff --git ") || l.starts_with("--- ") {
                    break;
                }
                if l.starts_with("@@") {
                    let old_start = parse_hunk_header(l).ok_or_else(|| {
                        format!("Invalid hunk header at line {}: {l}", i + 1)
                    })?;
                    i += 1;
                    let mut hlines: Vec<HunkLine> = Vec::new();
                    while i < lines.len() {
                        let hl = lines[i];
                        if hl.starts_with("@@")
                            || hl.starts_with("diff --git ")
                            || (hl.starts_with("--- ")
                                && i + 1 < lines.len()
                                && lines[i + 1].starts_with("+++ "))
                        {
                            break;
                        }
                        if hl.starts_with("\\ No newline at end of file") {
                            i += 1;
                            continue;
                        }
                        if let Some(rest) = hl.strip_prefix(' ') {
                            hlines.push(HunkLine::Context(rest.to_string()));
                        } else if let Some(rest) = hl.strip_prefix('-') {
                            hlines.push(HunkLine::Remove(rest.to_string()));
                        } else if let Some(rest) = hl.strip_prefix('+') {
                            hlines.push(HunkLine::Add(rest.to_string()));
                        } else if hl.is_empty() {
                            // Some LLMs emit bare empty lines for empty context lines inside a hunk.
                            hlines.push(HunkLine::Context(String::new()));
                        } else {
                            break;
                        }
                        i += 1;
                    }
                    if hlines.is_empty() {
                        return Err(format!(
                            "Empty hunk starting near line {}",
                            i
                        ));
                    }
                    hunks.push(DiffHunk {
                        old_start,
                        lines: hlines,
                    });
                } else {
                    i += 1;
                }
            }
            patches.push(FilePatch {
                old_path,
                new_path,
                hunks,
            });
        } else {
            i += 1;
        }
    }

    if patches.is_empty() {
        return Err(
            "No unified diff headers ('--- ' / '+++ ') found in patch. Provide standard unified diff format."
                .to_string(),
        );
    }
    Ok(patches)
}

fn hunk_matches_at(file_lines: &[String], pos: usize, expected_old: &[&str]) -> bool {
    if pos + expected_old.len() > file_lines.len() {
        return false;
    }
    for (k, exp) in expected_old.iter().enumerate() {
        if file_lines[pos + k] != *exp {
            return false;
        }
    }
    true
}

fn find_hunk_position(
    file_lines: &[String],
    expected_old: &[&str],
    hint_idx: usize,
    min_start: usize,
) -> Result<usize, String> {
    if expected_old.is_empty() {
        return Ok(hint_idx.min(file_lines.len()));
    }
    // 1. Exact hint position
    if hint_idx >= min_start && hunk_matches_at(file_lines, hint_idx, expected_old) {
        return Ok(hint_idx);
    }
    // 2. Search outward around hint_idx within +/- 80 lines
    let max_delta = 80usize;
    for delta in 1..=max_delta {
        if hint_idx >= delta {
            let cand = hint_idx - delta;
            if cand >= min_start && hunk_matches_at(file_lines, cand, expected_old) {
                return Ok(cand);
            }
        }
        let cand = hint_idx + delta;
        if cand >= min_start
            && cand + expected_old.len() <= file_lines.len()
            && hunk_matches_at(file_lines, cand, expected_old)
        {
            return Ok(cand);
        }
    }
    // 3. Global unique match anywhere in file >= min_start
    let mut all_hits = Vec::new();
    if expected_old.len() <= file_lines.len() {
        for pos in min_start..=(file_lines.len() - expected_old.len()) {
            if hunk_matches_at(file_lines, pos, expected_old) {
                all_hits.push(pos);
            }
        }
    }
    if all_hits.len() == 1 {
        return Ok(all_hits[0]);
    }
    if all_hits.len() > 1 {
        return Err(format!(
            "Hunk context matched {} locations (lines {:?}); add more surrounding context lines",
            all_hits.len(),
            all_hits.iter().map(|p| p + 1).collect::<Vec<_>>()
        ));
    }
    // Diagnostic hint with the first non-empty line of expected_old
    let first_non_empty = expected_old
        .iter()
        .map(|s| s.trim())
        .find(|s| !s.is_empty())
        .unwrap_or("");
    let near = if !first_non_empty.is_empty() {
        let mut hits = Vec::new();
        for (idx, l) in file_lines.iter().enumerate() {
            if l.contains(first_non_empty) {
                hits.push(format!("L{}: {}", idx + 1, l.chars().take(100).collect::<String>()));
                if hits.len() >= 3 {
                    break;
                }
            }
        }
        if hits.is_empty() {
            String::new()
        } else {
            format!("; similar lines: {}", hits.join(" | "))
        }
    } else {
        String::new()
    };
    Err(format!(
        "Hunk context not found near line {}{}",
        hint_idx + 1,
        near
    ))
}

fn apply_hunks_to_content(
    rel_path: &str,
    existing: &str,
    hunks: &[DiffHunk],
) -> Result<(String, usize, usize), String> {
    let had_trailing_newline = existing.ends_with('\n');
    let mut file_lines: Vec<String> = existing.lines().map(|l| l.to_string()).collect();
    let mut line_offset: isize = 0;
    let mut min_start: usize = 0;
    let mut total_added = 0usize;
    let mut total_removed = 0usize;

    for (h_idx, hunk) in hunks.iter().enumerate() {
        let expected_old: Vec<&str> = hunk
            .lines
            .iter()
            .filter_map(|hl| match hl {
                HunkLine::Context(s) | HunkLine::Remove(s) => Some(s.as_str()),
                HunkLine::Add(_) => None,
            })
            .collect();
        let replacement_new: Vec<String> = hunk
            .lines
            .iter()
            .filter_map(|hl| match hl {
                HunkLine::Context(s) | HunkLine::Add(s) => Some(s.clone()),
                HunkLine::Remove(_) => None,
            })
            .collect();

        let base_zero = hunk.old_start.saturating_sub(1) as isize;
        let hint_idx = (base_zero + line_offset).max(0) as usize;
        let pos = find_hunk_position(&file_lines, &expected_old, hint_idx, min_start).map_err(
            |reason| format!("{rel_path} hunk #{}: {reason}", h_idx + 1),
        )?;

        let added = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, HunkLine::Add(_)))
            .count();
        let removed = hunk
            .lines
            .iter()
            .filter(|l| matches!(l, HunkLine::Remove(_)))
            .count();
        total_added += added;
        total_removed += removed;

        let new_len = replacement_new.len();
        let old_len = expected_old.len();
        file_lines.splice(pos..pos + old_len, replacement_new);
        line_offset += new_len as isize - old_len as isize;
        min_start = pos + new_len;
    }

    let mut out = file_lines.join("\n");
    if (had_trailing_newline || existing.is_empty()) && !out.is_empty() {
        out.push('\n');
    }
    Ok((out, total_added, total_removed))
}

#[derive(Debug)]
enum PlannedWrite {
    Write { path: PathBuf, rel: String, content: String, created: bool },
    Delete { path: PathBuf, rel: String },
}

pub(in crate::agent) fn tool_apply_patch(default_proj: &Path, args: &Value) -> Value {
    let patch_text = args.get("patch").and_then(|s| s.as_str()).unwrap_or("");
    let dry_run = args
        .get("dry_run")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);

    if patch_text.trim().is_empty() {
        return json!({ "ok": false, "error": "Missing non-empty 'patch' parameter" });
    }

    let file_patches = match parse_unified_diff(patch_text) {
        Ok(fp) => fp,
        Err(e) => return json!({ "ok": false, "error": e }),
    };

    // Stage all file contents in memory first so multiple patches to the same file compose cleanly
    // and any failure aborts atomically before touching disk.
    let mut staged_content: HashMap<PathBuf, Option<String>> = HashMap::new();
    let mut planned: Vec<PlannedWrite> = Vec::new();
    let mut hunks_applied = 0usize;
    let mut lines_added = 0usize;
    let mut lines_removed = 0usize;

    for fp in &file_patches {
        let is_create = fp.old_path.is_none() && fp.new_path.is_some();
        let is_delete = fp.old_path.is_some() && fp.new_path.is_none();
        let rel = fp
            .new_path
            .as_deref()
            .or(fp.old_path.as_deref())
            .unwrap_or("")
            .to_string();
        if rel.is_empty() {
            return json!({ "ok": false, "error": "Empty file path in patch" });
        }
        let abs = resolve_in_project(default_proj, &rel);
        if !is_jailed_in(&abs, default_proj) {
            return json!({
                "ok": false,
                "error": format!("Patch denied: '{}' is outside active project sandbox or sensitive", rel)
            });
        }

        if is_delete {
            if !abs.exists() {
                return json!({
                    "ok": false,
                    "error": format!("Cannot delete '{}': file does not exist", rel)
                });
            }
            if abs.is_dir() {
                return json!({
                    "ok": false,
                    "error": format!("Cannot delete '{}': refusing to delete directory", rel)
                });
            }
            // Validate deletion hunks against current content if provided
            if !fp.hunks.is_empty() {
                let existing = match fs::read_to_string(&abs) {
                    Ok(s) => s,
                    Err(e) => {
                        return json!({ "ok": false, "error": format!("Failed to read '{}': {e}", rel) });
                    }
                };
                match apply_hunks_to_content(&rel, &existing, &fp.hunks) {
                    Ok((_, add, rem)) => {
                        hunks_applied += fp.hunks.len();
                        lines_added += add;
                        lines_removed += rem;
                    }
                    Err(e) => return json!({ "ok": false, "error": e }),
                }
            }
            staged_content.insert(abs.clone(), None);
            planned.push(PlannedWrite::Delete { path: abs, rel });
            continue;
        }

        if is_create {
            if abs.exists() && staged_content.get(&abs).map(|o| o.is_some()).unwrap_or(true) {
                return json!({
                    "ok": false,
                    "error": format!("Cannot create '{}': file already exists (use modify header instead of /dev/null)", rel)
                });
            }
            let (new_content, add, rem) = match apply_hunks_to_content(&rel, "", &fp.hunks) {
                Ok(res) => res,
                Err(e) => return json!({ "ok": false, "error": e }),
            };
            hunks_applied += fp.hunks.len();
            lines_added += add;
            lines_removed += rem;
            staged_content.insert(abs.clone(), Some(new_content.clone()));
            planned.push(PlannedWrite::Write {
                path: abs,
                rel,
                content: new_content,
                created: true,
            });
            continue;
        }

        // Modify existing file (or rename if old_path != new_path)
        let old_rel = fp.old_path.as_deref().unwrap_or(&rel);
        let old_abs = resolve_in_project(default_proj, old_rel);
        if !is_jailed_in(&old_abs, default_proj) {
            return json!({
                "ok": false,
                "error": format!("Patch denied: '{}' is outside active project sandbox or sensitive", old_rel)
            });
        }
        let existing = if let Some(Some(prev)) = staged_content.get(&old_abs) {
            prev.clone()
        } else {
            match fs::read_to_string(&old_abs) {
                Ok(s) => s,
                Err(e) => {
                    return json!({
                        "ok": false,
                        "error": format!("Cannot read '{}' for patching: {e}", old_rel)
                    });
                }
            }
        };
        let (updated, add, rem) = match apply_hunks_to_content(&rel, &existing, &fp.hunks) {
            Ok(res) => res,
            Err(e) => return json!({ "ok": false, "error": e }),
        };
        hunks_applied += fp.hunks.len();
        lines_added += add;
        lines_removed += rem;
        if old_abs != abs {
            staged_content.insert(old_abs.clone(), None);
            planned.push(PlannedWrite::Delete {
                path: old_abs,
                rel: old_rel.to_string(),
            });
        }
        staged_content.insert(abs.clone(), Some(updated.clone()));
        planned.push(PlannedWrite::Write {
            path: abs,
            rel,
            content: updated,
            created: false,
        });
    }

    let mut files_modified = Vec::new();
    let mut files_created = Vec::new();
    let mut files_deleted = Vec::new();

    if !dry_run {
        for op in &planned {
            match op {
                PlannedWrite::Write { path, content, .. } => {
                    if let Some(parent) = path.parent() {
                        if let Err(e) = fs::create_dir_all(parent) {
                            return json!({
                                "ok": false,
                                "error": format!("Failed to create parent directory for {}: {e}", path.display())
                            });
                        }
                    }
                    if let Err(e) = fs::write(path, content) {
                        return json!({
                            "ok": false,
                            "error": format!("Failed to write {}: {e}", path.display())
                        });
                    }
                }
                PlannedWrite::Delete { path, .. } => {
                    if let Err(e) = fs::remove_file(path) {
                        return json!({
                            "ok": false,
                            "error": format!("Failed to delete {}: {e}", path.display())
                        });
                    }
                }
            }
        }
    }

    for op in planned {
        match op {
            PlannedWrite::Write { rel, created: true, .. } => {
                if !files_created.contains(&rel) {
                    files_created.push(rel);
                }
            }
            PlannedWrite::Write { rel, created: false, .. } => {
                if !files_modified.contains(&rel) {
                    files_modified.push(rel);
                }
            }
            PlannedWrite::Delete { rel, .. } => {
                if !files_deleted.contains(&rel) {
                    files_deleted.push(rel);
                }
            }
        }
    }

    json!({
        "ok": true,
        "dry_run": dry_run,
        "files_modified": files_modified,
        "files_created": files_created,
        "files_deleted": files_deleted,
        "hunks_applied": hunks_applied,
        "lines_added": lines_added,
        "lines_removed": lines_removed,
    })
}
