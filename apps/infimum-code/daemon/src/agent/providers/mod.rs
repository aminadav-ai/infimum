use std::fs;
use std::io::Write;
#[cfg(unix)]
use std::os::unix::fs::OpenOptionsExt;
use std::path::Path;
#[cfg(test)]
use std::sync::Mutex;
use std::sync::OnceLock;

use super::timeline::TimelineSnapshot;

mod gemini;
mod history;
mod openai;

pub use gemini::run_native_agent_loop;
#[cfg(test)]
pub(in crate::agent) use gemini::{extract_gemini_error_from_raw, SseTurnAccumulator};
#[cfg(test)]
pub(in crate::agent) use history::{
    cache_aligned_keep_recent, compact_old_openai_tool_messages, compact_old_tool_responses,
    normalize_contents_roles,
};

pub struct AgentResult {
    pub elapsed_tenths: i64,
    pub cost_milli_ils: i64,
    pub total_tokens: i64,
    pub reply: String,
    pub timeline: Option<TimelineSnapshot>,
}

const STUDIO_SYSTEM_PROMPT_TPL: &str = include_str!("../../../../capsule/system_prompt.md");

/// Compute a deterministic, short cache key (`infimum-<16-hex>`) for OpenAI prompt caching
/// based on the active project directory and model identifier.
pub(in crate::agent) fn stable_prompt_cache_key(proj_dir: &Path, model_id: &str) -> String {
    let mut h: u64 = 0xcbf29ce484222325;
    for b in proj_dir.to_string_lossy().as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    h ^= 0xff;
    h = h.wrapping_mul(0x100000001b3);
    for b in model_id.as_bytes() {
        h ^= *b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("infimum-{h:016x}")
}

/// Render the studio agent system prompt from the externalized `capsule/system_prompt.md` template.
/// Static rules stay at the very beginning and dynamic plan/task/mode lines stay at the tail
/// to maximize provider-side implicit/automatic prompt cache hit rates.
pub(in crate::agent) fn render_studio_system_prompt(
    proj_dir: &Path,
    imported_plan: &str,
    imported_task: &str,
    dev_mode: bool,
    turbo_mode: bool,
) -> String {
    let plan = if imported_plan.trim().is_empty() {
        "(нет)"
    } else {
        imported_plan.trim()
    };
    let task = if imported_task.trim().is_empty() {
        "(нет)"
    } else {
        imported_task.trim()
    };
    let mut rendered = STUDIO_SYSTEM_PROMPT_TPL
        .replace("{proj}", &proj_dir.display().to_string())
        .replace("{plan}", plan)
        .replace("{task}", task);
    if turbo_mode {
        rendered.push_str(
            "\n\nРЕЖИМ TURBO: ВКЛЮЧЁН (Developer Mode активен, внешние мутации разрешены без подтверждения, увеличенный лимит шагов). Доступны сетевые операции `git_tool` (`fetch`/`pull`/`push`), GitHub CLI (`gh_cli`), Cloudflare Wrangler (`wrangler_cli`), загрузка веб-страниц (`web_fetch`) и веб-поиск через Gemini Search grounding (`web_search`).\n",
        );
    } else if dev_mode {
        rendered.push_str(
            "\n\nРЕЖИМ РАЗРАБОТЧИКА (Developer Mode): ВКЛЮЧЁН. Доступны сетевые операции `git_tool` (`fetch`/`pull`/`push`), GitHub CLI (`gh_cli`), Cloudflare Wrangler (`wrangler_cli`), загрузка веб-страниц (`web_fetch`) и веб-поиск через Gemini Search grounding (`web_search`).\n",
        );
    }
    rendered
}

/// Pure Infimum-compiled agent core module (`agent_core.ifm -> agent_core.ifw`),
/// verified by Z3 (pricing formulas + JSON API error extractor).
pub(super) const AGENT_CORE_AXW: &[u8] = include_bytes!("../../../../cores/agent_core.ifw");

pub(super) fn agent_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CACHE: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CACHE.get_or_init(|| {
        let mut cfg = wasmi::Config::default();
        cfg.consume_fuel(true);
        let engine = wasmi::Engine::new(&cfg);
        let module = wasmi::Module::new(&engine, AGENT_CORE_AXW)
            .expect("embedded agent_core.ifw must be valid WASM");
        (engine, module)
    })
}

pub(super) fn write_ifm_u32_buf(mem: &mut [u8], base_word: usize, s: &str, cap: usize) -> usize {
    let mut n = 0usize;
    for ch in s.chars() {
        if n >= cap {
            break;
        }
        let off = (base_word + 1 + n) * 4;
        if off + 4 <= mem.len() {
            mem[off..off + 4].copy_from_slice(&(ch as i32).to_le_bytes());
            n += 1;
        }
    }
    let len_off = base_word * 4;
    if len_off + 4 <= mem.len() {
        mem[len_off..len_off + 4].copy_from_slice(&(n as i32).to_le_bytes());
    }
    n
}

pub(in crate::agent) fn calc_cost_milli_ils_via_infimum(
    provider: &str,
    model: &str,
    in_tok: u64,
    cached_tok: u64,
    out_tok: u64,
) -> i64 {
    let (engine, module) = agent_core_module();
    let mut store = wasmi::Store::new(engine, ());
    let _ = store.set_fuel(2_000_000);
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
pub(in crate::agent) fn format_curl_header_line(name: &str, value: &str) -> Result<String, String> {
    if value.bytes().any(|b| matches!(b, b'\r' | b'\n' | b'\0')) {
        return Err(format!("Недопустимый управляющий символ в заголовке '{name}'."));
    }
    let escaped = value.replace('\\', "\\\\").replace('"', "\\\"");
    Ok(format!("header = \"{name}: {escaped}\"\n"))
}

pub(super) fn write_private_temp_file(path: &Path, content: &str) {
    let mut opts = fs::OpenOptions::new();
    opts.create(true).write(true).truncate(true);
    #[cfg(unix)]
    opts.mode(0o600);
    if let Ok(mut f) = opts.open(path) {
        let _ = f.write_all(content.as_bytes());
    }
}

#[cfg(test)]
pub(crate) fn test_cmd_lock() -> &'static Mutex<()> {
    static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| Mutex::new(()))
}
