use serde_json::{json, Value};
use std::fs;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
use std::time::Instant;

use super::history::{cache_aligned_keep_recent, compact_old_openai_tool_messages};
use super::{
    calc_cost_milli_ils_via_infimum, format_curl_header_line, render_studio_system_prompt,
    stable_prompt_cache_key, write_private_temp_file, AgentResult,
};
use crate::agent::conversations::{
    active_conv_cell, save_active_conversation, take_pending_images, AttachedImage, ConvMessage,
    MAX_ATTACHED_IMAGES,
};
use crate::agent::timeline::{
    reset_live_timeline, timeline_finalize, timeline_finish_step, timeline_push_step,
    timeline_push_thought, timeline_set_round, ASYNC_CANCELLED, ASYNC_GEMINI_PID,
};
use crate::studio_abi::{STEP_STATUS_CANCELLED, STEP_STATUS_DONE, STEP_STATUS_ERROR};
use crate::agent::tool_specs::{detect_tool_options, openai_tool_declarations};
use crate::agent::tools::{
    append_play_tag_if_axw_present, describe_tool_ru, describe_tool_step, execute_native_tool,
    resolve_whitelisted_project,
};
use crate::policy::CapabilityPolicy;

pub(in crate::agent) fn openai_provider_base_url(provider: &str) -> &'static str {
    match provider {
        "openai" => "https://api.openai.com/v1",
        "deepseek" => "https://api.deepseek.com/v1",
        "openrouter" => "https://openrouter.ai/api/v1",
        "groq" => "https://api.groq.com/openai/v1",
        "zai" => "https://api.z.ai/api/coding/paas/v4",
        "mistral" => "https://api.mistral.ai/v1",
        "cerebras" => "https://api.cerebras.ai/v1",
        "nvidia" => "https://integrate.api.nvidia.com/v1",
        _ => "https://openrouter.ai/api/v1",
    }
}

