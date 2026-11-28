use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::{
    agent_core_module, calc_cost_milli_ils_via_infimum, format_curl_header_line,
    render_studio_system_prompt, write_ifm_u32_buf, write_private_temp_file, AgentResult,
};
use super::history::{
    cache_aligned_keep_recent, compact_old_tool_responses, normalize_contents_roles,
};
use super::openai::run_openai_compat_native_agent_loop;
use crate::agent::conversations::{
    active_conv_cell, save_active_conversation, take_pending_images, AttachedImage, ConvMessage,
    MAX_ATTACHED_IMAGES,
};
use crate::agent::timeline::{
    reset_live_timeline, timeline_finalize, timeline_finish_step, timeline_push_step,
    timeline_set_round, timeline_set_thought, ASYNC_CANCELLED, ASYNC_GEMINI_PID,
};
use crate::studio_abi::{
    STEP_KIND_THINK, STEP_STATUS_CANCELLED, STEP_STATUS_DONE, STEP_STATUS_ERROR,
};
use crate::agent::tool_specs::{detect_tool_options, gemini_tool_declarations};
use crate::agent::tools::{
    append_play_tag_if_axw_present, describe_tool_ru, describe_tool_step, execute_native_tool,
    resolve_whitelisted_project,
};
use crate::policy::CapabilityPolicy;

#[derive(Debug, Default)]
pub(in crate::agent) struct GeminiTurn {
    pub(in crate::agent) parts: Vec<Value>,
    pub(in crate::agent) in_tokens: u64,
    pub(in crate::agent) cached_tokens: u64,
    pub(in crate::agent) out_tokens: u64,
}

/// Incrementally accumulates Gemini `streamGenerateContent?alt=sse` JSON chunks into a single turn,
/// preserving exact `thought`, `text`, `functionCall` and `thoughtSignature` ordering while exposing
/// live progress counters for the UI.
#[derive(Debug, Default)]
pub(in crate::agent) struct SseTurnAccumulator {
    pub(in crate::agent) parts: Vec<Value>,
    pub(in crate::agent) in_tokens: u64,
    pub(in crate::agent) cached_tokens: u64,
    pub(in crate::agent) out_tokens: u64,
    pub(in crate::agent) thought_chars: usize,
    pub(in crate::agent) answer_chars: usize,
    pub(in crate::agent) fn_calls_seen: usize,
    pub(in crate::agent) chunks_parsed: usize,
    pub(in crate::agent) api_error: Option<String>,
    pub(in crate::agent) raw_non_sse: String,
    pub(in crate::agent) sse_data_buf: String,
}

