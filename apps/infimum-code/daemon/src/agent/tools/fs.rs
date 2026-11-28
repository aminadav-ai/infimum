//! Filesystem, search, atomic batch-edit, and sandboxed shell command tools.

use crate::policy::CapabilityPolicy;
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::dev_tools::{build_clean_path, run_subprocess_managed, truncate_head_tail};
#[cfg(unix)]
use crate::agent::timeline::{signal_process_group, SIGKILL_NUM, SIGTERM_NUM};
use crate::agent::timeline::{
    timeline_update_step_tail, ASYNC_CANCELLED, ASYNC_CMD_KILL_REQUESTED, ASYNC_CMD_PID,
};

use super::ifm::{resolve_infimum_bin, resolve_z3_bin};
use super::sandbox::{
    collect_files_filtered, is_jailed_in, is_unsafe_fallback_command, resolve_in_project,
};

pub(in crate::agent) const EDITOR_CORE_AXW: &[u8] =
    include_bytes!("../../../../cores/editor_core.ifw");

pub(in crate::agent) fn editor_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("editor_core.ifw", EDITOR_CORE_AXW))
}

/// Find top-N lines in `content` most similar to the first non-empty line of `target`,
/// returning `"L<num>: <line>"` hints so the model can immediately fix near-miss edits.
pub(in crate::agent) fn find_nearest_lines(content: &str, target: &str, max_n: usize) -> Vec<String> {
    let needle = target
        .lines()
        .map(|l| l.trim())
        .find(|l| !l.is_empty())
        .unwrap_or("")
        .to_ascii_lowercase();
    if needle.is_empty() {
        return Vec::new();
    }
    let needle_tokens: Vec<&str> = needle
        .split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|t| t.len() >= 2)
        .collect();
    let mut scored: Vec<(usize, usize, &str)> = Vec::new();
    for (idx, line) in content.lines().enumerate() {
        let low = line.to_ascii_lowercase();
        let mut score = 0usize;
        if low.contains(&needle) {
            score += 100;
        }
        for tok in &needle_tokens {
            if low.contains(tok) {
                score += tok.len();
            }
        }
        if score > 0 {
            scored.push((score, idx + 1, line));
        }
    }
    scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.cmp(&b.1)));
    scored
        .into_iter()
        .take(max_n)
        .map(|(_, ln, l)| format!("L{ln}: {}", l.chars().take(120).collect::<String>()))
        .collect()
}

fn apply_batch_edits_large_fallback(
    existing: &str,
    edits: &[(String, String)],
) -> Result<String, (usize, String)> {
    let mut matches: Vec<(usize, usize, usize, &str)> = Vec::with_capacity(edits.len());
    for (idx, (target, replacement)) in edits.iter().enumerate() {
        if target.is_empty() {
            return Err((idx, "target must not be empty".to_string()));
        }
        let mut iter = existing.match_indices(target);
        let Some((start, _)) = iter.next() else {
            return Err((idx, "Exact target string not found in file".to_string()));
        };
        let extra = iter.count();
        if extra > 0 {
            return Err((
                idx,
                format!(
                    "Target matched {} places; include more surrounding lines to make it unique",
                    extra + 1
                ),
            ));
        }
        matches.push((start, start + target.len(), idx, replacement.as_str()));
    }
    matches.sort_by_key(|m| m.0);
    for w in matches.windows(2) {
        if w[0].1 > w[1].0 {
            return Err((
                w[1].2,
                format!("Edit #{} overlaps/conflicts with edit #{}", w[1].2, w[0].2),
            ));
        }
    }
    let mut out = String::with_capacity(existing.len());
    let mut cursor = 0usize;
    for (start, end, _, repl) in matches {
        out.push_str(&existing[cursor..start]);
        out.push_str(repl);
        cursor = end;
    }
    out.push_str(&existing[cursor..]);
    Ok(out)
}

