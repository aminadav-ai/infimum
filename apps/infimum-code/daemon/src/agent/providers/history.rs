use serde_json::{json, Value};

/// Align the number of kept recent tool turns so the compacted prefix boundary only advances
/// every `stride` tool turns (default 4). Keeping the prefix byte-identical for 3 out of 4 rounds
/// maximizes Gemini Implicit Context Caching and OpenAI Automatic Prompt Caching hit rates.
pub(in crate::agent) fn cache_aligned_keep_recent(
    total_tool_turns: usize,
    min_keep_recent: usize,
    stride: usize,
) -> usize {
    if total_tool_turns <= min_keep_recent || stride <= 1 {
        return min_keep_recent;
    }
    let raw_compact = total_tool_turns - min_keep_recent;
    let aligned_compact = (raw_compact / stride) * stride;
    total_tool_turns - aligned_compact
}

/// Compact large edit/write/patch argument fields inside a JSON object in-place,
/// preserving small metadata fields (`path`, `name`, `action`, `dry_run`, etc.).
fn compact_edit_args_object(fn_name: &str, args: &mut serde_json::Map<String, Value>) -> bool {
    const ARG_THRESHOLD: usize = 300;
    let mut changed = false;
    match fn_name {
        "write_project_file" => {
            if let Some(Value::String(content)) = args.get("content") {
                if content.len() > ARG_THRESHOLD && !content.starts_with("[compacted old ") {
                    let lines = content.lines().count();
                    let chars = content.chars().count();
                    args.insert(
                        "content".to_string(),
                        Value::String(format!(
                            "[compacted old write_project_file content: {chars} chars, {lines} lines]"
                        )),
                    );
                    changed = true;
                }
            }
        }
        "edit_project_file" => {
            for field in ["target", "replacement"] {
                if let Some(Value::String(s)) = args.get(field) {
                    if s.len() > ARG_THRESHOLD && !s.starts_with("[compacted old ") {
                        let chars = s.chars().count();
                        args.insert(
                            field.to_string(),
                            Value::String(format!("[compacted old edit {field}: {chars} chars]")),
                        );
                        changed = true;
                    }
                }
            }
        }
        "replace_symbol" | "replace_ifm_symbol" => {
            if let Some(Value::String(src)) = args.get("new_source") {
                if src.len() > ARG_THRESHOLD && !src.starts_with("[compacted old ") {
                    let lines = src.lines().count();
                    let chars = src.chars().count();
                    args.insert(
                        "new_source".to_string(),
                        Value::String(format!(
                            "[compacted old replace_symbol new_source: {chars} chars, {lines} lines]"
                        )),
                    );
                    changed = true;
                }
            }
        }
        "batch_edit_project_file" => {
            if let Some(Value::Array(edits)) = args.get("edits") {
                let total_chars: usize = edits.iter().map(|e| e.to_string().len()).sum();
                if total_chars > ARG_THRESHOLD
                    && !edits
                        .first()
                        .and_then(|e| e.get("_compacted"))
                        .is_some()
                {
                    let count = edits.len();
                    args.insert(
                        "edits".to_string(),
                        json!([{ "_compacted": format!("omitted {count} old batch edits ({total_chars} chars)") }]),
                    );
                    changed = true;
                }
            }
        }
        "apply_patch" => {
            if let Some(Value::String(patch)) = args.get("patch") {
                if patch.len() > ARG_THRESHOLD && !patch.starts_with("[compacted old ") {
                    let lines = patch.lines().count();
                    let chars = patch.chars().count();
                    args.insert(
                        "patch".to_string(),
                        Value::String(format!(
                            "[compacted old apply_patch diff: {chars} chars, {lines} lines]"
                        )),
                    );
                    changed = true;
                }
            }
        }
        _ => {}
    }
    changed
}