/// Full multi-step OpenAI-compatible agent loop (OpenAI, DeepSeek, OpenRouter, Groq, Z.ai, Mistral)
/// with shared conversation history, prompt caching accounting, and all native workspace tools.
pub(in crate::agent) fn run_openai_compat_native_agent_loop<F>(
    policy: &CapabilityPolicy,
    api_key: &str,
    provider: &str,
    model_id: &str,
    prompt: &str,
    active_project: Option<&str>,
    mut set_status: F,
) -> AgentResult
where
    F: FnMut(String),
{
    let max_openai_steps: usize = policy.max_agent_steps();
    let t0 = Instant::now();
    ASYNC_CANCELLED.store(false, Ordering::SeqCst);
    reset_live_timeline(max_openai_steps as i64);

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
    let tools_decl = openai_tool_declarations(tool_opts);
    let cache_key = stable_prompt_cache_key(&proj_dir, model_id);

    let mut messages: Vec<Value> = vec![json!({
        "role": "system",
        "content": sys_prompt
    })];

    let h_start = hist_msgs.len().saturating_sub(16);
    let hist_slice = &hist_msgs[h_start..];
    let img_win_start = hist_slice.len().saturating_sub(4);
    for (idx, m) in hist_slice.iter().enumerate() {
        let role = if m.role == "assistant" { "assistant" } else { "user" };
        let mut img_parts: Vec<Value> = Vec::new();
        if role == "user" && idx >= img_win_start {
            for img in m.images.iter().take(MAX_ATTACHED_IMAGES) {
                if !img.mime_type.is_empty() && !img.data.is_empty() {
                    img_parts.push(json!({
                        "type": "image_url",
                        "image_url": { "url": format!("data:{};base64,{}", img.mime_type, img.data) }
                    }));
                }
            }
        }
        if !img_parts.is_empty() {
            if !m.content.trim().is_empty() {
                img_parts.push(json!({ "type": "text", "text": m.content }));
            }
            messages.push(json!({ "role": role, "content": img_parts }));
        } else if !m.content.trim().is_empty() {
            messages.push(json!({ "role": role, "content": m.content }));
        }
    }

    let mut cur_imgs: Vec<Value> = Vec::new();
    for img in pending_imgs.iter().take(MAX_ATTACHED_IMAGES) {
        if !img.mime_type.is_empty() && !img.data.is_empty() {
            cur_imgs.push(json!({
                "type": "image_url",
                "image_url": { "url": format!("data:{};base64,{}", img.mime_type, img.data) }
            }));
        }
    }
    if !cur_imgs.is_empty() {
        if !effective_prompt.trim().is_empty() {
            cur_imgs.push(json!({ "type": "text", "text": effective_prompt }));
        }
        messages.push(json!({ "role": "user", "content": cur_imgs }));
    } else {
        messages.push(json!({ "role": "user", "content": effective_prompt }));
    }

    let base_url = openai_provider_base_url(provider);
    let url = format!("{}/chat/completions", base_url.trim_end_matches('/'));
    let display_model = format!("{provider}:{model_id}");

    let mut total_in_tok: u64 = 0;
    let mut total_cached_tok: u64 = 0;
    let mut total_out_tok: u64 = 0;
    let mut final_reply = String::new();
    let mut tool_steps: usize = 0;
    let mut action_log: Vec<String> = Vec::new();

    for step in 0..max_openai_steps {
        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break;
        }
        let is_last_step = step + 1 >= max_openai_steps;
        timeline_set_round((step + 1) as i64);
        set_status(format!(
            "Шаг {}/{max_openai_steps}: думаю ({display_model})…",
            step + 1
        ));

        let mut req_messages = messages.clone();
        let tool_msg_count = req_messages
            .iter()
            .filter(|m| m.get("role").and_then(|r| r.as_str()) == Some("tool"))
            .count();
        let keep_recent = cache_aligned_keep_recent(tool_msg_count, 4, 4);
        compact_old_openai_tool_messages(&mut req_messages, keep_recent);

        let mut body = if is_last_step {
            req_messages.push(json!({
                "role": "user",
                "content": "Подведи краткий итог выполненных изменений и ответь пользователю на русском языке."
            }));
            json!({
                "model": model_id,
                "messages": req_messages,
                "stream": false,
            })
        } else {
            json!({
                "model": model_id,
                "messages": req_messages,
                "tools": tools_decl,
                "tool_choice": "auto",
                "stream": false,
            })
        };
        if provider == "openai" {
            if let Some(obj) = body.as_object_mut() {
                obj.insert("prompt_cache_key".to_string(), Value::String(cache_key.clone()));
            }
        }

        let pid_self = std::process::id();
        let req_path = PathBuf::from(format!("/tmp/infimum_web_oai_req_{pid_self}.json"));
        write_private_temp_file(&req_path, &body.to_string());
        let mut hdrs = String::from("header = \"Content-Type: application/json\"\n");
        let auth_line = match format_curl_header_line("Authorization", &format!("Bearer {api_key}")) {
            Ok(line) => line,
            Err(e) => {
                let _ = fs::remove_file(&req_path);
                return AgentResult {
                    reply: e,
                    elapsed_tenths: 1,
                    cost_milli_ils: 0,
                    total_tokens: 0,
                    timeline: Some(timeline_finalize(1)),
                };
            }
        };
        hdrs.push_str(&auth_line);
        if provider == "openrouter" {
            hdrs.push_str("header = \"HTTP-Referer: https://infimum-lang.org\"\n");
            hdrs.push_str("header = \"X-Title: Infimum Code\"\n");
        }

        let mut parsed_resp: Option<Value> = None;
        let mut last_err_msg = String::new();
        for attempt in 0..3usize {
            if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                break;
            }
            if attempt > 0 {
                set_status(format!(
                    "Шаг {}/{max_openai_steps}: повтор запроса ({display_model}, попытка {}/3)…",
                    step + 1,
                    attempt + 1
                ));
                std::thread::sleep(std::time::Duration::from_millis(1200 * attempt as u64));
                if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                    break;
                }
            }
            let spawn_res = Command::new("curl")
                .args([
                    "-s",
                    "--proto",
                    "=https",
                    "--connect-timeout",
                    "20",
                    "--max-time",
                    "240",
                    "--config",
                    "-",
                    "-d",
                    &format!("@{}", req_path.display()),
                    &url,
                ])
                .stdin(Stdio::piped())
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            let curl_out = match spawn_res {
                Ok(mut child) => {
                    ASYNC_GEMINI_PID.store(child.id(), Ordering::SeqCst);
                    if let Some(mut stdin) = child.stdin.take() {
                        let _ = stdin.write_all(hdrs.as_bytes());
                    }
                    let out = child.wait_with_output();
                    ASYNC_GEMINI_PID.store(0, Ordering::SeqCst);
                    out
                }
                Err(e) => Err(e),
            };

            if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                break;
            }

            match curl_out {
                Ok(o) => match serde_json::from_slice::<Value>(&o.stdout) {
                    Ok(v) => {
                        parsed_resp = Some(v);
                        break;
                    }
                    Err(_) => {
                        let preview: String =
                            String::from_utf8_lossy(&o.stdout).chars().take(280).collect();
                        last_err_msg = if preview.trim().is_empty() {
                            format!("Пустой ответ / обрыв связи от {display_model} (таймаут или сброс соединения)")
                        } else {
                            format!("Некорректный JSON от {display_model}: {preview}")
                        };
                    }
                },
                Err(_) => {
                    last_err_msg = format!("Ошибка сети при обращении к {display_model} ({url}).");
                }
            }
        }
        let _ = fs::remove_file(&req_path);

        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break;
        }

        let Some(resp_json) = parsed_resp else {
            final_reply = last_err_msg;
            break;
        };

        if let Some(err) = resp_json.get("error") {
            let msg = err
                .get("message")
                .and_then(|m| m.as_str())
                .or_else(|| err.as_str())
                .unwrap_or("API Error");
            final_reply = format!("Ошибка {display_model}: {msg}");
            break;
        }

        if let Some(usage) = resp_json.get("usage") {
            let p_tok = usage.get("prompt_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            let c_tok = usage
                .get("prompt_tokens_details")
                .and_then(|d| d.get("cached_tokens"))
                .and_then(|v| v.as_u64())
                .or_else(|| usage.get("prompt_cache_hit_tokens").and_then(|v| v.as_u64()))
                .unwrap_or(0);
            let o_tok = usage.get("completion_tokens").and_then(|v| v.as_u64()).unwrap_or(0);
            total_in_tok += p_tok.saturating_sub(c_tok);
            total_cached_tok += c_tok;
            total_out_tok += o_tok;
        }

        let Some(msg_obj) = resp_json
            .get("choices")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("message"))
            .cloned()
        else {
            break;
        };

        if let Some(rt) = msg_obj
            .get("reasoning_content")
            .or_else(|| msg_obj.get("reasoning"))
            .and_then(|r| r.as_str())
        {
            timeline_push_thought(rt);
        }

        let step_text = msg_obj
            .get("content")
            .and_then(|c| c.as_str())
            .unwrap_or("")
            .trim()
            .to_string();

        let tool_calls = msg_obj
            .get("tool_calls")
            .and_then(|tc| tc.as_array())
            .cloned()
            .unwrap_or_default();

        messages.push(msg_obj.clone());

        if !step_text.is_empty() {
            final_reply = step_text;
        }

        if tool_calls.is_empty() || is_last_step {
            break;
        }

        for tc in &tool_calls {
            if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                break;
            }
            let call_id = tc.get("id").and_then(|s| s.as_str()).unwrap_or("call_0");
            let fn_obj = tc.get("function").cloned().unwrap_or_else(|| json!({}));
            let fn_name = fn_obj.get("name").and_then(|n| n.as_str()).unwrap_or("");
            let fn_args: Value = match fn_obj.get("arguments") {
                Some(Value::String(s)) => serde_json::from_str(s).unwrap_or_else(|_| json!({})),
                Some(v) => v.clone(),
                None => json!({}),
            };

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

            let mut res_str = res.to_string();
            const MAX_TOOL_RESULT_CHARS: usize = 96_000;
            if res_str.chars().count() > MAX_TOOL_RESULT_CHARS {
                let truncated: String = res_str.chars().take(MAX_TOOL_RESULT_CHARS).collect();
                res_str = format!("{truncated}… [truncated]");
            }
            messages.push(json!({
                "role": "tool",
                "tool_call_id": call_id,
                "name": fn_name,
                "content": res_str,
            }));
        }
    }

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
            format!("Генерация прервана пользователем (запрос к {display_model} остановлен).")
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
        provider,
        model_id,
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
        "{:.1} с • {:.2} ₪ • {} ток.{} • {display_model}",
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