impl SseTurnAccumulator {
    pub(in crate::agent) fn ingest_line(&mut self, raw_line: &str) -> bool {
        let line = raw_line.trim_end_matches(&['\r', '\n'][..]);
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim_start();
            if payload == "[DONE]" {
                return self.flush_sse_event();
            }
            if !self.sse_data_buf.is_empty() {
                self.sse_data_buf.push('\n');
            }
            self.sse_data_buf.push_str(payload);
            if let Ok(v) = serde_json::from_str::<Value>(&self.sse_data_buf) {
                self.sse_data_buf.clear();
                self.ingest_chunk(&v);
                return true;
            }
            return false;
        }
        if line.is_empty() {
            return self.flush_sse_event();
        }
        if line.starts_with("event:") || line.starts_with("id:") || line.starts_with(':') {
            return false;
        }
        if self.raw_non_sse.len() < 16_384 {
            if !self.raw_non_sse.is_empty() {
                self.raw_non_sse.push('\n');
            }
            self.raw_non_sse.push_str(line);
        }
        false
    }

    pub(in crate::agent) fn flush_sse_event(&mut self) -> bool {
        let payload = self.sse_data_buf.trim();
        if payload.is_empty() {
            self.sse_data_buf.clear();
            return false;
        }
        let parsed = serde_json::from_str::<Value>(payload).ok();
        self.sse_data_buf.clear();
        if let Some(v) = parsed {
            self.ingest_chunk(&v);
            return true;
        }
        false
    }

    pub(in crate::agent) fn ingest_chunk(&mut self, chunk: &Value) {
        self.chunks_parsed += 1;
        if let Some(err) = chunk.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .unwrap_or("API error");
            self.api_error = Some(msg.to_string());
            return;
        }
        if let Some(u) = chunk.get("usageMetadata") {
            let cached = u
                .get("cachedContentTokenCount")
                .and_then(|x| x.as_u64())
                .unwrap_or(0);
            if cached > 0 {
                self.cached_tokens = cached;
            }
            if let Some(p) = u.get("promptTokenCount").and_then(|x| x.as_u64()) {
                // `promptTokenCount` in Gemini includes `cachedContentTokenCount`.
                // Split them so billing charges cached tokens at cached rate.
                self.in_tokens = p.saturating_sub(self.cached_tokens);
            }
            let cand = u.get("candidatesTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            let think = u.get("thoughtsTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            if cand + think > 0 {
                self.out_tokens = cand + think;
            }
        }
        if let Some(parts) = chunk
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|ct| ct.get("parts"))
            .and_then(|p| p.as_array())
        {
            for part in parts {
                self.push_part(part);
            }
        }
    }

    fn push_part(&mut self, part: &Value) {
        if part.get("functionCall").is_some() {
            self.fn_calls_seen += 1;
            self.parts.push(part.clone());
            return;
        }
        let is_thought = part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false);
        let has_sig = part.get("thoughtSignature").is_some();
        if let Some(txt) = part.get("text").and_then(|t| t.as_str()) {
            if is_thought {
                self.thought_chars += txt.chars().count();
            } else {
                self.answer_chars += txt.chars().count();
            }
            // Merge adjacent plain text/thought chunks without overwriting thoughtSignature.
            if !has_sig {
                if let Some(last) = self.parts.last_mut() {
                    let last_is_thought =
                        last.get("thought").and_then(|b| b.as_bool()).unwrap_or(false);
                    let last_has_sig = last.get("thoughtSignature").is_some();
                    let last_has_fc = last.get("functionCall").is_some();
                    if last_is_thought == is_thought && !last_has_sig && !last_has_fc {
                        if let Some(last_txt) = last.get("text").and_then(|t| t.as_str()) {
                            let merged = format!("{last_txt}{txt}");
                            if let Some(obj) = last.as_object_mut() {
                                obj.insert("text".to_string(), Value::String(merged));
                                return;
                            }
                        }
                    }
                }
            }
        }
        self.parts.push(part.clone());
    }
}

