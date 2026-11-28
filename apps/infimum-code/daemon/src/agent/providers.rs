//! LLM provider loops (Gemini SSE + OpenAI-compatible providers), system prompt rendering,
//! cost accounting, and tool-response compaction.

use crate::policy::CapabilityPolicy;
use crate::studio_abi::*;
use serde_json::{json, Value};
use std::fs;
use std::io::{BufRead, BufReader, Write};
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::Ordering;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::OnceLock;
use std::time::Instant;

use super::conversations::{
    active_conv_cell, save_active_conversation, take_pending_images, AttachedImage, ConvMessage,
    MAX_ATTACHED_IMAGES,
};
use super::timeline::{
    reset_live_timeline, timeline_finalize, timeline_finish_step, timeline_push_step,
    timeline_push_thought, timeline_set_round, timeline_set_thought, TimelineSnapshot,
    ASYNC_CANCELLED, ASYNC_GEMINI_PID,
};
use super::tool_specs::{detect_tool_options, gemini_tool_declarations, openai_tool_declarations};
use super::tools::{
    append_play_tag_if_axw_present, describe_tool_ru, describe_tool_step, execute_native_tool,
    resolve_whitelisted_project, studio_root_candidates,
};

pub(super) const AGENT_CORE_AXW: &[u8] = include_bytes!("../../../cores/agent_core.ifw");

pub(super) fn agent_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("agent_core.ifw", AGENT_CORE_AXW))
}

pub(super) const EMBEDDED_STUDIO_SYSTEM_PROMPT: &str = include_str!("../../../capsule/system_prompt.md");

/// Load the live `capsule/system_prompt.md` on every request (with embedded fallback)
/// so prompt tweaks take effect immediately without recompiling Rust or restarting Infimum Code.
pub(super) fn render_studio_system_prompt(
    proj_dir: &Path,
    imported_plan: &str,
    imported_task: &str,
    dev_mode: bool,
    turbo_mode: bool,
) -> String {
    let mut tpl = None;
    for root in studio_root_candidates() {
        if let Ok(s) = fs::read_to_string(root.join("capsule/system_prompt.md")) {
            if !s.trim().is_empty() {
                tpl = Some(s);
                break;
            }
        }
    }
    let raw = tpl.unwrap_or_else(|| EMBEDDED_STUDIO_SYSTEM_PROMPT.to_string());
    let proj_s = proj_dir.display().to_string();
    let plan_s = if imported_plan.trim().is_empty() { "(нет)" } else { imported_plan.trim() };
    let task_s = if imported_task.trim().is_empty() { "(нет)" } else { imported_task.trim() };
    let mut rendered = raw
        .replace("{proj}", &proj_s)
        .replace("{plan}", plan_s)
        .replace("{task}", task_s);
    if turbo_mode {
        rendered.push_str(
            "\nРЕЖИМ TURBO: ВКЛЮЧЁН. Developer Mode активен, все внешние мутации (`git push`, `gh`, `wrangler`) разрешены без подтверждения, лимит шагов увеличен до 160.\n",
        );
    } else if dev_mode {
        rendered.push_str(
            "\nРЕЖИМ РАЗРАБОТЧИКА (Developer Mode): ВКЛЮЧЁН. Доступны сетевые операции `git_tool` (`fetch`/`pull`/`push`), GitHub CLI (`gh_cli`), Cloudflare Wrangler (`wrangler_cli`), загрузка веб-страниц (`web_fetch`) и веб-поиск через Gemini Search grounding (`web_search`).\n",
        );
    }
    rendered
}

pub(super) fn write_ifm_u32_buf(mem: &mut [u8], base_word: usize, s: &str, max_chars: usize) -> usize {
    let chars: Vec<u32> = s.chars().take(max_chars).map(|c| c as u32).collect();
    let len_off = base_word * 4;
    if len_off + 4 <= mem.len() {
        mem[len_off..len_off + 4].copy_from_slice(&(chars.len() as i32).to_le_bytes());
    }
    for (i, &cp) in chars.iter().enumerate() {
        let off = (base_word + 1 + i) * 4;
        if off + 4 <= mem.len() {
            mem[off..off + 4].copy_from_slice(&cp.to_le_bytes());
        }
    }
    chars.len()
}