/// Compact older verbose tool payloads in Gemini `contents` to keep context lean on long loops.
///
/// Safety invariants:
/// 1. Never modifies model `functionCall` parts that contain `thoughtSignature` (strictly
///    preserving Gemini 3/4 thinking signatures).
/// 2. For unsigned model `functionCall` parts older than `keep_recent_tool_steps`, compacts only
///    large write/edit/patch bodies (`content`, `new_source`, `patch`, `edits`).
/// 3. Never drops error responses (`error`, `exit_code != 0`, `verified == false`).
/// 4. Keeps the last `keep_recent_tool_steps` tool-response turns completely intact.
pub(in crate::agent) fn compact_old_tool_responses(
    contents: &mut [Value],
    keep_recent_tool_steps: usize,
) {
    let tool_turn_indices: Vec<usize> = contents
        .iter()
        .enumerate()
        .filter_map(|(idx, turn)| {
            if turn.get("role").and_then(|r| r.as_str()) != Some("user") {
                return None;
            }
            let has_fn_resp = turn
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|arr| arr.iter().any(|part| part.get("functionResponse").is_some()))
                .unwrap_or(false);
            if has_fn_resp { Some(idx) } else { None }
        })
        .collect();

    if tool_turn_indices.len() <= keep_recent_tool_steps {
        return;
    }
    let compact_count = tool_turn_indices.len() - keep_recent_tool_steps;
    const CHAR_THRESHOLD: usize = 600;

    for &turn_idx in &tool_turn_indices[..compact_count] {
        // Compact unsigned preceding model `functionCall` edit/write arguments.
        if turn_idx > 0 {
            if let Some(prev_turn) = contents.get_mut(turn_idx - 1) {
                if prev_turn.get("role").and_then(|r| r.as_str()) == Some("model") {
                    if let Some(parts) = prev_turn.get_mut("parts").and_then(|p| p.as_array_mut()) {
                        for part in parts.iter_mut() {
                            // Never touch any part carrying a Gemini `thoughtSignature`!
                            if part.get("thoughtSignature").is_some()
                                || part.get("thought_signature").is_some()
                            {
                                continue;
                            }
                            if let Some(fc) = part.get_mut("functionCall") {
                                let fn_name = fc
                                    .get("name")
                                    .and_then(|n| n.as_str())
                                    .unwrap_or("")
                                    .to_string();
                                if let Some(args_obj) =
                                    fc.get_mut("args").and_then(|a| a.as_object_mut())
                                {
                                    compact_edit_args_object(&fn_name, args_obj);
                                }
                            }
                        }
                    }
                }
            }
        }

        let Some(parts) = contents[turn_idx]
            .get_mut("parts")
            .and_then(|p| p.as_array_mut())
        else {
            continue;
        };
        for part in parts.iter_mut() {
            let Some(fr) = part.get_mut("functionResponse") else {
                continue;
            };
            let fn_name = fr
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let Some(resp) = fr.get_mut("response").and_then(|r| r.as_object_mut()) else {
                continue;
            };
            if resp.contains_key("error")
                || resp
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .map(|c| c != 0)
                    .unwrap_or(false)
                || resp.get("verified").and_then(|v| v.as_bool()) == Some(false)
            {
                continue;
            }
            match fn_name.as_str() {
                "read_project_file" => {
                    if let Some(content) = resp.get("content").and_then(|v| v.as_str()) {
                        if content.len() > CHAR_THRESHOLD && !content.starts_with("[compacted ") {
                            let path = resp
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let start_line =
                                resp.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1);
                            let end_line =
                                resp.get("end_line").and_then(|v| v.as_u64()).unwrap_or(0);
                            let orig_len = content.chars().count();
                            resp.insert(
                                "content".to_string(),
                                Value::String(format!(
                                    "[compacted older tool output: read_project_file {path} lines {start_line}..{end_line}, original {orig_len} chars; call read_project_file again if exact lines are needed]"
                                )),
                            );
                        }
                    }
                }
                "get_file_outline" => {
                    if let Some(outline) = resp.get("outline").and_then(|v| v.as_str()) {
                        if outline.len() > CHAR_THRESHOLD && !outline.starts_with("[compacted ") {
                            let path = resp
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let orig_len = outline.chars().count();
                            resp.insert(
                                "outline".to_string(),
                                Value::String(format!(
                                    "[compacted older tool output: get_file_outline {path}, original {orig_len} chars; call get_file_outline again if exact outline is needed]"
                                )),
                            );
                        }
                    }
                }
                "get_symbol" | "get_ifm_symbol" => {
                    if let Some(src) = resp.get("source").and_then(|v| v.as_str()) {
                        if src.len() > CHAR_THRESHOLD && !src.starts_with("[compacted ") {
                            let path = resp
                                .get("path")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let sym = resp
                                .get("symbol")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let orig_len = src.chars().count();
                            resp.insert(
                                "source".to_string(),
                                Value::String(format!(
                                    "[compacted older tool output: {fn_name} {sym} in {path}, original {orig_len} chars]"
                                )),
                            );
                        }
                    }
                }
                "search_project" => {
                    if let Some(matches) = resp.get("matches").and_then(|v| v.as_str()) {
                        if matches.len() > CHAR_THRESHOLD && !matches.starts_with("[compacted ") {
                            let count = resp
                                .get("matches_shown")
                                .or_else(|| resp.get("count"))
                                .and_then(|v| v.as_u64())
                                .unwrap_or(0);
                            let orig_len = matches.chars().count();
                            resp.insert(
                                "matches".to_string(),
                                Value::String(format!(
                                    "[compacted older tool output: search_project ({count} matches, original {orig_len} chars)]"
                                )),
                            );
                        }
                    }
                }
                "list_project_files" => {
                    if let Some(files) = resp.get("files").and_then(|v| v.as_array()) {
                        if files.len() > 25 {
                            let proj = resp
                                .get("project_dir")
                                .and_then(|v| v.as_str())
                                .unwrap_or("")
                                .to_string();
                            let count = files.len();
                            resp.insert(
                                "files".to_string(),
                                Value::String(format!(
                                    "[compacted older tool output: list_project_files {proj} ({count} files); call list_project_files again if needed]"
                                )),
                            );
                        }
                    }
                }
                "read_infimum_guide" | "read_infimum_std" | "read_infimum_template" => {
                    for key in ["guide", "content", "template", "source"] {
                        if let Some(txt) = resp.get(key).and_then(|v| v.as_str()) {
                            if txt.len() > CHAR_THRESHOLD && !txt.starts_with("[compacted ") {
                                let orig_len = txt.chars().count();
                                resp.insert(
                                    key.to_string(),
                                    Value::String(format!(
                                        "[compacted older tool output: {fn_name}, original {orig_len} chars; call again if needed]"
                                    )),
                                );
                            }
                        }
                    }
                }
                "web_fetch" | "web_search" | "git_tool" | "gh_cli" | "wrangler_cli"
                | "run_project_command" | "cargo_check" => {
                    for key in ["content", "stdout", "answer", "diagnostics_text"] {
                        if let Some(txt) = resp.get(key).and_then(|v| v.as_str()) {
                            if txt.len() > 1_200 && !txt.starts_with("[compacted ") {
                                let preview: String = txt.chars().take(240).collect();
                                let orig_len = txt.chars().count();
                                resp.insert(
                                    key.to_string(),
                                    Value::String(format!(
                                        "{preview}… [compacted older {fn_name} output, original {orig_len} chars]"
                                    )),
                                );
                            }
                        }
                    }
                }
                _ => {}
            }
        }
    }
}