/// Full multi-step native agent loop (Google Gemini or OpenAI-compatible providers via `provider::model`)
/// with shared conversation history & real filesystem/proc tools.
pub fn run_native_agent_loop<F>(
    policy: &CapabilityPolicy,
    api_key: &str,
    model: &str,
    prompt: &str,
    active_project: Option<&str>,
    mut set_status: F,
) -> AgentResult
where
    F: FnMut(String),
{
    if let Some((provider, model_id)) = model.split_once("::") {
        return run_openai_compat_native_agent_loop(
            policy,
            api_key,
            provider,
            model_id,
            prompt,
            active_project,
            set_status,
        );
    }
    const GEMINI_HOST: &str = "generativelanguage.googleapis.com";
    let max_agent_steps: usize = policy.max_agent_steps();
    let t0 = Instant::now();
    ASYNC_CANCELLED.store(false, Ordering::SeqCst);
    reset_live_timeline(max_agent_steps as i64);

    // 1. Take any attached screenshot/image and update shared ActiveConversation + save to disk
    let pending_imgs: Vec<AttachedImage> = take_pending_images();
    let effective_prompt = if prompt.trim().is_empty() && !pending_imgs.is_empty() {
        "Проанализируй прикреплённое изображение."
    } else {
        prompt
    };

    let (proj_name, hist_msgs, imported_plan, imported_task) = {
        let mut g = active_conv_cell().lock().unwrap();
        if let Some(p) = active_project {
            if !p.trim().is_empty() {
                g.project = p.trim().to_string();
            }
        }
        if g.title.trim().is_empty() {
            g.title = effective_prompt.replace('\n', " ").chars().take(48).collect();
        }
        let prev = g.messages.clone();
        g.messages.push(ConvMessage {
            role: "user".to_string(),
            content: effective_prompt.to_string(),
            meta: None,
            images: pending_imgs.clone(),
            timeline: None,
        });
        (
            g.project.clone(),
            prev,
            g.imported_plan.clone(),
            g.imported_task.clone(),
        )
    };
    save_active_conversation();

    let Some(proj_dir) = resolve_whitelisted_project(policy, &proj_name) else {
        let reply = "Рабочая папка не выбрана или не добавлена в белый список.\nНажмите **+** рядом с «ПРОЕКТЫ» слева, чтобы выбрать папку — агент имеет доступ строго внутри неё.".to_string();
        return AgentResult {
            elapsed_tenths: (t0.elapsed().as_millis() / 100) as i64,
            cost_milli_ils: 0,
            total_tokens: 0,
            reply,
            timeline: None,
        };
    };

    let dev_mode = policy.is_dev_mode();
    let turbo_mode = policy.is_turbo_mode();
    let sys_prompt =
        render_studio_system_prompt(&proj_dir, &imported_plan, &imported_task, dev_mode, turbo_mode);
    let tool_opts = detect_tool_options(&proj_dir, dev_mode);
    let tools_decl = json!([gemini_tool_declarations(tool_opts)]);

    // Build Gemini `contents` from shared conversation history (last 16 messages, images in last 4) + current prompt
    let mut contents: Vec<Value> = Vec::new();
    let h_start = hist_msgs.len().saturating_sub(16);
    let hist_slice = &hist_msgs[h_start..];
    let img_win_start = hist_slice.len().saturating_sub(4);
    for (idx, m) in hist_slice.iter().enumerate() {
        let g_role = if m.role == "assistant" { "model" } else { "user" };
        let mut parts: Vec<Value> = Vec::new();
        if idx >= img_win_start {
            for im in m.images.iter().take(MAX_ATTACHED_IMAGES) {
                if !im.mime_type.is_empty() && !im.data.is_empty() {
                    parts.push(json!({
                        "inlineData": {
                            "mimeType": im.mime_type,
                            "data": im.data
                        }
                    }));
                }
            }
        }
        if !m.content.trim().is_empty() {
            parts.push(json!({ "text": m.content }));
        }
        if !parts.is_empty() {
            contents.push(json!({
                "role": g_role,
                "parts": parts
            }));
        }
    }
    let mut cur_parts: Vec<Value> = Vec::new();
    for im in &pending_imgs {
        if !im.mime_type.is_empty() && !im.data.is_empty() {
            cur_parts.push(json!({
                "inlineData": {
                    "mimeType": im.mime_type,
                    "data": im.data
                }
            }));
        }
    }
    if !effective_prompt.trim().is_empty() {
        cur_parts.push(json!({ "text": effective_prompt }));
    }
    contents.push(json!({
        "role": "user",
        "parts": cur_parts
    }));
    normalize_contents_roles(&mut contents);

    let mut total_in_tok: u64 = 0;
    let mut total_cached_tok: u64 = 0;
    let mut total_out_tok: u64 = 0;
    let mut final_reply = String::new();
    let mut tool_steps = 0usize;
    let mut tool_rounds_done = 0usize;
    let mut action_log: Vec<String> = Vec::new();

    let pid = std::process::id();
    let req_path = PathBuf::from(format!("/tmp/infimum_web_agent_{pid}.json"));
    let mut hdr_cfg = String::from("header = \"Content-Type: application/json\"\n");
    match format_curl_header_line("x-goog-api-key", api_key) {
        Ok(line) => hdr_cfg.push_str(&line),
        Err(e) => {
            return AgentResult {
                reply: e,
                elapsed_tenths: 1,
                cost_milli_ils: 0,
                total_tokens: 0,
                timeline: Some(timeline_finalize(1)),
            };
        }
    }
    let url = format!("https://{GEMINI_HOST}/v1beta/models/{model}:streamGenerateContent?alt=sse");

    let mut include_thoughts = true;
    for step in 0..max_agent_steps {
        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break;
        }
        let is_last = step == max_agent_steps - 1;
        timeline_set_round((step + 1) as i64);
        let think_title = if step == 0 {
            "Анализ задачи и планирование".to_string()
        } else if is_last {
            "Формирование итогового ответа".to_string()
        } else {
            format!("Раунд {}: обдумывание следующих шагов", step + 1)
        };
        let think_step_idx = timeline_push_step(STEP_KIND_THINK, think_title, model.to_string());
        set_status(if step == 0 {
            format!("Шаг 1/{max_agent_steps}: анализирую ({model})…")
        } else if is_last {
            format!("Шаг {max_agent_steps}/{max_agent_steps}: подвожу итог…")
        } else {
            format!("Шаг {}/{max_agent_steps}: думаю ({model})…", step + 1)
        });

        // Cache-aligned non-destructive compaction: advances prefix compaction every 4 rounds
        // so Gemini Implicit Context Caching hits on 3 out of 4 consecutive rounds.
        let mut req_contents = contents.clone();
        let keep_recent = cache_aligned_keep_recent(tool_rounds_done, 4, 4);
        compact_old_tool_responses(&mut req_contents, keep_recent);

        let build_req_body = |with_thoughts: bool| -> Value {
            let mut b = json!({
                "systemInstruction": { "parts": [{ "text": sys_prompt }] },
                "contents": req_contents,
                "tools": tools_decl,
            });
            if let Some(obj) = b.as_object_mut() {
                if with_thoughts {
                    obj.insert(
                        "generationConfig".to_string(),
                        json!({ "thinkingConfig": { "includeThoughts": true } }),
                    );
                }
                if is_last {
                    obj.insert(
                        "toolConfig".to_string(),
                        json!({ "functionCallingConfig": { "mode": "NONE" } }),
                    );
                }
            }
            b
        };

        write_private_temp_file(&req_path, &build_req_body(include_thoughts).to_string());

        let mut turn_res = call_gemini_sse_with_retry(
            &url,
            &hdr_cfg,
            &req_path,
            model,
            step + 1,
            max_agent_steps,
            &mut set_status,
        );

        // Graceful fallback if the selected model rejects `thinkingConfig`
        if include_thoughts {
            if let Err(ref e) = turn_res {
                let low = e.to_lowercase();
                if low.contains("thinkingconfig")
                    || low.contains("includethoughts")
                    || low.contains("thinking")
                {
                    include_thoughts = false;
                    write_private_temp_file(&req_path, &build_req_body(false).to_string());
                    turn_res = call_gemini_sse_with_retry(
                        &url,
                        &hdr_cfg,
                        &req_path,
                        model,
                        step + 1,
                        max_agent_steps,
                        &mut set_status,
                    );
                }
            }
        }

        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            timeline_finish_step(think_step_idx, STEP_STATUS_CANCELLED, None);
            break;
        }

        let turn = match turn_res {
            Ok(t) => {
                timeline_finish_step(think_step_idx, STEP_STATUS_DONE, None);
                t
            }
            Err(err_msg) => {
                timeline_finish_step(
                    think_step_idx,
                    STEP_STATUS_ERROR,
                    Some(err_msg.chars().take(160).collect()),
                );
                final_reply = err_msg;
                break;
            }
        };

        total_in_tok += turn.in_tokens;
        total_cached_tok += turn.cached_tokens;
        total_out_tok += turn.out_tokens;

        if turn.parts.is_empty() {
            break;
        }
        let cand_content = json!({
            "role": "model",
            "parts": turn.parts,
        });
        contents.push(cand_content.clone());

        let mut step_text = String::new();
        let mut round_thought = String::new();
        let mut fn_responses: Vec<Value> = Vec::new();
        if let Some(parts) = cand_content.get("parts").and_then(|p| p.as_array()) {
            for part in parts {
                let is_thought = part.get("thought").and_then(|b| b.as_bool()).unwrap_or(false);
                if is_thought {
                    if let Some(txt) = part.get("text").and_then(|t| t.as_str()) {
                        let tt = txt.trim();
                        if !tt.is_empty() {
                            if !round_thought.is_empty() {
                                round_thought.push('\n');
                            }
                            round_thought.push_str(tt);
                        }
                    }
                } else if let Some(txt) = part.get("text").and_then(|t| t.as_str()) {
                    let t = txt.trim();
                    if !t.is_empty() {
                        if !step_text.is_empty() {
                            step_text.push('\n');
                        }
                        step_text.push_str(t);
                    }
                }
            }
            if !round_thought.is_empty() {
                timeline_set_thought(&round_thought);
            }
            for part in parts {
                if let Some(fc) = part.get("functionCall") {
                    if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                        break;
                    }
                    let fn_name = fc.get("name").and_then(|n| n.as_str()).unwrap_or("");
                    let fn_args = fc.get("args").cloned().unwrap_or_else(|| json!({}));
                    let desc = describe_tool_ru(fn_name, &fn_args);
                    let (st_kind, st_title, st_detail) = describe_tool_step(fn_name, &fn_args);
                    let st_idx = timeline_push_step(st_kind, st_title, st_detail);
                    set_status(desc.clone());
                    action_log.push(desc);
                    let res = execute_native_tool(policy, &proj_dir, fn_name, &fn_args, Some(st_idx));
                    let st_final = if res.get("killed_by_user").and_then(|v| v.as_bool()) == Some(true) {
                        STEP_STATUS_CANCELLED
                    } else if res.get("error").is_some()
                        || res.get("timed_out").and_then(|v| v.as_bool()) == Some(true)
                        || res.get("exit_code").and_then(|v| v.as_i64()).map(|c| c != 0).unwrap_or(false)
                        || res.get("verified").and_then(|v| v.as_bool()) == Some(false)
                    {
                        STEP_STATUS_ERROR
                    } else {
                        STEP_STATUS_DONE
                    };
                    let err_tail = res
                        .get("error")
                        .and_then(|v| v.as_str())
                        .map(|s| s.chars().take(160).collect::<String>());
                    timeline_finish_step(st_idx, st_final, err_tail);
                    tool_steps += 1;
                    fn_responses.push(json!({
                        "functionResponse": {
                            "name": fn_name,
                            "response": res,
                        }
                    }));
                }
            }
        }
        if !step_text.is_empty() {
            final_reply = step_text;
        }
        if fn_responses.is_empty() || is_last {
            break;
        }
        if step == max_agent_steps - 2 {
            fn_responses.push(json!({
                "text": "Это был последний доступный шаг инструментов. Подведи итог выполненной работы и ответь пользователю на русском языке."
            }));
        }
        contents.push(json!({
            "role": "user",
            "parts": fn_responses,
        }));
        tool_rounds_done += 1;
    }

    let _ = fs::remove_file(&req_path);

    let recent_log = || -> String {
        if action_log.is_empty() {
            String::new()
        } else {
            let tail: Vec<String> = action_log
                .iter()
                .rev()
                .take(8)
                .rev()
                .map(|s| format!("• {s}"))
                .collect();
            format!("\n\nПоследние шаги:\n{}", tail.join("\n"))
        }
    };

    if ASYNC_CANCELLED.load(Ordering::SeqCst) {
        final_reply = if tool_steps == 0 {
            "Генерация прервана пользователем (запрос к API остановлен).".to_string()
        } else {
            format!(
                "Генерация прервана пользователем после {tool_steps} вызовов инструментов.{}",
                recent_log()
            )
        };
    } else if final_reply.trim().is_empty() {
        final_reply = format!(
            "Выполнено вызовов инструментов: {tool_steps}, но финальный текстовый ответ не пришёл.{}",
            recent_log()
        );
    }

    let elapsed_tenths = (t0.elapsed().as_millis() / 100) as i64;
    let total_tokens = (total_in_tok + total_cached_tok + total_out_tok) as i64;
    let cost_milli_ils = calc_cost_milli_ils_via_infimum(
        "gemini",
        model,
        total_in_tok,
        total_cached_tok,
        total_out_tok,
    );
    let final_timeline = timeline_finalize(elapsed_tenths);
    let saved_timeline = if final_timeline.steps.is_empty() && final_timeline.thought_snippet.is_empty() {
        None
    } else {
        Some(final_timeline)
    };

    let cache_tag = if total_cached_tok > 0 && (total_in_tok + total_cached_tok) > 0 {
        let pct = ((total_cached_tok as f64 / (total_in_tok + total_cached_tok) as f64) * 100.0)
            .round() as u64;
        format!(" (кеш {pct}%)")
    } else {
        String::new()
    };

    append_play_tag_if_axw_present(&mut final_reply, active_project);
    let meta_str = format!(
        "{:.1} с • {:.2} ₪ • {} ток.{} • {model}",
        elapsed_tenths as f64 / 10.0,
        cost_milli_ils as f64 / 1000.0,
        total_tokens,
        cache_tag
    );
    if let Ok(mut g) = active_conv_cell().lock() {
        g.messages.push(ConvMessage {
            role: "assistant".to_string(),
            content: final_reply.clone(),
            meta: Some(meta_str),
            images: Vec::new(),
            timeline: saved_timeline.clone(),
        });
    }
    save_active_conversation();

    AgentResult {
        elapsed_tenths,
        cost_milli_ils,
        total_tokens,
        reply: final_reply,
        timeline: saved_timeline,
    }
}