pub(in crate::agent) fn apply_batch_edits(
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
    let est_out: isize = src_chars.len() as isize
        + edits
            .iter()
            .map(|(t, r)| r.chars().count() as isize - t.chars().count() as isize)
            .sum::<isize>();
    if edits.len() > 64
        || src_chars.len() > 125_000
        || est_out > 125_000
        || pool_needed > 60_000
    {
        return apply_batch_edits_large_fallback(existing, edits);
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
    crate::core_loader::check_core_abi(&inst, &mut store, 1, "editor_core")
        .map_err(|e| (0, e))?;
    let err_code_word = crate::core_loader::get_word_export(&inst, &mut store, "get_err_code_word")
        .map_err(|e| (0, e))?;
    let err_idx_word = crate::core_loader::get_word_export(&inst, &mut store, "get_err_idx_word")
        .map_err(|e| (0, e))?;
    let err_aux_word = crate::core_loader::get_word_export(&inst, &mut store, "get_err_aux_word")
        .map_err(|e| (0, e))?;
    let src_len_word = crate::core_loader::get_word_export(&inst, &mut store, "get_src_len_word")
        .map_err(|e| (0, e))?;
    let edit_count_word =
        crate::core_loader::get_word_export(&inst, &mut store, "get_edit_count_word")
            .map_err(|e| (0, e))?;
    let t_off_word = crate::core_loader::get_word_export(&inst, &mut store, "get_t_off_word")
        .map_err(|e| (0, e))?;
    let t_len_word = crate::core_loader::get_word_export(&inst, &mut store, "get_t_len_word")
        .map_err(|e| (0, e))?;
    let r_off_word = crate::core_loader::get_word_export(&inst, &mut store, "get_r_off_word")
        .map_err(|e| (0, e))?;
    let r_len_word = crate::core_loader::get_word_export(&inst, &mut store, "get_r_len_word")
        .map_err(|e| (0, e))?;
    let pool_buf_word =
        crate::core_loader::get_word_export(&inst, &mut store, "get_pool_buf_word")
            .map_err(|e| (0, e))?;
    let src_buf_word = crate::core_loader::get_word_export(&inst, &mut store, "get_src_buf_word")
        .map_err(|e| (0, e))?;
    let out_buf_word = crate::core_loader::get_word_export(&inst, &mut store, "get_out_buf_word")
        .map_err(|e| (0, e))?;
    let func = inst
        .get_typed_func::<(), i32>(&store, "apply_batch_edits_ifm")
        .map_err(|e| (0, e.to_string()))?;

    {
        let mem = memory.data_mut(&mut store);
        crate::core_loader::put_i32(mem, src_len_word, src_chars.len() as i32);
        crate::core_loader::put_i32(mem, edit_count_word, edits.len() as i32);
        for (i, &cp) in src_chars.iter().enumerate() {
            crate::core_loader::put_i32(mem, src_buf_word + i, cp as i32);
        }
        let mut p_cur = 0usize;
        for (idx, (target, replacement)) in edits.iter().enumerate() {
            let t_off = p_cur;
            let mut t_len = 0usize;
            for ch in target.chars() {
                crate::core_loader::put_i32(mem, pool_buf_word + p_cur, ch as i32);
                p_cur += 1;
                t_len += 1;
            }
            let r_off = p_cur;
            let mut r_len = 0usize;
            for ch in replacement.chars() {
                crate::core_loader::put_i32(mem, pool_buf_word + p_cur, ch as i32);
                p_cur += 1;
                r_len += 1;
            }
            crate::core_loader::put_i32(mem, t_off_word + idx, t_off as i32);
            crate::core_loader::put_i32(mem, t_len_word + idx, t_len as i32);
            crate::core_loader::put_i32(mem, r_off_word + idx, r_off as i32);
            crate::core_loader::put_i32(mem, r_len_word + idx, r_len as i32);
        }
    }

    let out_len = func.call(&mut store, ()).map_err(|e| (0, e.to_string()))?;
    let mem = memory.data(&store);
    let err_code = crate::core_loader::get_i32(mem, err_code_word);
    let err_idx = crate::core_loader::get_i32(mem, err_idx_word).max(0) as usize;
    let err_aux = crate::core_loader::get_i32(mem, err_aux_word).max(0) as usize;

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
        let cp = crate::core_loader::get_i32(mem, out_buf_word + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            out.push(ch);
        }
    }
    Ok(out)
}