pub(super) fn calc_cost_milli_ils_via_infimum(
    provider: &str,
    model: &str,
    in_tok: u64,
    cached_tok: u64,
    out_tok: u64,
) -> i64 {
    let (engine, module) = agent_core_module();
    let mut store = wasmi::Store::new(engine, ());
    if store.set_fuel(5_000_000).is_err() {
        return 0;
    }
    let linker = wasmi::Linker::new(engine);
    let Ok(inst) = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
    else {
        return 0;
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return 0;
    };
    if crate::core_loader::check_core_abi(&inst, &mut store, 1, "agent_core").is_err() {
        return 0;
    }
    let Ok(func) = inst.get_typed_func::<(i32, i32, i32), i32>(&store, "calc_cost_milli_ils_ifm")
    else {
        return 0;
    };
    let Ok(prov_word) = crate::core_loader::get_word_export(&inst, &mut store, "get_prov_buf_word")
    else {
        return 0;
    };
    let Ok(model_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_model_buf_word")
    else {
        return 0;
    };
    {
        let mem = memory.data_mut(&mut store);
        write_ifm_u32_buf(mem, prov_word, provider, 64);
        write_ifm_u32_buf(mem, model_word, model, 128);
    }
    func.call(
        &mut store,
        (
            in_tok.min(50_000_000) as i32,
            cached_tok.min(50_000_000) as i32,
            out_tok.min(50_000_000) as i32,
        ),
    )
    .unwrap_or(0)
    .max(0) as i64
}

/// Format a single `curl --config -` header directive, rejecting control chars (`\r`, `\n`, `\0`)
/// and escaping `\` and `"` so API keys never touch `/tmp`, argv, or inject extra curl directives.
pub(super) fn format_curl_header_line(name: &str, value: &str) -> Result<String, String> {
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
        return Err(format!("Недопустимый управляющий символ в заголовке '{name}'."));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("header = \"{name}: {escaped}\"\n"))
}

fn write_private_temp_file(path: &Path, content: &str) {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(content.as_bytes());
    }
}

pub(super) fn openai_provider_base_url(provider: &str) -> &'static str {
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