pub(in crate::agent) fn extract_gemini_error_from_raw(raw: &str) -> Option<String> {
    let trimmed = raw.trim();
    if trimmed.is_empty() {
        return None;
    }
    let (engine, module) = agent_core_module();
    let mut store = wasmi::Store::new(engine, ());
    store.set_fuel(10_000_000).ok()?;
    let linker = wasmi::Linker::new(engine);
    let inst = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
        .ok()?;
    let memory = inst.get_memory(&store, "memory")?;
    crate::core_loader::check_core_abi(&inst, &mut store, 1, "agent_core").ok()?;
    let func = inst
        .get_typed_func::<i32, i32>(&store, "extract_api_error_ifm")
        .ok()?;
    let json_in_word =
        crate::core_loader::get_word_export(&inst, &mut store, "get_json_in_word").ok()?;
    let json_out_word =
        crate::core_loader::get_word_export(&inst, &mut store, "get_json_out_word").ok()?;
    let in_len = {
        let mem = memory.data_mut(&mut store);
        write_ifm_u32_buf(mem, json_in_word, trimmed, 16_384)
    };
    let out_len = func.call(&mut store, in_len as i32).ok()?.max(0) as usize;
    if out_len == 0 {
        return None;
    }
    let mem = memory.data(&store);
    let mut msg = String::with_capacity(out_len);
    for i in 0..out_len.min(2048) {
        let off = (json_out_word + 1 + i) * 4;
        if off + 4 <= mem.len() {
            let cp = i32::from_le_bytes(mem[off..off + 4].try_into().unwrap()) as u32;
            if let Some(ch) = char::from_u32(cp) {
                msg.push(ch);
            }
        }
    }
    if msg.is_empty() { None } else { Some(msg) }
}