/// Compact older verbose OpenAI-compatible tool messages AND older large edit/write/patch
/// tool_call arguments in assistant messages while keeping the most recent `keep_recent_tool_msgs`
/// tool responses intact.
pub(in crate::agent) fn compact_old_openai_tool_messages(
    messages: &mut [Value],
    keep_recent_tool_msgs: usize,
) {
    let tool_msg_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(i, m)| {
            if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                Some(i)
            } else {
                None
            }
        })
        .collect();
    if tool_msg_indices.len() <= keep_recent_tool_msgs {
        return;
    }
    let compact_count = tool_msg_indices.len() - keep_recent_tool_msgs;
    let last_compacted_tool_idx = tool_msg_indices[compact_count - 1];

    // 1. Compact large edit/write/patch arguments inside assistant `tool_calls` preceding the cutoff.
    for msg in messages[..last_compacted_tool_idx].iter_mut() {
        if msg.get("role").and_then(|r| r.as_str()) != Some("assistant") {
            continue;
        }
        let Some(tool_calls) = msg.get_mut("tool_calls").and_then(|tc| tc.as_array_mut()) else {
            continue;
        };
        for tc in tool_calls.iter_mut() {
            let Some(fn_obj) = tc.get_mut("function").and_then(|f| f.as_object_mut()) else {
                continue;
            };
            let fn_name = fn_obj
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            let Some(args_str) = fn_obj.get("arguments").and_then(|a| a.as_str()) else {
                continue;
            };
            if args_str.len() <= 350 {
                continue;
            }
            if let Ok(Value::Object(mut args_map)) = serde_json::from_str::<Value>(args_str) {
                if compact_edit_args_object(&fn_name, &mut args_map) {
                    if let Ok(compact_json) = serde_json::to_string(&Value::Object(args_map)) {
                        fn_obj.insert("arguments".to_string(), Value::String(compact_json));
                    }
                }
            }
        }
    }

    // 2. Compact old tool response contents.
    for &idx in &tool_msg_indices[..compact_count] {
        let fn_name = messages[idx]
            .get("name")
            .and_then(|n| n.as_str())
            .unwrap_or("")
            .to_string();
        let Some(content) = messages[idx].get("content").and_then(|c| c.as_str()) else {
            continue;
        };
        if content.len() <= 1_200 || content.contains("\"error\"") || content.starts_with("[compacted ") {
            continue;
        }
        if matches!(
            fn_name.as_str(),
            "read_project_file"
                | "get_file_outline"
                | "get_symbol"
                | "get_ifm_symbol"
                | "search_project"
                | "list_project_files"
                | "read_infimum_guide"
                | "read_infimum_std"
                | "read_infimum_template"
                | "web_fetch"
                | "web_search"
                | "cargo_check"
        ) {
            let orig_len = content.chars().count();
            let preview: String = content.chars().take(240).collect();
            if let Some(obj) = messages[idx].as_object_mut() {
                obj.insert(
                    "content".to_string(),
                    Value::String(format!(
                        "{preview}… [compacted older {fn_name} output, original {orig_len} chars]"
                    )),
                );
            }
        }
    }
}

/// Ensure Gemini `contents` starts with a `"user"` turn and merges any consecutive same-role turns.
pub(in crate::agent) fn normalize_contents_roles(contents: &mut Vec<Value>) {
    while let Some(first) = contents.first() {
        if first.get("role").and_then(|r| r.as_str()) == Some("model") {
            contents.remove(0);
        } else {
            break;
        }
    }
    let mut merged: Vec<Value> = Vec::with_capacity(contents.len());
    for item in contents.drain(..) {
        let role = item.get("role").and_then(|r| r.as_str()).unwrap_or("");
        let same_as_prev = merged
            .last()
            .and_then(|m| m.get("role"))
            .and_then(|r| r.as_str())
            == Some(role);
        if same_as_prev {
            if let (Some(prev_parts), Some(new_parts)) = (
                merged
                    .last_mut()
                    .and_then(|m| m.get_mut("parts"))
                    .and_then(|p| p.as_array_mut()),
                item.get("parts").and_then(|p| p.as_array()),
            ) {
                prev_parts.extend(new_parts.iter().cloned());
                continue;
            }
        }
        merged.push(item);
    }
    *contents = merged;
}