pub(in crate::agent) fn tool_list_project_files(default_proj: &Path, args: &Value) -> Value {
    let sub = args
        .get("subdir")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            args.get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
        });
    let dir = sub
        .map(|s| resolve_in_project(default_proj, s))
        .unwrap_or_else(|| default_proj.to_path_buf());
    if !is_jailed_in(&dir, default_proj) {
        return json!({ "error": "Access denied: path is outside the active project sandbox" });
    }
    let pattern = args
        .get("pattern")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let max_depth = args
        .get("max_depth")
        .and_then(|v| v.as_u64())
        .unwrap_or(6)
        .clamp(1, 12) as usize;
    let limit = args
        .get("limit")
        .and_then(|v| v.as_u64())
        .unwrap_or(200)
        .clamp(1, 500) as usize;
    let mut files = Vec::new();
    let mut total_found = 0usize;
    collect_files_filtered(
        &dir,
        &dir,
        &mut files,
        &mut total_found,
        0,
        max_depth,
        limit,
        pattern,
    );
    let truncated = total_found > files.len();
    json!({
        "project_dir": dir.display().to_string(),
        "count": files.len(),
        "total_found": total_found,
        "truncated": truncated,
        "files": files
    })
}

pub(in crate::agent) fn tool_search_project(default_proj: &Path, args: &Value) -> Value {
    let query = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
    let sub = args
        .get("path")
        .and_then(|s| s.as_str())
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            args.get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.trim().is_empty())
        });
    let target_path = sub
        .map(|s| resolve_in_project(default_proj, s))
        .unwrap_or_else(|| default_proj.to_path_buf());
    if query.is_empty() || !is_jailed_in(&target_path, default_proj) {
        return json!({ "error": "Invalid search parameters or path outside active project sandbox" });
    }
    let glob_filter = args
        .get("glob")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let is_regex = args
        .get("is_regex")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let case_sensitive = args
        .get("case_sensitive")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let context_lines = args
        .get("context_lines")
        .and_then(|v| v.as_u64())
        .unwrap_or(0)
        .min(5);
    let mode = args
        .get("mode")
        .and_then(|s| s.as_str())
        .unwrap_or("matches")
        .trim();
    let max_matches = args
        .get("max_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(50)
        .clamp(1, 200) as usize;

    let (search_cwd, search_arg) = if target_path.is_file() {
        let parent = target_path.parent().unwrap_or(default_proj).to_path_buf();
        let fname = target_path
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_else(|| ".".to_string());
        (parent, fname)
    } else {
        (target_path.clone(), ".".to_string())
    };

    let rg_bin = ["/usr/bin/rg", "/usr/local/bin/rg"]
        .iter()
        .map(Path::new)
        .find(|p| p.is_file())
        .map(|p| p.to_path_buf());

    let output_res = if let Some(rg) = rg_bin {
        let mut cmd = Command::new(rg);
        cmd.current_dir(&search_cwd)
            .arg("--no-heading")
            .arg("--color=never")
            .arg("--glob=!.git/**")
            .arg("--glob=!target/**")
            .arg("--glob=!node_modules/**")
            .arg("--glob=!.infimum/**")
            .arg("--glob=!dist/**")
            .arg("--glob=!Cargo.lock")
            .arg("--glob=!package-lock.json")
            .arg("--glob=!.env")
            .arg("--glob=!.env.*")
            .arg("--glob=!*.pem")
            .arg("--glob=!*.key");
        if !is_regex {
            cmd.arg("-F");
        }
        if !case_sensitive {
            cmd.arg("-i");
        }
        if let Some(g) = glob_filter {
            cmd.arg("--glob").arg(g);
        }
        match mode {
            "files" => {
                cmd.arg("-l");
            }
            "count" => {
                cmd.arg("-c");
            }
            _ => {
                cmd.arg("-n");
                if context_lines > 0 {
                    cmd.arg("-C").arg(context_lines.to_string());
                }
            }
        }
        cmd.arg("--").arg(query).arg(&search_arg);
        cmd.output()
    } else {
        let mut cmd = Command::new("grep");
        cmd.current_dir(&search_cwd).args([
            "-rI",
            "--exclude-dir=.git",
            "--exclude-dir=target",
            "--exclude-dir=node_modules",
            "--exclude-dir=.infimum",
            "--exclude-dir=dist",
            "--exclude=Cargo.lock",
            "--exclude=package-lock.json",
            "--exclude=.env",
            "--exclude=.env.*",
            "--exclude=*.pem",
            "--exclude=*.key",
        ]);
        if !is_regex {
            cmd.arg("-F");
        }
        if !case_sensitive {
            cmd.arg("-i");
        }
        if let Some(g) = glob_filter {
            cmd.arg(format!("--include={g}"));
        }
        match mode {
            "files" => {
                cmd.arg("-l");
            }
            "count" => {
                cmd.arg("-c");
            }
            _ => {
                cmd.arg("-n");
                if context_lines > 0 {
                    cmd.arg("-C").arg(context_lines.to_string());
                }
            }
        }
        cmd.arg("--").arg(query).arg(&search_arg);
        cmd.output()
    };

    match output_res {
        Ok(o) => {
            let raw = String::from_utf8_lossy(&o.stdout);
            let mut kept: Vec<&str> = Vec::new();
            let mut bytes_used = 0usize;
            let mut total_lines = 0usize;
            let mut truncated = false;
            const MAX_BYTES: usize = 6500;
            for line in raw.lines() {
                total_lines += 1;
                if kept.len() >= max_matches || bytes_used + line.len() + 1 > MAX_BYTES {
                    truncated = true;
                    continue;
                }
                bytes_used += line.len() + 1;
                kept.push(line);
            }
            json!({
                "ok": true,
                "mode": mode,
                "matches_shown": kept.len(),
                "total_lines": total_lines,
                "truncated": truncated,
                "matches": kept.join("\n"),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_read_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
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
            let with_nums = args
                .get("with_line_numbers")
                .and_then(|v| v.as_bool())
                .unwrap_or(true);
            let s_idx = s.saturating_sub(1).min(lines.len());
            let e_idx = e.min(lines.len()).max(s_idx);
            let capped_end = e_idx.min(s_idx + 500);
            let slice = if with_nums {
                lines[s_idx..capped_end]
                    .iter()
                    .enumerate()
                    .map(|(i, l)| format!("{}| {l}", s_idx + i + 1))
                    .collect::<Vec<_>>()
                    .join("\n")
            } else {
                lines[s_idx..capped_end].join("\n")
            };
            json!({
                "path": p.display().to_string(),
                "start_line": s_idx + 1,
                "end_line": capped_end,
                "total_lines": lines.len(),
                "has_more": capped_end < lines.len(),
                "content": slice
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_edit_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let target = args.get("target").and_then(|s| s.as_str()).unwrap_or("");
    let replacement = args.get("replacement").and_then(|s| s.as_str()).unwrap_or("");
    let replace_all = args
        .get("replace_all")
        .and_then(|v| v.as_bool())
        .unwrap_or(false);
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Edit denied: path is outside active project sandbox or sensitive" });
    }
    if target.is_empty() {
        return json!({ "error": "target must not be empty" });
    }
    match fs::read_to_string(&p) {
        Ok(existing) => {
            let count = existing.matches(target).count();
            if count == 0 {
                let nearest = find_nearest_lines(&existing, target, 3);
                json!({
                    "ok": false,
                    "error": "Exact target string not found in file",
                    "nearest_lines": nearest,
                })
            } else if count > 1 && !replace_all {
                json!({
                    "ok": false,
                    "error": format!("Target matched {count} places; include more surrounding lines or pass replace_all=true")
                })
            } else {
                let updated = if replace_all {
                    existing.replace(target, replacement)
                } else {
                    existing.replacen(target, replacement, 1)
                };
                match fs::write(&p, &updated) {
                    Ok(_) => json!({
                        "ok": true,
                        "replacements": if replace_all { count } else { 1 },
                        "path": p.display().to_string()
                    }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_delete_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if raw.trim().is_empty() || !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Delete denied: path is outside active project sandbox or sensitive" });
    }
    if !p.exists() {
        return json!({ "error": format!("File not found: {}", p.display()) });
    }
    if p.is_dir() {
        return json!({ "error": "Refusing to delete a directory; only regular files are allowed" });
    }
    match fs::remove_file(&p) {
        Ok(_) => json!({ "ok": true, "deleted": p.display().to_string() }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_move_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw_from = args.get("from").and_then(|s| s.as_str()).unwrap_or("");
    let raw_to = args.get("to").and_then(|s| s.as_str()).unwrap_or("");
    let from_p = resolve_in_project(default_proj, raw_from);
    let to_p = resolve_in_project(default_proj, raw_to);
    if raw_from.trim().is_empty()
        || raw_to.trim().is_empty()
        || !is_jailed_in(&from_p, default_proj)
        || !is_jailed_in(&to_p, default_proj)
    {
        return json!({ "error": "Move denied: source or destination is outside active project sandbox or sensitive" });
    }
    if !from_p.is_file() {
        return json!({ "error": format!("Source file not found: {}", from_p.display()) });
    }
    if let Some(parent) = to_p.parent() {
        if let Err(e) = fs::create_dir_all(parent) {
            return json!({ "error": format!("Failed to create destination parent dir: {e}") });
        }
    }
    match fs::rename(&from_p, &to_p) {
        Ok(_) => json!({
            "ok": true,
            "from": from_p.display().to_string(),
            "to": to_p.display().to_string(),
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_batch_edit_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
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

pub(in crate::agent) fn tool_write_project_file(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let content = args.get("content").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Write denied: path is outside active project sandbox or sensitive" });
    }
    if let Some(parent) = p.parent() {
        let _ = fs::create_dir_all(parent);
    }
    match fs::write(&p, content) {
        Ok(_) => json!({
            "ok": true,
            "written_bytes": content.len(),
            "path": p.display().to_string()
        }),
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_run_project_command(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    args: &Value,
    step_idx: Option<usize>,
) -> Value {
    if !policy.allow_proc {
        return json!({ "error": "Proc capability denied (launch with --allow-proc)" });
    }
    let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
    let timeout_secs = args
        .get("timeout_secs")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(5, 300);
    let cwd = args
        .get("cwd")
        .and_then(|s| s.as_str())
        .filter(|s| !s.is_empty())
        .map(|s| resolve_in_project(default_proj, s))
        .unwrap_or_else(|| default_proj.to_path_buf());
    if !is_jailed_in(&cwd, default_proj) {
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
    command.args([
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
                        if cmd_start.elapsed() > Duration::from_secs(timeout_secs) {
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
            let is_userns_blocked = exit_code != 0
                && all_output.is_empty()
                && (bwrap_err.contains("uid map")
                    || bwrap_err.contains("namespace")
                    || bwrap_err.contains("Permission denied")
                    || bwrap_err.contains("Operation not permitted")
                    || bwrap_err.contains("bwrap:"));
            if is_userns_blocked && !killed_by_user && !timed_out {
                if is_unsafe_fallback_command(cmd) {
                    all_output =
                        format!("Sandbox denied potentially unsafe command in fallback mode: {cmd}");
                    exit_code = 1;
                } else {
                    let mut fb_path = build_clean_path();
                    if let Some(p) = infimum_bin.parent() {
                        fb_path = format!("{}:{fb_path}", p.display());
                    }
                    if let Some(p) = z3_bin.as_ref().and_then(|z| z.parent()) {
                        fb_path = format!("{}:{fb_path}", p.display());
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
                    match run_subprocess_managed(fb, timeout_secs, &format!("$ {cmd}")) {
                        Ok((code, out_txt, fb_to)) => {
                            exit_code = code;
                            timed_out = fb_to;
                            if code == -9 && out_txt.contains("[terminated by user]") {
                                killed_by_user = true;
                            }
                            all_output = out_txt.trim_end().to_string();
                        }
                        Err(e) => {
                            exit_code = -1;
                            all_output = e;
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
            let (trunc, was_truncated) = truncate_head_tail(&full, 6000);
            if killed_by_user {
                json!({
                    "exit_code": exit_code,
                    "killed_by_user": true,
                    "truncated": was_truncated,
                    "output": trunc,
                    "note": "Команда принудительно прервана пользователем через кнопку прерывания."
                })
            } else if timed_out {
                json!({
                    "exit_code": exit_code,
                    "timed_out": true,
                    "truncated": was_truncated,
                    "output": trunc,
                    "note": format!("Команда превысила лимит времени ({timeout_secs} с) и была остановлена.")
                })
            } else {
                json!({
                    "exit_code": exit_code,
                    "truncated": was_truncated,
                    "output": trunc,
                })
            }
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}