pub(in crate::agent) fn is_transient_api_error(msg: &str) -> bool {
    let lower = msg.to_lowercase();
    lower.contains("overloaded")
        || lower.contains("unavailable")
        || lower.contains("internal error")
        || lower.contains("deadline exceeded")
        || lower.contains("try again")
        || lower.contains("503")
        || lower.contains("502")
        || lower.contains("504")
        || lower.contains("500")
}

pub(in crate::agent) fn call_gemini_sse_with_retry<F>(
    url: &str,
    hdr_cfg: &str,
    req_path: &Path,
    model: &str,
    step_num: usize,
    max_steps: usize,
    set_status: &mut F,
) -> Result<GeminiTurn, String>
where
    F: FnMut(String),
{
    const MAX_ATTEMPTS: usize = 2;
    for attempt in 0..MAX_ATTEMPTS {
        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            return Err("Отменено пользователем.".to_string());
        }
        match call_gemini_sse_once(url, hdr_cfg, req_path, model, step_num, max_steps, set_status) {
            Ok(turn) => return Ok(turn),
            Err((msg, retryable)) => {
                if !retryable || attempt + 1 >= MAX_ATTEMPTS || ASYNC_CANCELLED.load(Ordering::SeqCst)
                {
                    return Err(msg);
                }
                set_status(format!(
                    "Шаг {step_num}/{max_steps}: повтор запроса ({model}) после обрыва связи…"
                ));
                for _ in 0..20 {
                    if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                        return Err("Отменено пользователем.".to_string());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(100));
                }
            }
        }
    }
    Err("Не удалось получить ответ от Gemini API.".to_string())
}