/// Full multi-step OpenAI-compatible agent loop (DeepSeek, OpenRouter :free, Groq, Z.ai GLM, Mistral)
/// with shared conversation history, context caching accounting, and all 9 native workspace tools.
pub(super) fn run_openai_compat_native_agent_loop<F>(
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
    let sys_prompt = render_studio_system_prompt(&proj_dir, &imported_plan, &imported_task, dev_mode, turbo_mode);
    let tool_opts = detect_tool_options(&proj_dir, dev_mode);
    let tools_decl = openai_tool_declarations(tool_opts);

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
        set_status(format!("Шаг {}/{max_openai_steps}: думаю ({display_model})…", step + 1));

        let mut req_messages = messages.clone();
        compact_old_openai_tool_messages(&mut req_messages, 4);

        let body = if is_last_step {
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
        let pct = ((total_cached_tok as f64 / (total_in_tok + total_cached_tok) as f64) * 100.0).round() as u64;
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

pub struct AgentResult {
    pub elapsed_tenths: i64,
    pub cost_milli_ils: i64,
    pub total_tokens: i64,
    pub reply: String,
    pub timeline: Option<TimelineSnapshot>,
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
    let sys_prompt = render_studio_system_prompt(&proj_dir, &imported_plan, &imported_task, dev_mode, turbo_mode);
    let tool_opts = detect_tool_options(&proj_dir, dev_mode);
    let tools_decl = json!([gemini_tool_declarations(tool_opts)]);

    // Build Gemini `contents` from full shared conversation history (last 16 messages, images in last 4) + current prompt
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
    let mut total_out_tok: u64 = 0;
    let mut final_reply = String::new();
    let mut tool_steps = 0usize;
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

        // Non-destructively compact older large read-only tool payloads on a working copy
        // while keeping the latest 4 tool-response steps and all model turns/errors intact.
        let mut req_contents = contents.clone();
        compact_old_tool_responses(&mut req_contents, 4);

        // Always keep `tools` declared when tool calls exist in `contents`; disable further calls on last step via `toolConfig`.
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
                if low.contains("thinkingconfig") || low.contains("includethoughts") || low.contains("thinking") {
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
                timeline_push_thought(&round_thought);
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
    let total_tokens = (total_in_tok + total_out_tok) as i64;
    let cost_milli_ils = calc_cost_milli_ils_via_infimum("gemini", model, total_in_tok, 0, total_out_tok);
    let final_timeline = timeline_finalize(elapsed_tenths);
    let saved_timeline = if final_timeline.steps.is_empty() && final_timeline.thought_snippet.is_empty() {
        None
    } else {
        Some(final_timeline)
    };

    append_play_tag_if_axw_present(&mut final_reply, active_project);
    // Persist assistant reply into shared conversation JSON!
    let meta_str = format!(
        "{:.1} с • {:.2} ₪ • {} ток. • {model}",
        elapsed_tenths as f64 / 10.0,
        cost_milli_ils as f64 / 1000.0,
        total_tokens
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

pub(super) const COMPACT_RESPONSE_THRESHOLD_CHARS: usize = 900;

/// Compact older large read-only `functionResponse` payloads (`read_project_file`, `search_project`,
/// `list_project_files`, `get_file_outline`) while strictly preserving Gemini 4 protocol invariants:
/// 1. Never modifies `model` turns (`thoughtSignature`, `functionCall`) or `user`/`model` alternation.
/// 2. Keeps the latest `keep_recent_tool_steps` messages containing `functionResponse` completely intact.
/// 3. Never compacts mutation/verification/command tools or any response containing an error.
pub(super) fn compact_old_tool_responses(contents: &mut [Value], keep_recent_tool_steps: usize) {
    let tool_msg_indices: Vec<usize> = contents
        .iter()
        .enumerate()
        .filter_map(|(idx, msg)| {
            if msg.get("role").and_then(|r| r.as_str()) == Some("model") {
                return None;
            }
            let has_fr = msg
                .get("parts")
                .and_then(|p| p.as_array())
                .map(|parts| parts.iter().any(|p| p.get("functionResponse").is_some()))
                .unwrap_or(false);
            if has_fr { Some(idx) } else { None }
        })
        .collect();

    if tool_msg_indices.len() <= keep_recent_tool_steps {
        return;
    }

    let compact_count = tool_msg_indices.len() - keep_recent_tool_steps;
    for &msg_idx in &tool_msg_indices[..compact_count] {
        let Some(parts) = contents[msg_idx]
            .get_mut("parts")
            .and_then(|p| p.as_array_mut())
        else {
            continue;
        };
        for part in parts {
            let Some(fr) = part.get_mut("functionResponse") else {
                continue;
            };
            let tool_name = fr
                .get("name")
                .and_then(|n| n.as_str())
                .unwrap_or("")
                .to_string();
            if !matches!(
                tool_name.as_str(),
                "read_project_file"
                    | "search_project"
                    | "list_project_files"
                    | "get_file_outline"
                    | "get_symbol"
                    | "get_ifm_symbol"
                    | "read_infimum_repo"
                    | "search_infimum_repo"
                    | "list_infimum_repo"
                    | "read_infimum_std"
                    | "read_infimum_guide"
                    | "read_infimum_template"
                    | "web_fetch"
                    | "web_search"
                    | "git_tool"
                    | "gh_cli"
                    | "wrangler_cli"
                    | "run_project_command"
            ) {
                continue;
            }
            let Some(resp) = fr.get_mut("response").and_then(|r| r.as_object_mut()) else {
                continue;
            };
            // Never compact error or non-zero exit responses
            if resp.contains_key("error")
                || resp.get("ok").and_then(|v| v.as_bool()) == Some(false)
                || resp
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .map(|c| c != 0)
                    .unwrap_or(false)
            {
                continue;
            }
            let serialized_len = serde_json::to_string(resp)
                .map(|s| s.chars().count())
                .unwrap_or(0);
            if serialized_len <= COMPACT_RESPONSE_THRESHOLD_CHARS {
                continue;
            }

            match tool_name.as_str() {
                "read_project_file" => {
                    if let Some(content) = resp.get("content").and_then(|v| v.as_str()) {
                        let orig_chars = content.chars().count();
                        let path = resp
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<unknown>");
                        let start = resp.get("start_line").and_then(|v| v.as_u64()).unwrap_or(1);
                        let end = resp.get("end_line").and_then(|v| v.as_u64()).unwrap_or(start);
                        resp.insert(
                            "content".to_string(),
                            json!(format!(
                                "[compacted older tool output: read_project_file {path} lines {start}..{end}, original {orig_chars} chars; call read_project_file again if exact lines are needed]"
                            )),
                        );
                    }
                }
                "search_project" => {
                    if let Some(matches_str) = resp.get("matches").and_then(|v| v.as_str()) {
                        let orig_chars = matches_str.chars().count();
                        resp.insert(
                            "matches".to_string(),
                            json!(format!(
                                "[compacted older tool output: search_project, original {orig_chars} chars; call search_project again if exact matches are needed]"
                            )),
                        );
                    }
                }
                "list_project_files" => {
                    if let Some(files_val) = resp.get("files") {
                        let orig_chars = serde_json::to_string(files_val)
                            .map(|s| s.chars().count())
                            .unwrap_or(0);
                        let count = files_val.as_array().map(|a| a.len()).unwrap_or(0);
                        let dir = resp
                            .get("project_dir")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<unknown>");
                        resp.insert(
                            "files".to_string(),
                            json!(format!(
                                "[compacted older tool output: list_project_files {dir} ({count} files), original {orig_chars} chars; call list_project_files again if exact list is needed]"
                            )),
                        );
                    }
                }
                "get_file_outline" => {
                    if let Some(outline_str) = resp.get("outline").and_then(|v| v.as_str()) {
                        let orig_chars = outline_str.chars().count();
                        let path = resp
                            .get("path")
                            .and_then(|v| v.as_str())
                            .unwrap_or("<unknown>");
                        resp.insert(
                            "outline".to_string(),
                            json!(format!(
                                "[compacted older tool output: get_file_outline {path}, original {orig_chars} chars; call get_file_outline again if exact outline is needed]"
                            )),
                        );
                    }
                }
                "get_symbol" | "get_ifm_symbol" => {
                    if let Some(src_str) = resp.get("source").and_then(|v| v.as_str()) {
                        let orig_chars = src_str.chars().count();
                        let sym = resp.get("name").and_then(|v| v.as_str()).unwrap_or("?");
                        let path = resp.get("path").and_then(|v| v.as_str()).unwrap_or("?");
                        resp.insert(
                            "source".to_string(),
                            json!(format!(
                                "[compacted older tool output: {tool_name} {sym} in {path}, original {orig_chars} chars]"
                            )),
                        );
                    }
                }
                "run_project_command" | "git_tool" | "gh_cli" | "wrangler_cli" => {
                    if let Some(out_str) = resp.get("output").and_then(|v| v.as_str()) {
                        let orig_chars = out_str.chars().count();
                        let first_line = out_str.lines().next().unwrap_or("");
                        let snippet: String = first_line.chars().take(120).collect();
                        resp.insert(
                            "output".to_string(),
                            json!(format!(
                                "[compacted older ok output: {tool_name} ({snippet}), original {orig_chars} chars]"
                            )),
                        );
                    }
                }
                "web_fetch" | "read_infimum_repo" | "read_infimum_std" | "read_infimum_guide"
                | "read_infimum_template" => {
                    for key in ["content", "guide", "source", "template"] {
                        if let Some(val_str) = resp.get(key).and_then(|v| v.as_str()) {
                            let orig_chars = val_str.chars().count();
                            if orig_chars > 400 {
                                resp.insert(
                                    key.to_string(),
                                    json!(format!(
                                        "[compacted older tool output: {tool_name}, original {orig_chars} chars; call again if exact text is needed]"
                                    )),
                                );
                            }
                        }
                    }
                }
                "web_search" => {
                    if let Some(sum_str) = resp.get("summary").and_then(|v| v.as_str()) {
                        let orig_chars = sum_str.chars().count();
                        if orig_chars > 400 {
                            resp.insert(
                                "summary".to_string(),
                                json!(format!(
                                    "[compacted older web_search summary, original {orig_chars} chars]"
                                )),
                            );
                        }
                    }
                }
                "search_infimum_repo" | "list_infimum_repo" => {
                    for key in ["matches", "files"] {
                        if let Some(v) = resp.get(key) {
                            let orig_chars = serde_json::to_string(v).map(|s| s.chars().count()).unwrap_or(0);
                            if orig_chars > 400 {
                                resp.insert(
                                    key.to_string(),
                                    json!(format!(
                                        "[compacted older tool output: {tool_name}, original {orig_chars} chars]"
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

/// Compact older large `role: "tool"` messages in OpenAI-compatible conversations,
/// keeping the latest `keep_recent_tool_msgs` tool results and all error responses intact.
pub(super) fn compact_old_openai_tool_messages(messages: &mut [Value], keep_recent_tool_msgs: usize) {
    let tool_indices: Vec<usize> = messages
        .iter()
        .enumerate()
        .filter_map(|(idx, m)| {
            if m.get("role").and_then(|r| r.as_str()) == Some("tool") {
                Some(idx)
            } else {
                None
            }
        })
        .collect();
    if tool_indices.len() <= keep_recent_tool_msgs {
        return;
    }
    let compact_count = tool_indices.len() - keep_recent_tool_msgs;
    for &idx in &tool_indices[..compact_count] {
        let Some(content) = messages[idx].get("content").and_then(|v| v.as_str()) else {
            continue;
        };
        let orig_chars = content.chars().count();
        if orig_chars <= COMPACT_RESPONSE_THRESHOLD_CHARS {
            continue;
        }
        // Preserve error responses in full
        if let Ok(parsed) = serde_json::from_str::<Value>(content) {
            if parsed.get("error").is_some()
                || parsed.get("ok").and_then(|v| v.as_bool()) == Some(false)
                || parsed
                    .get("exit_code")
                    .and_then(|v| v.as_i64())
                    .map(|c| c != 0)
                    .unwrap_or(false)
            {
                continue;
            }
        }
        let preview: String = content.chars().take(160).collect();
        messages[idx]["content"] = json!(format!(
            "[compacted older tool result, original {orig_chars} chars; preview: {preview}...]"
        ));
    }
}

pub(super) struct GeminiTurn {
    parts: Vec<Value>,
    in_tokens: u64,
    out_tokens: u64,
}

#[derive(Default)]
pub(super) struct SseTurnAccumulator {
    pub(super) parts: Vec<Value>,
    pub(super) in_tokens: u64,
    pub(super) out_tokens: u64,
    pub(super) thought_chars: usize,
    pub(super) thought_text: String,
    pub(super) answer_chars: usize,
    pub(super) fn_calls_seen: usize,
    pub(super) chunks_parsed: usize,
    pub(super) api_error: Option<String>,
    pub(super) raw_non_sse: String,
}

impl SseTurnAccumulator {
    fn is_plain_text_part(part: &Value) -> Option<(bool, &str)> {
        let obj = part.as_object()?;
        if obj.contains_key("functionCall")
            || obj.contains_key("thoughtSignature")
            || obj.contains_key("inlineData")
            || obj.contains_key("executableCode")
            || obj.contains_key("codeExecutionResult")
        {
            return None;
        }
        let text = obj.get("text")?.as_str()?;
        let is_thought = obj.get("thought").and_then(|v| v.as_bool()).unwrap_or(false);
        Some((is_thought, text))
    }

    fn push_part(&mut self, part: Value) {
        if part.get("functionCall").is_some() {
            self.fn_calls_seen += 1;
        } else if let Some(txt) = part.get("text").and_then(|t| t.as_str()) {
            let is_thought = part.get("thought").and_then(|v| v.as_bool()).unwrap_or(false);
            if is_thought {
                self.thought_chars += txt.chars().count();
                self.thought_text.push_str(txt);
                if self.thought_text.len() > 2400 {
                    let keep: String = self
                        .thought_text
                        .chars()
                        .rev()
                        .take(500)
                        .collect::<Vec<_>>()
                        .into_iter()
                        .rev()
                        .collect();
                    self.thought_text = keep;
                }
                timeline_set_thought(&self.thought_text);
            } else {
                self.answer_chars += txt.chars().count();
            }
        }

        if let Some((is_thought, new_txt)) = Self::is_plain_text_part(&part) {
            if let Some(last) = self.parts.last_mut() {
                if let Some((last_thought, _)) = Self::is_plain_text_part(last) {
                    if last_thought == is_thought {
                        if let Some(existing) = last
                            .as_object_mut()
                            .and_then(|o| o.get_mut("text"))
                            .and_then(|v| v.as_str())
                            .map(|s| s.to_string())
                        {
                            let mut combined = existing;
                            combined.push_str(new_txt);
                            if let Some(o) = last.as_object_mut() {
                                o.insert("text".to_string(), Value::String(combined));
                            }
                            return;
                        }
                    }
                }
            }
        }
        self.parts.push(part);
    }

    fn ingest_chunk(&mut self, v: &Value) {
        self.chunks_parsed += 1;
        if let Some(err) = v.get("error").and_then(|e| e.get("message")).and_then(|m| m.as_str()) {
            self.api_error = Some(err.to_string());
            return;
        }
        if let Some(u) = v.get("usageMetadata") {
            let p_tok = u.get("promptTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            let c_tok = u.get("candidatesTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            let t_tok = u.get("thoughtsTokenCount").and_then(|x| x.as_u64()).unwrap_or(0);
            if p_tok > 0 {
                self.in_tokens = p_tok;
            }
            if c_tok + t_tok > 0 {
                self.out_tokens = c_tok + t_tok;
            }
        }
        if let Some(parts) = v
            .get("candidates")
            .and_then(|c| c.get(0))
            .and_then(|c0| c0.get("content"))
            .and_then(|ct| ct.get("parts"))
            .and_then(|p| p.as_array())
        {
            for p in parts {
                self.push_part(p.clone());
            }
        }
    }

    pub(super) fn ingest_line(&mut self, raw_line: &str) -> bool {
        let line = raw_line.trim_end_matches(&['\r', '\n'][..]);
        if line.is_empty()
            || line.starts_with(':')
            || line.starts_with("event:")
            || line.starts_with("id:")
            || line.starts_with("retry:")
        {
            return false;
        }
        if let Some(rest) = line.strip_prefix("data:") {
            let payload = rest.trim();
            if payload.is_empty() || payload == "[DONE]" {
                return false;
            }
            if let Ok(v) = serde_json::from_str::<Value>(payload) {
                self.ingest_chunk(&v);
                return true;
            }
        }
        if self.raw_non_sse.len() < 16384 {
            self.raw_non_sse.push_str(line);
            self.raw_non_sse.push('\n');
        }
        false
    }
}

pub(super) fn extract_gemini_error_from_raw(raw: &str) -> Option<String> {
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

pub(super) fn is_transient_api_error(msg: &str) -> bool {
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

pub(super) fn call_gemini_sse_with_retry<F>(
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
                if !retryable || attempt + 1 >= MAX_ATTEMPTS || ASYNC_CANCELLED.load(Ordering::SeqCst) {
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

pub(super) fn call_gemini_sse_once<F>(
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
        // Check if server returned a non-SSE JSON error (e.g. HTTP 400/429/500/503) or full JSON body.
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
        let retryable = matches!(exit_code, 7 | 18 | 28 | 35 | 52 | 56) || acc.raw_non_sse.trim().is_empty();
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
        out_tokens: acc.out_tokens,
    })
}

/// Ensure Gemini `contents` starts with a `"user"` turn and merges any consecutive same-role turns.
pub(super) fn normalize_contents_roles(contents: &mut Vec<Value>) {
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

#[cfg(test)]
pub(crate) fn test_cmd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}