pub(in crate::agent) fn call_gemini_sse_once<F>(
    url: &str,
    hdr_cfg: &str,
    req_path: &Path,
    model: &str,
    step_num: usize,
    max_steps: usize,
    set_status: &mut F,
) -> Result<GeminiTurn, (String, bool)>
where
    F: FnMut(String),
{
    let mut child = Command::new("curl")
        .args([
            "-sS",
            "-N",
            "--proto",
            "=https",
            "--connect-timeout",
            "25",
            "--keepalive-time",
            "20",
            "--speed-limit",
            "1",
            "--speed-time",
            "150",
            "--max-time",
            "900",
            "-H",
            "Expect:",
            "--config",
            "-",
            "--data-binary",
            &format!("@{}", req_path.display()),
            url,
        ])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (format!("Не удалось запустить curl для Gemini API: {e}"), false))?;

    ASYNC_GEMINI_PID.store(child.id(), Ordering::SeqCst);
    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(hdr_cfg.as_bytes());
    }

    let stderr_handle = child.stderr.take().map(|mut err_pipe| {
        std::thread::spawn(move || {
            let mut s = String::new();
            let _ = std::io::Read::read_to_string(&mut err_pipe, &mut s);
            s
        })
    });

    let mut acc = SseTurnAccumulator::default();
    let mut last_status_ts = Instant::now();

    if let Some(stdout) = child.stdout.take() {
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        loop {
            line.clear();
            match reader.read_line(&mut line) {
                Ok(0) => break,
                Ok(_) => {
                    if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                        let _ = child.kill();
                        break;
                    }
                    if acc.ingest_line(&line) && last_status_ts.elapsed().as_millis() >= 180 {
                        last_status_ts = Instant::now();
                        if acc.fn_calls_seen > 0 {
                            set_status(format!(
                                "Шаг {step_num}/{max_steps}: готовлю инструмент ({model})…"
                            ));
                        } else if acc.answer_chars > 0 {
                            set_status(format!(
                                "Шаг {step_num}/{max_steps}: пишу ответ ({model}) • {} симв…",
                                acc.answer_chars
                            ));
                        } else if acc.thought_chars > 0 {
                            set_status(format!(
                                "Шаг {step_num}/{max_steps}: думаю ({model}) • {} симв…",
                                acc.thought_chars
                            ));
                        }
                    }
                }
                Err(_) => break,
            }
        }
    }

    let exit_status = child.wait();
    ASYNC_GEMINI_PID.store(0, Ordering::SeqCst);

    let stderr_txt = stderr_handle
        .and_then(|h| h.join().ok())
        .unwrap_or_default()
        .trim()
        .to_string();

    if ASYNC_CANCELLED.load(Ordering::SeqCst) {
        return Err(("Отменено пользователем.".to_string(), false));
    }

    if let Some(err) = acc.api_error {
        let retryable = is_transient_api_error(&err);
        return Err((format!("Gemini API error ({model}): {err}"), retryable));
    }

    if acc.chunks_parsed == 0 {
        if let Some(api_err) = extract_gemini_error_from_raw(&acc.raw_non_sse) {
            let retryable = is_transient_api_error(&api_err);
            return Err((format!("Gemini API error ({model}): {api_err}"), retryable));
        }
        if let Ok(v) = serde_json::from_str::<Value>(acc.raw_non_sse.trim()) {
            acc.ingest_chunk(&v);
            if let Some(err) = acc.api_error {
                let retryable = is_transient_api_error(&err);
                return Err((format!("Gemini API error ({model}): {err}"), retryable));
            }
        }
    }

    let exit_code = exit_status.ok().and_then(|s| s.code()).unwrap_or(-1);
    if acc.chunks_parsed == 0 {
        let retryable =
            matches!(exit_code, 7 | 18 | 28 | 35 | 52 | 56) || acc.raw_non_sse.trim().is_empty();
        let detail = if !stderr_txt.is_empty() {
            format!("Ошибка соединения с Gemini API (curl код {exit_code}): {stderr_txt}")
        } else if !acc.raw_non_sse.trim().is_empty() {
            let preview: String = acc.raw_non_sse.trim().chars().take(240).collect();
            format!("Некорректный ответ Gemini API (curl код {exit_code}): {preview}")
        } else {
            format!("Пустой ответ от Gemini API (curl код {exit_code}; возможно, обрыв сети или таймаут ожидания).")
        };
        return Err((detail, retryable));
    }

    if exit_code != 0 && acc.answer_chars == 0 && acc.fn_calls_seen == 0 {
        let detail = if stderr_txt.is_empty() {
            format!("Поток Gemini API оборвался до получения ответа (curl код {exit_code}).")
        } else {
            format!("Поток Gemini API оборвался до получения ответа (curl код {exit_code}): {stderr_txt}")
        };
        return Err((detail, matches!(exit_code, 18 | 28 | 52 | 56)));
    }

    Ok(GeminiTurn {
        parts: acc.parts,
        in_tokens: acc.in_tokens,
        cached_tokens: acc.cached_tokens,
        out_tokens: acc.out_tokens,
    })
}
