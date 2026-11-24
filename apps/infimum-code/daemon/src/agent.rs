//! Native Infimum Code Agent Loop, Shared Conversations (`~/.config/infimum-code/conversations`),
//! and Antigravity Brain Session Importer (`~/.gemini/antigravity/brain`).
//!
//! 100% compatible with the browser Infimum Code conversation JSON schema so users can
//! seamlessly switch between browser Infimum Code and Infimum Code Native (.ifw).

use crate::policy::CapabilityPolicy;
use crate::studio_abi::*;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
#[cfg(unix)]
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

pub static ASYNC_GEMINI_PID: AtomicU32 = AtomicU32::new(0);
pub static ASYNC_CMD_PID: AtomicU32 = AtomicU32::new(0);
pub static ASYNC_CANCELLED: AtomicBool = AtomicBool::new(false);
pub static ASYNC_CMD_KILL_REQUESTED: AtomicBool = AtomicBool::new(false);

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct AgentStep {
    pub status: i64,
    pub kind: i64,
    pub dur_tenths: i64,
    pub title: String,
    pub detail: String,
    #[serde(default)]
    pub output_tail: String,
    #[serde(skip)]
    pub started_at: Option<Instant>,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct TimelineSnapshot {
    pub round: i64,
    pub max_rounds: i64,
    pub total_elapsed_tenths: i64,
    #[serde(default)]
    pub has_running_cmd: bool,
    #[serde(default)]
    pub thought_snippet: String,
    #[serde(default)]
    pub steps: Vec<AgentStep>,
}

struct LiveTimelineState {
    started_at: Option<Instant>,
    snap: TimelineSnapshot,
}

fn live_timeline_cell() -> &'static Mutex<LiveTimelineState> {
    static CELL: OnceLock<Mutex<LiveTimelineState>> = OnceLock::new();
    CELL.get_or_init(|| {
        Mutex::new(LiveTimelineState {
            started_at: None,
            snap: TimelineSnapshot::default(),
        })
    })
}

pub fn reset_live_timeline(max_rounds: i64) {
    ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);
    if let Ok(mut g) = live_timeline_cell().lock() {
        g.started_at = Some(Instant::now());
        g.snap = TimelineSnapshot {
            round: 1,
            max_rounds,
            total_elapsed_tenths: 0,
            has_running_cmd: false,
            thought_snippet: String::new(),
            steps: Vec::new(),
        };
    }
}

fn timeline_set_round(round: i64) {
    if let Ok(mut g) = live_timeline_cell().lock() {
        g.snap.round = round;
    }
}

fn timeline_set_thought(raw_thought: &str) {
    let cleaned = raw_thought.replace('\r', "");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let max_chars = 600;
    let snippet: String = if chars.len() > max_chars {
        let mut s = String::from("…");
        for ch in &chars[chars.len() - max_chars..] {
            s.push(*ch);
        }
        s
    } else {
        trimmed.to_string()
    };
    if let Ok(mut g) = live_timeline_cell().lock() {
        g.snap.thought_snippet = snippet;
    }
}

fn timeline_push_thought(raw_thought: &str) {
    let cleaned = raw_thought.replace('\r', "");
    let trimmed = cleaned.trim();
    if trimmed.is_empty() {
        return;
    }
    let chars: Vec<char> = trimmed.chars().collect();
    let max_chars = 1000;
    let body: String = if chars.len() > max_chars {
        let mut s: String = chars[..max_chars - 1].iter().collect();
        s.push('…');
        s
    } else {
        trimmed.to_string()
    };
    if let Ok(mut g) = live_timeline_cell().lock() {
        let round = g.snap.round.max(1);
        let is_dup = g
            .snap
            .steps
            .last()
            .map(|st| st.kind == STEP_KIND_THINK && st.output_tail == body)
            .unwrap_or(false);
        if !is_dup {
            g.snap.steps.push(AgentStep {
                status: STEP_STATUS_DONE,
                kind: STEP_KIND_THINK,
                dur_tenths: 0,
                title: format!("Мысли модели (шаг {round})"),
                detail: String::new(),
                output_tail: body,
                started_at: None,
            });
        }
        g.snap.thought_snippet.clear();
    }
}

fn timeline_push_step(kind: i64, title: String, detail: String) -> usize {
    if let Ok(mut g) = live_timeline_cell().lock() {
        let idx = g.snap.steps.len();
        if kind == STEP_KIND_CMD {
            g.snap.has_running_cmd = true;
        }
        g.snap.steps.push(AgentStep {
            status: STEP_STATUS_RUNNING,
            kind,
            dur_tenths: 0,
            title,
            detail,
            output_tail: String::new(),
            started_at: Some(Instant::now()),
        });
        idx
    } else {
        0
    }
}

fn timeline_update_step_tail(step_idx: usize, tail: String) {
    if let Ok(mut g) = live_timeline_cell().lock() {
        if let Some(st) = g.snap.steps.get_mut(step_idx) {
            st.output_tail = tail;
        }
    }
}

fn timeline_finish_step(step_idx: usize, status: i64, tail_opt: Option<String>) {
    if let Ok(mut g) = live_timeline_cell().lock() {
        if let Some(st) = g.snap.steps.get_mut(step_idx) {
            if let Some(t0) = st.started_at {
                st.dur_tenths = (t0.elapsed().as_millis() / 100) as i64;
            }
            st.status = status;
            if let Some(t) = tail_opt {
                st.output_tail = t;
            }
            if st.kind == STEP_KIND_CMD {
                g.snap.has_running_cmd = false;
            }
        }
    }
}

fn timeline_finalize(total_elapsed_tenths: i64) -> TimelineSnapshot {
    if let Ok(mut g) = live_timeline_cell().lock() {
        g.snap.total_elapsed_tenths = total_elapsed_tenths;
        g.snap.has_running_cmd = false;
        for st in &mut g.snap.steps {
            if st.status == STEP_STATUS_RUNNING {
                if let Some(t0) = st.started_at {
                    st.dur_tenths = (t0.elapsed().as_millis() / 100) as i64;
                }
                st.status = if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                    STEP_STATUS_CANCELLED
                } else {
                    STEP_STATUS_DONE
                };
            }
        }
        g.snap.clone()
    } else {
        TimelineSnapshot::default()
    }
}

pub fn get_live_timeline_words(max_words: usize) -> Vec<i64> {
    let snap = if let Ok(g) = live_timeline_cell().lock() {
        let mut s = g.snap.clone();
        if let Some(t0) = g.started_at {
            s.total_elapsed_tenths = (t0.elapsed().as_millis() / 100) as i64;
        }
        for st in &mut s.steps {
            if st.status == STEP_STATUS_RUNNING {
                if let Some(st0) = st.started_at {
                    st.dur_tenths = (st0.elapsed().as_millis() / 100) as i64;
                }
            }
        }
        s.has_running_cmd = ASYNC_CMD_PID.load(Ordering::SeqCst) > 0;
        s
    } else {
        return Vec::new();
    };
    encode_timeline_words(&snap, max_words)
}

/// Binary TimelineBlob layout (i64 words):
///   [0] total_words
///   [1] round
///   [2] max_rounds
///   [3] total_elapsed_tenths
///   [4] has_running_cmd (0|1)
///   [5] step_count
///   [6] thought_len, followed by thought_chars[0..thought_len]
///   then for each step:
///     [+0] status, [+1] kind, [+2] dur_tenths,
///     [+3] title_len, title_chars...,
///     [+?] detail_len, detail_chars...,
///     [+?] tail_len, tail_chars...
pub fn encode_timeline_words(snap: &TimelineSnapshot, max_words: usize) -> Vec<i64> {
    if max_words < 16 {
        return Vec::new();
    }
    let thought_chars: Vec<i64> = snap
        .thought_snippet
        .chars()
        .take(600)
        .map(|c| c as i64)
        .collect();
    let mut out: Vec<i64> = Vec::with_capacity(256);
    out.push(0); // [0] total_words (patched at end)
    out.push(snap.round.max(1));
    out.push(snap.max_rounds.max(1));
    out.push(snap.total_elapsed_tenths.max(0));
    out.push(if snap.has_running_cmd { 1 } else { 0 });
    out.push(0); // [5] step_count (patched at end)
    out.push(thought_chars.len() as i64);
    out.extend_from_slice(&thought_chars);

    let max_steps = 32usize;
    let start_idx = snap.steps.len().saturating_sub(max_steps);
    let mut encoded_steps: i64 = 0;
    for st in &snap.steps[start_idx..] {
        let title_c: Vec<i64> = st.title.chars().take(100).map(|c| c as i64).collect();
        let detail_c: Vec<i64> = st.detail.chars().take(180).map(|c| c as i64).collect();
        let tail_limit = if st.kind == STEP_KIND_THINK { 1000 } else { 420 };
        let tail_c: Vec<i64> = st.output_tail.chars().take(tail_limit).map(|c| c as i64).collect();
        let needed = 3 + (1 + title_c.len()) + (1 + detail_c.len()) + (1 + tail_c.len());
        if out.len() + needed > max_words {
            break;
        }
        out.push(st.status);
        out.push(st.kind);
        out.push(st.dur_tenths.max(0));
        out.push(title_c.len() as i64);
        out.extend_from_slice(&title_c);
        out.push(detail_c.len() as i64);
        out.extend_from_slice(&detail_c);
        out.push(tail_c.len() as i64);
        out.extend_from_slice(&tail_c);
        encoded_steps += 1;
    }
    out[0] = out.len() as i64;
    let total_steps = (snap.steps.len() as i64).min(65535);
    out[5] = if total_steps > encoded_steps {
        encoded_steps | (total_steps << 16)
    } else {
        encoded_steps
    };
    out
}

#[cfg(unix)]
pub const SIGTERM_NUM: i32 = 15;
#[cfg(unix)]
pub const SIGKILL_NUM: i32 = 9;

/// Send a POSIX signal to a dedicated process group (`-pgid`) via `kill(2)`.
/// Strictly rejects `pgid <= 1` so `kill(0, ..)` or `kill(-1, ..)` can never be issued.
#[cfg(unix)]
pub fn signal_process_group(pgid: u32, sig: i32) {
    if pgid <= 1 || pgid > i32::MAX as u32 {
        return;
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        let _ = kill(-(pgid as i32), sig);
    }
}

/// Send a POSIX signal to a single process (`pid > 1`) via `kill(2)`.
#[cfg(unix)]
pub fn signal_pid(pid: u32, sig: i32) {
    if pid <= 1 || pid > i32::MAX as u32 {
        return;
    }
    extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        let _ = kill(pid as i32, sig);
    }
}

/// Kill the currently running shell command process group without cancelling the agent loop.
pub fn kill_active_shell_command() -> bool {
    let pid = ASYNC_CMD_PID.load(Ordering::SeqCst);
    if pid <= 1 {
        return false;
    }
    ASYNC_CMD_KILL_REQUESTED.store(true, Ordering::SeqCst);
    #[cfg(unix)]
    {
        signal_process_group(pid, SIGTERM_NUM);
        std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(160));
            if ASYNC_CMD_PID.load(Ordering::SeqCst) == pid {
                signal_process_group(pid, SIGKILL_NUM);
                signal_pid(pid, SIGKILL_NUM);
            }
        });
    }
    #[cfg(not(unix))]
    {
        let _ = Command::new("kill").args(["-9", &pid.to_string()]).status();
    }
    true
}

#[derive(Clone, Debug)]
pub struct AttachedImage {
    pub mime_type: String,
    pub data: String, // base64
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

const MAX_ATTACHED_IMAGES: usize = 6;

fn pending_images_cell() -> &'static Mutex<Vec<AttachedImage>> {
    static CELL: OnceLock<Mutex<Vec<AttachedImage>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(Vec::new()))
}

pub fn add_pending_image(img: AttachedImage) -> Option<usize> {
    let mut g = pending_images_cell().lock().ok()?;
    if g.len() >= MAX_ATTACHED_IMAGES {
        return None;
    }
    let idx = g.len();
    g.push(img);
    Some(idx)
}

pub fn remove_pending_image(idx: usize) {
    if let Ok(mut g) = pending_images_cell().lock() {
        if idx < g.len() {
            g.remove(idx);
        }
    }
}

pub fn clear_pending_images() {
    if let Ok(mut g) = pending_images_cell().lock() {
        g.clear();
    }
}

pub fn take_pending_images() -> Vec<AttachedImage> {
    if let Ok(mut g) = pending_images_cell().lock() {
        std::mem::take(&mut *g)
    } else {
        Vec::new()
    }
}

pub fn has_pending_images() -> bool {
    pending_images_cell()
        .lock()
        .ok()
        .map(|g| !g.is_empty())
        .unwrap_or(false)
}

pub fn pending_images_meta() -> Vec<(u32, u32, String)> {
    pending_images_cell()
        .lock()
        .ok()
        .map(|g| g.iter().map(|im| (im.width, im.height, im.mime_type.clone())).collect())
        .unwrap_or_default()
}

fn loaded_conv_gpu_images_cell() -> &'static Mutex<Option<Vec<(u32, u32, Vec<u8>)>>> {
    static CELL: OnceLock<Mutex<Option<Vec<(u32, u32, Vec<u8>)>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

pub fn set_loaded_conv_gpu_images(imgs: Vec<(u32, u32, Vec<u8>)>) {
    if let Ok(mut g) = loaded_conv_gpu_images_cell().lock() {
        *g = Some(imgs);
    }
}

pub fn take_loaded_conv_gpu_images() -> Option<Vec<(u32, u32, Vec<u8>)>> {
    loaded_conv_gpu_images_cell().lock().ok()?.take()
}

#[derive(Clone, Debug)]
pub struct ConvMessage {
    pub role: String, // "user" | "assistant"
    pub content: String,
    pub meta: Option<String>,
    pub images: Vec<AttachedImage>,
    pub timeline: Option<TimelineSnapshot>,
}

#[derive(Clone, Debug)]
pub struct ActiveConversation {
    pub id: String,
    pub project: String,
    pub title: String,
    pub imported_plan: String,
    pub imported_task: String,
    pub messages: Vec<ConvMessage>,
}

impl Default for ActiveConversation {
    fn default() -> Self {
        Self {
            id: format!("conv-{}", now_millis()),
            project: "infimum-code".to_string(),
            title: String::new(),
            imported_plan: String::new(),
            imported_task: String::new(),
            messages: Vec::new(),
        }
    }
}

pub fn active_conv_cell() -> &'static Mutex<ActiveConversation> {
    static CELL: OnceLock<Mutex<ActiveConversation>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(ActiveConversation::default()))
}

fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

fn conversations_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config/infimum-code/conversations");
    let _ = fs::create_dir_all(&dir);
    Some(dir)
}

fn native_settings_path() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config/infimum-code");
    let _ = fs::create_dir_all(&dir);
    Some(dir.join("native_settings.json"))
}

#[derive(Clone, Copy, Debug)]
pub struct NativeSettings {
    pub theme: i32,
    pub font_scale_pct: i32,
    pub currency_id: i32,
    pub model_idx: i32,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            theme: 0,
            font_scale_pct: 120,
            currency_id: 0,
            model_idx: 0,
        }
    }
}

pub fn load_native_settings() -> Option<NativeSettings> {
    let p = native_settings_path()?;
    let raw = fs::read_to_string(&p).ok()?;
    let v = serde_json::from_str::<Value>(&raw).ok()?;
    let d = NativeSettings::default();
    Some(NativeSettings {
        theme: v
            .get("theme")
            .and_then(|x| x.as_i64())
            .map(|x| x.clamp(0, 1) as i32)
            .unwrap_or(d.theme),
        font_scale_pct: v
            .get("font_scale_pct")
            .and_then(|x| x.as_i64())
            .map(|x| x.clamp(80, 200) as i32)
            .unwrap_or(d.font_scale_pct),
        currency_id: v
            .get("currency_id")
            .and_then(|x| x.as_i64())
            .map(|x| x.clamp(0, 2) as i32)
            .unwrap_or(d.currency_id),
        model_idx: v
            .get("model_idx")
            .and_then(|x| x.as_i64())
            .map(|x| x.clamp(0, 21) as i32)
            .unwrap_or(d.model_idx),
    })
}

pub fn save_native_settings(s: NativeSettings) -> bool {
    let Some(p) = native_settings_path() else {
        return false;
    };
    let doc = json!({
        "theme": s.theme.clamp(0, 1),
        "font_scale_pct": s.font_scale_pct.clamp(80, 200),
        "currency_id": s.currency_id.clamp(0, 2),
        "model_idx": s.model_idx.clamp(0, 21),
    });
    let Ok(bytes) = serde_json::to_string_pretty(&doc) else {
        return false;
    };
    fs::write(p, bytes).is_ok()
}

pub fn active_conv_header() -> String {
    if let Ok(g) = active_conv_cell().lock() {
        let proj = if g.project.trim().is_empty() {
            "infimum-code"
        } else {
            g.project.trim()
        };
        let title = if g.title.trim().is_empty() {
            "Новый диалог"
        } else {
            g.title.trim()
        };
        let clean_title: String = title.replace('\n', " ").chars().take(60).collect();
        format!("{proj} / {clean_title}")
    } else {
        "infimum-code.ifw / Автономный диалог Gemini 4".to_string()
    }
}

fn brain_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".gemini/antigravity/brain"))
}

/// Strip Antigravity XML wrappers (`<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, `<CONTEXT_SUMMARY>`).
pub fn clean_antigravity_user_text(raw: &str) -> String {
    if let Some(s) = raw.find("<USER_REQUEST>") {
        let after = &raw[s + "<USER_REQUEST>".len()..];
        if let Some(e) = after.find("</USER_REQUEST>") {
            return after[..e].trim().to_string();
        }
    }
    if raw.contains("<CONTEXT_SUMMARY>") {
        return String::new();
    }
    raw.trim().to_string()
}

pub fn list_saved_conversations() -> (Vec<PathBuf>, Vec<String>) {
    let Some(dir) = conversations_dir() else {
        return (Vec::new(), Vec::new());
    };
    let mut entries: Vec<(u64, PathBuf, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&dir) {
        for ent in rd.flatten() {
            let p = ent.path();
            if p.extension().and_then(|s| s.to_str()) != Some("json") {
                continue;
            }
            let Ok(raw) = fs::read_to_string(&p) else {
                continue;
            };
            let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                continue;
            };
            let ts = v.get("updated_at").and_then(|x| x.as_u64()).unwrap_or(0);
            let proj = v
                .get("project")
                .and_then(|x| x.as_str())
                .unwrap_or("studio")
                .replace(['\t', '\n', '\r'], " ");
            let title = v.get("title").and_then(|x| x.as_str()).unwrap_or("Диалог");
            let clean_title: String = title
                .replace(['\t', '\n', '\r'], " ")
                .chars()
                .take(44)
                .collect();
            let label = format!("{proj}\t{clean_title}");
            entries.push((ts, p, label));
        }
    }
    entries.sort_by_key(|e| std::cmp::Reverse(e.0));
    entries.truncate(60);
    let paths = entries.iter().map(|e| e.1.clone()).collect();
    let labels = entries.into_iter().map(|e| e.2).collect();
    (paths, labels)
}

pub fn load_conversation_from_path(
    path: &Path,
) -> Option<Vec<(i32, String, Option<TimelineSnapshot>, Vec<AttachedImage>)>> {
    let raw = fs::read_to_string(path).ok()?;
    let v = serde_json::from_str::<Value>(&raw).ok()?;
    let id = v
        .get("id")
        .and_then(|x| x.as_str())
        .map(|s| s.to_string())
        .unwrap_or_else(|| {
            path.file_stem()
                .and_then(|s| s.to_str())
                .unwrap_or("conv")
                .to_string()
        });
    let project = v
        .get("project")
        .and_then(|x| x.as_str())
        .unwrap_or("infimum-code")
        .to_string();
    let title = v
        .get("title")
        .and_then(|x| x.as_str())
        .unwrap_or("Диалог")
        .to_string();
    let imported_plan = v
        .get("imported_plan")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();
    let imported_task = v
        .get("imported_task")
        .and_then(|x| x.as_str())
        .unwrap_or("")
        .to_string();

    let mut conv_msgs = Vec::new();
    let mut ring_msgs = Vec::new();
    if let Some(arr) = v.get("messages").and_then(|x| x.as_array()) {
        for m in arr {
            let is_user = m.get("role").and_then(|r| r.as_str()) == Some("user");
            let txt = m.get("content").and_then(|c| c.as_str()).unwrap_or("").trim();
            let mut imgs: Vec<AttachedImage> = Vec::new();
            if let Some(iarr) = m.get("images").and_then(|x| x.as_array()) {
                for im in iarr.iter().take(MAX_ATTACHED_IMAGES) {
                    let mime = im.get("mime_type").and_then(|v| v.as_str()).unwrap_or("");
                    let data = im.get("data").and_then(|v| v.as_str()).unwrap_or("");
                    if !mime.is_empty() && !data.is_empty() {
                        imgs.push(AttachedImage {
                            mime_type: mime.to_string(),
                            data: data.to_string(),
                            width: 0,
                            height: 0,
                        });
                    }
                }
            }
            if txt.is_empty() && imgs.is_empty() {
                continue;
            }
            let meta = m
                .get("meta")
                .and_then(|x| x.as_str())
                .map(|s| s.to_string());
            let timeline: Option<TimelineSnapshot> = m
                .get("timeline")
                .and_then(|x| serde_json::from_value::<TimelineSnapshot>(x.clone()).ok());
            let display_txt = if imgs.is_empty() {
                txt.to_string()
            } else if txt.is_empty() {
                format!("[Прикреплено изображений: {}]", imgs.len())
            } else {
                txt.to_string()
            };
            conv_msgs.push(ConvMessage {
                role: if is_user { "user" } else { "assistant" }.to_string(),
                content: txt.to_string(),
                meta,
                images: imgs.clone(),
                timeline: timeline.clone(),
            });
            ring_msgs.push((if is_user { 0 } else { 1 }, display_txt, timeline, imgs));
        }
    }

    if let Ok(mut g) = active_conv_cell().lock() {
        *g = ActiveConversation {
            id,
            project,
            title,
            imported_plan,
            imported_task,
            messages: conv_msgs,
        };
    }
    Some(ring_msgs)
}

pub fn start_new_conversation(project: &str) {
    let proj = if project.trim().is_empty() {
        "infimum-code".to_string()
    } else {
        project.trim().to_string()
    };
    if let Ok(mut g) = active_conv_cell().lock() {
        *g = ActiveConversation {
            id: format!("conv-{}", now_millis()),
            project: proj,
            title: String::new(),
            imported_plan: String::new(),
            imported_task: String::new(),
            messages: Vec::new(),
        };
    }
}

pub fn save_active_conversation() {
    let Some(dir) = conversations_dir() else {
        return;
    };
    let snapshot = match active_conv_cell().lock() {
        Ok(g) => g.clone(),
        Err(_) => return,
    };
    if snapshot.messages.is_empty() {
        return;
    }
    let clean_id: String = snapshot
        .id
        .chars()
        .filter(|c| c.is_ascii_alphanumeric() || *c == '-' || *c == '_')
        .take(80)
        .collect();
    if clean_id.is_empty() {
        return;
    }
    let title = if snapshot.title.trim().is_empty() {
        snapshot
            .messages
            .iter()
            .find(|m| m.role == "user")
            .map(|m| m.content.replace('\n', " ").chars().take(48).collect::<String>())
            .unwrap_or_else(|| "Диалог".to_string())
    } else {
        snapshot.title.clone()
    };

    let msgs_json: Vec<Value> = snapshot
        .messages
        .iter()
        .map(|m| {
            let mut obj = json!({
                "role": m.role,
                "content": m.content,
            });
            if let Some(map) = obj.as_object_mut() {
                if let Some(meta) = &m.meta {
                    map.insert("meta".to_string(), json!(meta));
                }
                if !m.images.is_empty() {
                    let imgs_v: Vec<Value> = m
                        .images
                        .iter()
                        .map(|im| {
                            json!({
                                "mime_type": im.mime_type,
                                "data": im.data,
                            })
                        })
                        .collect();
                    map.insert("images".to_string(), Value::Array(imgs_v));
                }
                if let Some(ref tl) = m.timeline {
                    if let Ok(tl_v) = serde_json::to_value(tl) {
                        map.insert("timeline".to_string(), tl_v);
                    }
                }
            }
            obj
        })
        .collect();

    let payload = json!({
        "id": clean_id,
        "project": snapshot.project,
        "title": title,
        "updated_at": now_secs(),
        "imported_plan": snapshot.imported_plan,
        "imported_task": snapshot.imported_task,
        "messages": msgs_json,
    });
    let p = dir.join(format!("{clean_id}.json"));
    if let Ok(pretty) = serde_json::to_string_pretty(&payload) {
        let _ = fs::write(p, pretty);
    }
}

pub fn list_antigravity_sessions() -> (Vec<PathBuf>, Vec<String>) {
    let Some(brain) = brain_dir() else {
        return (Vec::new(), Vec::new());
    };
    let mut sessions: Vec<(SystemTime, PathBuf, String)> = Vec::new();
    if let Ok(rd) = fs::read_dir(&brain) {
        for ent in rd.flatten() {
            let dir_p = ent.path();
            if !dir_p.is_dir() {
                continue;
            }
            let dir_name = ent.file_name().to_string_lossy().to_string();
            if dir_name.starts_with('.') || dir_name == "tempmediaStorage" {
                continue;
            }
            let tr = dir_p.join(".system_generated/logs/transcript.jsonl");
            let plan_p = dir_p.join("implementation_plan.md");
            let task_p = dir_p.join("task.md");
            let has_tr = tr.exists();
            let has_plan = plan_p.exists();
            let has_task = task_p.exists();
            if !has_tr && !has_plan && !has_task {
                continue;
            }

            let mtime = fs::metadata(&tr)
                .or_else(|_| fs::metadata(&plan_p))
                .or_else(|_| fs::metadata(&dir_p))
                .and_then(|m| m.modified())
                .unwrap_or(UNIX_EPOCH);

            let id_short: String = dir_name.chars().take(8).collect();
            let mut preview = String::new();
            let mut msg_cnt = 0usize;
            if let Ok(f) = fs::File::open(&tr) {
                let reader = BufReader::new(f);
                for line in reader.lines().map_while(Result::ok) {
                    if let Ok(v) = serde_json::from_str::<Value>(&line) {
                        let ty = v.get("type").and_then(|t| t.as_str()).unwrap_or("");
                        if ty == "USER_INPUT" || ty == "PLANNER_RESPONSE" {
                            msg_cnt += 1;
                            if ty == "USER_INPUT" {
                                if let Some(c) = v.get("content").and_then(|x| x.as_str()) {
                                    let clean = clean_antigravity_user_text(c);
                                    if !clean.is_empty() {
                                        preview = clean.replace('\n', " ").chars().take(36).collect();
                                    }
                                }
                            }
                        }
                    }
                }
            }
            if preview.is_empty() && has_plan {
                if let Ok(plan_txt) = fs::read_to_string(&plan_p) {
                    if let Some(first_line) = plan_txt.lines().find(|l| !l.trim().is_empty()) {
                        preview = first_line.trim_start_matches('#').trim().chars().take(36).collect();
                    }
                }
            }
            let badge = if has_plan { " [План]" } else { "" };
            let label = if preview.is_empty() {
                format!("#{id_short} ({msg_cnt} ш.){badge}")
            } else {
                format!("#{id_short} {preview}{badge}")
            };
            sessions.push((mtime, dir_p, label));
        }
    }
    sessions.sort_by_key(|s| std::cmp::Reverse(s.0));
    sessions.truncate(16);
    let paths = sessions.iter().map(|s| s.1.clone()).collect();
    let labels = sessions.into_iter().map(|s| s.2).collect();
    (paths, labels)
}

pub fn import_antigravity_session_dir(dir_p: &Path) -> Vec<(i32, String)> {
    let dir_name = dir_p
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("agy")
        .to_string();
    let tr = dir_p.join(".system_generated/logs/transcript.jsonl");
    let plan_md = fs::read_to_string(dir_p.join("implementation_plan.md")).unwrap_or_default();
    let task_md = fs::read_to_string(dir_p.join("task.md")).unwrap_or_default();

    let mut conv_msgs: Vec<ConvMessage> = Vec::new();
    let mut ring_msgs: Vec<(i32, String)> = Vec::new();

    if let Ok(f) = fs::File::open(&tr) {
        let reader = BufReader::new(f);
        for line in reader.lines().map_while(Result::ok) {
            let Ok(v) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let ty = v.get("type").and_then(|x| x.as_str()).unwrap_or("");
            if ty != "USER_INPUT" && ty != "PLANNER_RESPONSE" {
                continue;
            }
            let raw_content = v.get("content").and_then(|x| x.as_str()).unwrap_or("");
            let content = if ty == "USER_INPUT" {
                clean_antigravity_user_text(raw_content)
            } else {
                raw_content.trim().to_string()
            };
            if content.is_empty() {
                continue;
            }
            let is_user = ty == "USER_INPUT";
            conv_msgs.push(ConvMessage {
                role: if is_user { "user" } else { "assistant" }.to_string(),
                content: content.clone(),
                meta: Some("История Antigravity".to_string()),
                images: Vec::new(),
                timeline: None,
            });
            ring_msgs.push((if is_user { 0 } else { 1 }, content));
        }
    }

    if conv_msgs.is_empty() && (!plan_md.trim().is_empty() || !task_md.trim().is_empty()) {
        let summary = if !plan_md.trim().is_empty() {
            format!("Импортирован план Antigravity (#{}):\n\n{}", &dir_name[..dir_name.len().min(8)], plan_md.trim())
        } else {
            format!("Импортирован список задач Antigravity (#{}):\n\n{}", &dir_name[..dir_name.len().min(8)], task_md.trim())
        };
        conv_msgs.push(ConvMessage {
            role: "assistant".to_string(),
            content: summary.clone(),
            meta: Some("История Antigravity".to_string()),
            images: Vec::new(),
            timeline: None,
        });
        ring_msgs.push((1, summary));
    }

    // Keep recent 60 messages in conversation state (matches browser studio)
    if conv_msgs.len() > 60 {
        let start = conv_msgs.len() - 60;
        conv_msgs = conv_msgs[start..].to_vec();
    }

    let short_id: String = dir_name.chars().take(16).collect();
    let title = conv_msgs
        .iter()
        .find(|m| m.role == "user")
        .map(|m| m.content.replace('\n', " ").chars().take(48).collect::<String>())
        .unwrap_or_else(|| format!("Antigravity #{}", &dir_name[..dir_name.len().min(8)]));

    if let Ok(mut g) = active_conv_cell().lock() {
        let keep_proj = if g.project.is_empty() {
            "infimum-code".to_string()
        } else {
            g.project.clone()
        };
        *g = ActiveConversation {
            id: format!("agy-{short_id}"),
            project: keep_proj,
            title,
            imported_plan: plan_md,
            imported_task: task_md,
            messages: conv_msgs,
        };
    }
    save_active_conversation();
    ring_msgs
}

// --- Native Agent Tools (Capability-Checked) ---

fn is_safe_path(candidate: &Path, roots: &[PathBuf]) -> bool {
    if roots.is_empty() {
        return false;
    }
    let check_target = if candidate.exists() {
        fs::canonicalize(candidate).ok()
    } else {
        // Walk up to the nearest existing ancestor so creating files inside brand-new
        // nested subdirectories (e.g. `src/main.ifm` in an empty project) succeeds while
        // still rejecting `..` traversal and symlink escapes.
        let mut cur = candidate;
        let mut tail: Vec<std::ffi::OsString> = Vec::new();
        let mut resolved: Option<PathBuf> = None;
        loop {
            if cur.exists() {
                if let Ok(mut base) = fs::canonicalize(cur) {
                    for seg in tail.iter().rev() {
                        base.push(seg);
                    }
                    resolved = Some(base);
                }
                break;
            }
            match cur.file_name() {
                Some(name) => {
                    if name == ".." {
                        break;
                    }
                    tail.push(name.to_os_string());
                }
                None => break,
            }
            match cur.parent() {
                Some(parent) if parent != cur => cur = parent,
                _ => break,
            }
        }
        // Reject if any component in the original candidate was `..` outside canonicalization
        if candidate
            .components()
            .any(|c| matches!(c, std::path::Component::ParentDir))
        {
            None
        } else {
            resolved
        }
    };
    let Some(canon) = check_target else {
        return false;
    };
    roots.iter().any(|r| {
        fs::canonicalize(r)
            .map(|cr| canon.starts_with(cr))
            .unwrap_or(false)
    })
}

/// Blocks reading, writing, or listing secret/credential files even if they exist inside a project folder.
pub fn is_sensitive_path(path: &Path) -> bool {
    for comp in path.components() {
        let s = comp.as_os_str().to_string_lossy().to_ascii_lowercase();
        if matches!(s.as_str(), ".ssh" | ".gnupg" | ".aws" | ".gemini") {
            return true;
        }
    }
    let fname = path
        .file_name()
        .map(|f| f.to_string_lossy().to_ascii_lowercase())
        .unwrap_or_default();
    if fname == ".env"
        || fname.starts_with(".env.")
        || fname.ends_with(".pem")
        || fname.ends_with(".key")
        || fname.ends_with(".p12")
        || fname.ends_with(".pfx")
        || matches!(
            fname.as_str(),
            "id_rsa"
                | "id_ed25519"
                | "id_ecdsa"
                | ".netrc"
                | ".npmrc"
                | ".pypirc"
                | "credentials.json"
        )
    {
        return true;
    }
    false
}

fn workspaces_config_path() -> PathBuf {
    let home = env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
    PathBuf::from(home).join(".config/infimum-code/workspaces.json")
}

/// Prevents accidentally adding system roots, `$HOME` itself, or hidden `$HOME/.*` secret directories as a workspace.
fn is_forbidden_workspace_root(canon: &Path) -> bool {
    if is_sensitive_path(canon) {
        return true;
    }
    let home = env::var("HOME")
        .ok()
        .and_then(|h| fs::canonicalize(h).ok());
    if let Some(ref h) = home {
        if canon == h {
            return true;
        }
        // Block any hidden dot-directory directly inside $HOME (~/.config, ~/.local, ~/.mozilla, ~/.aws, etc.)
        if let Ok(rel) = canon.strip_prefix(h) {
            if let Some(first) = rel.components().next() {
                let first_s = first.as_os_str().to_string_lossy();
                if first_s.starts_with('.') {
                    return true;
                }
            }
        }
    }
    let s = canon.to_string_lossy();
    matches!(
        s.as_ref(),
        "/" | "/home" | "/root" | "/etc" | "/usr" | "/bin" | "/sbin" | "/lib" | "/lib64" | "/var" | "/boot" | "/opt" | "/srv" | "/proc" | "/sys" | "/dev"
    )
}

/// Load user-approved workspace folders (strict manual whitelist).
/// Returns only existing safe directories explicitly added by the user.
pub fn load_allowed_workspaces(policy: &CapabilityPolicy) -> Vec<PathBuf> {
    if !policy.fs_enabled() {
        return Vec::new();
    }
    let cfg = workspaces_config_path();
    let Ok(raw) = fs::read_to_string(&cfg) else {
        return Vec::new();
    };
    let Ok(v) = serde_json::from_str::<Value>(&raw) else {
        return Vec::new();
    };
    let mut out = Vec::new();
    if let Some(arr) = v.get("workspaces").and_then(|w| w.as_array()) {
        for item in arr {
            if let Some(s) = item.as_str() {
                let p = PathBuf::from(s);
                if p.is_dir() {
                    if let Ok(canon) = fs::canonicalize(&p) {
                        if !is_forbidden_workspace_root(&canon) && !out.contains(&canon) {
                            out.push(canon);
                        }
                    }
                }
            }
        }
    }
    out
}

pub fn save_allowed_workspaces(list: &[PathBuf]) -> Result<(), String> {
    let cfg = workspaces_config_path();
    if let Some(parent) = cfg.parent() {
        fs::create_dir_all(parent).map_err(|e| e.to_string())?;
    }
    let strings: Vec<String> = list.iter().map(|p| p.display().to_string()).collect();
    let payload = json!({ "workspaces": strings }).to_string();
    fs::write(&cfg, payload).map_err(|e| e.to_string())?;
    #[cfg(unix)]
    let _ = fs::set_permissions(&cfg, fs::Permissions::from_mode(0o600));
    Ok(())
}

pub fn add_allowed_workspace(policy: &CapabilityPolicy, dir: &Path) -> Result<bool, String> {
    if !dir.is_dir() {
        return Err("Выбранный путь не является папкой".to_string());
    }
    let canon = fs::canonicalize(dir).map_err(|e| e.to_string())?;
    if is_forbidden_workspace_root(&canon) {
        return Err(format!(
            "Нельзя добавить системную или домашнюю корневую папку ({})",
            canon.display()
        ));
    }
    let mut list = load_allowed_workspaces(policy);
    if !list.contains(&canon) {
        list.push(canon);
        save_allowed_workspaces(&list)?;
        return Ok(true);
    }
    Ok(false)
}

pub fn delete_conversations_for_project(project_name: &str) {
    let target = project_name.trim();
    if target.is_empty() {
        return;
    }
    if let Some(dir) = conversations_dir() {
        if let Ok(rd) = fs::read_dir(&dir) {
            for ent in rd.flatten() {
                let p = ent.path();
                if p.extension().and_then(|s| s.to_str()) != Some("json") {
                    continue;
                }
                let Ok(raw) = fs::read_to_string(&p) else {
                    continue;
                };
                let Ok(v) = serde_json::from_str::<Value>(&raw) else {
                    continue;
                };
                let proj = v
                    .get("project")
                    .and_then(|x| x.as_str())
                    .unwrap_or("")
                    .trim();
                if proj == target {
                    let _ = fs::remove_file(&p);
                }
            }
        }
    }
    if let Ok(mut g) = active_conv_cell().lock() {
        if g.project.trim() == target {
            *g = ActiveConversation::default();
        }
    }
}

pub fn remove_allowed_workspace(policy: &CapabilityPolicy, idx: usize) -> Vec<PathBuf> {
    let mut list = load_allowed_workspaces(policy);
    if idx < list.len() {
        let removed = list.remove(idx);
        let _ = save_allowed_workspaces(&list);
        if let Some(name) = removed.file_name().and_then(|s| s.to_str()) {
            let still_exists = list
                .iter()
                .any(|p| p.file_name().and_then(|s| s.to_str()) == Some(name));
            if !still_exists {
                delete_conversations_for_project(name);
            }
        }
    }
    list
}

pub fn resolve_whitelisted_project(policy: &CapabilityPolicy, name: &str) -> Option<PathBuf> {
    let trimmed = name.trim();
    if trimmed.is_empty() {
        return None;
    }
    let list = load_allowed_workspaces(policy);
    for ws in list {
        let base = ws
            .file_name()
            .map(|f| f.to_string_lossy().to_string())
            .unwrap_or_default();
        if base == trimmed || ws.to_string_lossy() == trimmed {
            return Some(ws);
        }
    }
    None
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<String>, depth: usize) {
    if depth > 4 || out.len() >= 220 {
        return;
    }
    let Ok(rd) = fs::read_dir(dir) else {
        return;
    };
    let mut ents: Vec<_> = rd.flatten().collect();
    ents.sort_by_key(|e| e.file_name());
    for ent in ents {
        if out.len() >= 220 {
            break;
        }
        let name = ent.file_name().to_string_lossy().to_string();
        if matches!(
            name.as_str(),
            ".git" | "target" | "node_modules" | ".infimum" | "logs" | "dist"
        ) {
            continue;
        }
        let p = ent.path();
        if is_sensitive_path(&p) {
            continue;
        }
        if p.is_dir() {
            collect_files(root, &p, out, depth + 1);
        } else if let Ok(rel) = p.strip_prefix(root) {
            out.push(rel.display().to_string());
        }
    }
}

const OUTLINE_MAX_ENTRIES: usize = 400;
const OUTLINE_MAX_LINE_CHARS: usize = 300;
const OUTLINE_CORE_AXW: &[u8] = include_bytes!("../../cores/outline_core.ifw");

fn outline_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("outline_core.ifw", OUTLINE_CORE_AXW))
}

fn extract_outline_via_infimum(src: &str, is_rust: bool) -> (Vec<String>, bool) {
    let (engine, module) = outline_core_module();
    let mut store = wasmi::Store::new(engine, ());
    if store.set_fuel(400_000_000).is_err() {
        return (Vec::new(), false);
    }
    let linker = wasmi::Linker::new(engine);
    let Ok(inst) = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
    else {
        return (Vec::new(), false);
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return (Vec::new(), false);
    };
    let Ok(func) = inst.get_typed_func::<(), i32>(&store, "extract_outline_ifm") else {
        return (Vec::new(), false);
    };

    let chars: Vec<u32> = src.chars().take(262_144).map(|c| c as u32).collect();
    {
        let mem = memory.data_mut(&mut store);
        let put = |m: &mut [u8], word: usize, v: i32| {
            let off = word * 4;
            if off + 4 <= m.len() {
                m[off..off + 4].copy_from_slice(&v.to_le_bytes());
            }
        };
        put(mem, 0, chars.len() as i32);
        put(mem, 1, i32::from(is_rust));
        for (i, &cp) in chars.iter().enumerate() {
            put(mem, 25_000 + i, cp as i32);
        }
    }

    let Ok(out_len_raw) = func.call(&mut store, ()) else {
        return (Vec::new(), false);
    };
    let mem = memory.data(&store);
    let get = |word: usize| -> i32 {
        let off = word * 4;
        if off + 4 <= mem.len() {
            i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
        } else {
            0
        }
    };
    let truncated = get(4) != 0;
    let out_len = (out_len_raw.max(0) as usize).min(131_072);
    let mut text = String::with_capacity(out_len);
    for i in 0..out_len {
        let cp = get(287_200 + 1 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            text.push(ch);
        }
    }
    let entries = if text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|l| l.to_string()).collect()
    };
    (entries, truncated)
}

fn extract_ifm_outline(src: &str) -> (Vec<String>, bool) {
    extract_outline_via_infimum(src, false)
}

fn extract_rs_outline(src: &str) -> (Vec<String>, bool) {
    extract_outline_via_infimum(src, true)
}

#[derive(Debug, Clone)]
struct IfmSymbolSpan {
    kind: &'static str,
    name: String,
    start_line: usize, // 1-based inclusive
    end_line: usize,   // 1-based inclusive
}

fn scan_brace_delta(line: &str) -> (i32, bool) {
    let bytes = line.as_bytes();
    let mut i = 0usize;
    let mut delta = 0i32;
    let mut saw_open = false;
    let mut in_str = false;
    let mut in_chr = false;
    while i < bytes.len() {
        let b = bytes[i];
        if in_str {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'"' {
                in_str = false;
            }
            i += 1;
            continue;
        }
        if in_chr {
            if b == b'\\' && i + 1 < bytes.len() {
                i += 2;
                continue;
            }
            if b == b'\'' {
                in_chr = false;
            }
            i += 1;
            continue;
        }
        if b == b'/' && i + 1 < bytes.len() && bytes[i + 1] == b'/' {
            break;
        }
        match b {
            b'"' => in_str = true,
            b'\'' => in_chr = true,
            b'{' => {
                delta += 1;
                saw_open = true;
            }
            b'}' => delta -= 1,
            _ => {}
        }
        i += 1;
    }
    (delta, saw_open)
}

fn parse_ifm_decl_header(trimmed: &str) -> Option<(&'static str, String)> {
    let prefixes: &[(&str, &str)] = &[
        ("export fn ", "fn"),
        ("fn ", "fn"),
        ("export forma ", "forma"),
        ("forma ", "forma"),
        ("struct ", "struct"),
        ("enum ", "enum"),
        ("layout ", "layout"),
        ("const ", "const"),
    ];
    for &(pfx, kind) in prefixes {
        if let Some(rest) = trimmed.strip_prefix(pfx) {
            let name: String = rest
                .chars()
                .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
                .collect();
            if !name.is_empty() {
                return Some((kind, name));
            }
        }
    }
    None
}

fn parse_ifm_symbol_spans(src: &str) -> Vec<IfmSymbolSpan> {
    let lines: Vec<&str> = src.lines().collect();
    let mut spans = Vec::new();
    let mut depth = 0i32;
    let mut cur: Option<(&'static str, String, usize, bool)> = None; // (kind, name, start, opened)

    for (idx, line) in lines.iter().enumerate() {
        let ln = idx + 1;
        let trimmed = line.trim_start();
        if depth == 0 && cur.is_none() {
            if let Some((kind, name)) = parse_ifm_decl_header(trimmed) {
                if kind == "const" {
                    let end = if trimmed.contains(';') {
                        ln
                    } else {
                        let mut e = ln;
                        for (j, l2) in lines.iter().enumerate().skip(idx + 1) {
                            e = j + 1;
                            if l2.contains(';') {
                                break;
                            }
                        }
                        e
                    };
                    spans.push(IfmSymbolSpan {
                        kind,
                        name,
                        start_line: ln,
                        end_line: end,
                    });
                    continue;
                }
                cur = Some((kind, name, ln, false));
            }
        }
        let (delta, saw_open) = scan_brace_delta(line);
        depth = (depth + delta).max(0);
        if let Some((kind, name, start_line, mut opened)) = cur.take() {
            if saw_open || depth > 0 {
                opened = true;
            }
            if opened && depth == 0 {
                spans.push(IfmSymbolSpan {
                    kind,
                    name,
                    start_line,
                    end_line: ln,
                });
            } else {
                cur = Some((kind, name, start_line, opened));
            }
        }
    }
    if let Some((kind, name, start_line, _)) = cur {
        spans.push(IfmSymbolSpan {
            kind,
            name,
            start_line,
            end_line: lines.len().max(start_line),
        });
    }
    spans
}

fn line_contains_ident(line: &str, ident: &str) -> bool {
    if ident.is_empty() {
        return false;
    }
    let code = line.split("//").next().unwrap_or("");
    let bytes = code.as_bytes();
    let ib = ident.as_bytes();
    if bytes.len() < ib.len() {
        return false;
    }
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut i = 0;
    while i + ib.len() <= bytes.len() {
        if &bytes[i..i + ib.len()] == ib {
            let left_ok = i == 0 || !is_id(bytes[i - 1]);
            let right_ok = i + ib.len() == bytes.len() || !is_id(bytes[i + ib.len()]);
            if left_ok && right_ok {
                return true;
            }
        }
        i += 1;
    }
    false
}

fn infimum_template_source(kind: &str) -> Option<(&'static str, &'static str)> {
    match kind {
        "ui" => Some((
            "Infimum.Forma 2.1 Interactive Native UI App (Toggle + Slider + TextInput + layout + if/for + ui_fmt_centered + test)",
            r#"use "std/prelude.ifm";

enum Action {
  Inc = 1,
  Reset = 2,
  ToggleTurbo = 3,
  TabBase = 10,
}

layout AppState {
  tick: word,
  counter: word inv 0..=999999,
  active_tab: word inv 0..=2,
  turbo: word inv 0..=1,
  intensity: word inv 0..=100,
  name_buf: words[40],
}

forma ActionBtn(label: str, act_id: i64, bg_col: i64, hov_col: i64) -[Mem]-> i64 {
  Button(w: 154, h: 42, bg: bg_col, hover_bg: hov_col, radius: 8, on_click: act_id, align: center) {
    Text(label, w: 120, h: 24, size: 15, style: 1, color: pack_rgba(255, 255, 255, 255), align: center)
  }
}

forma MainScreen(vw: i64, vh: i64, tab: i64, turbo: i64)
  req vw >= 640 && vw <= 4096 && vh >= 480 && vh <= 2160 && tab >= 0 && tab <= 2 && turbo >= 0 && turbo <= 1
  -[Mem]-> i64
{
  Box(x: 0, y: 0, w: vw, h: vh, bg: pack_rgba(17, 19, 24, 255), align: center) {
    Col(w: 480, h: 416, pad: 24, gap: 12, bg: pack_rgba(28, 32, 42, 255), border: 1, border_col: pack_rgba(91, 155, 255, 255), radius: 14, align: center) {
      Text("Infimum Native App", w: 380, h: 36, size: 26, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
      Row(w: 324, h: 32, gap: 8, align: center) {
        for i in 0..3 {
          Button(
            w: 102, h: 32,
            bg: if tab == i { pack_rgba(59, 130, 246, 255) } else { pack_rgba(38, 43, 56, 255) },
            hover_bg: pack_rgba(74, 144, 255, 255),
            radius: 7,
            on_click: Action::TabBase + i,
            align: center
          ) {
            Text(if i == 0 { "Overview" } else if i == 1 { "Controls" } else { "About" }, w: 88, h: 20, size: 13, style: 1, color: pack_rgba(248, 250, 255, 255), align: center)
          }
        }
      }
      Box(w: 420, h: 26, align: center) {
        if tab == 0 {
          Text("Zero magic addresses, 100% Z3-verified Forma 2.1 UI", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(160, 174, 198, 255), align: center)
        } else if tab == 1 {
          Text("Interactive Toggle, Slider & TextInput widgets", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(110, 231, 183, 255), align: center)
        } else {
          Text("Built with Infimum Lang & Strata Wasm", w: 420, h: 22, size: 14, style: 0, color: pack_rgba(253, 224, 71, 255), align: center)
        }
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Turbo Mode:", w: 120, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Toggle(id: 20, w: 44, h: 24, checked: turbo == 1, on_change: Action::ToggleTurbo, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      Row(w: 420, h: 28, gap: 12, align: center) {
        Text("Intensity:", w: 90, h: 20, size: 14, style: 1, color: pack_rgba(220, 230, 245, 255))
        Slider(id: 21, w: 240, h: 24, bind: AppState::intensity, min: 0, max: 100, step: 5, bg: pack_rgba(45, 52, 68, 255), border_col: pack_rgba(74, 86, 112, 255), accent: pack_rgba(59, 130, 246, 255))
      }
      TextInput(id: 22, w: 420, h: 36, buf: AppState::name_buf, max_len: 32, placeholder: "Type a label...", bg: pack_rgba(18, 21, 29, 255), border_col: pack_rgba(64, 74, 96, 255), accent: pack_rgba(91, 155, 255, 255), color: pack_rgba(248, 250, 255, 255), size: 14)
      Row(w: 320, h: 42, gap: 12, align: center) {
        ActionBtn("Action", Action::Inc, pack_rgba(59, 130, 246, 255), pack_rgba(96, 165, 250, 255))
        ActionBtn("Reset", Action::Reset, pack_rgba(44, 50, 64, 255), pack_rgba(64, 72, 92, 255))
      }
    }
  }
}

export fn init_app() -[Mem]-> i64 {
  AppState.tick = 0;
  AppState.counter = 0;
  AppState.active_tab = 0;
  AppState.turbo = 1;
  AppState.intensity = 60;
  buf_clear(AppState::name_buf);
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  AppState.tick += 1;

  forma_begin_frame(w, h, mx, my, m_clicked, wheel_dy);
  forma_set_input(m_down, key_char, key_special, AppState.tick);
  let act = MainScreen(w, h, AppState.active_tab, AppState.turbo);
  if act == Action::Inc {
    let step = if AppState.turbo == 1 { 5 } else { 1 };
    AppState.counter = clamp_i64(AppState.counter + step, 0, 999999);
    sfx_click();
  } else if act == Action::Reset {
    AppState.counter = 0;
    AppState.intensity = 60;
    buf_clear(AppState::name_buf);
    sfx_clack();
  } else if act == Action::ToggleTurbo {
    AppState.turbo = 1 - AppState.turbo;
    sfx_click();
  } else if act >= Action::TabBase && act < Action::TabBase + 3 {
    AppState.active_tab = act - Action::TabBase;
    sfx_click();
  }
  gfx_set_cursor(forma_cursor());
  ui_fmt_centered(w / 2, h / 2 + 228, "Clicks: {AppState.counter}  |  Intensity: {AppState.intensity}%  |  Tab #{AppState.active_tab + 1}", 15, 1, pack_rgba(200, 214, 238, 255), 0);
  gfx_end_frame()
}

test "init resets app state" {
  let _ = init_app();
  assert(AppState.counter == 0);
  assert(AppState.active_tab == 0);
  assert(AppState.turbo == 1);
  assert(AppState.intensity == 60);
}
"#,
        )),
        "game2d" => Some((
            "2D GPU Game Starter (auto-placed layout + struct array + for + ui_fmt + test)",
            r#"use "std/prelude.ifm";

struct Orb {
  x: word,
  y: word,
  vx: word,
  vy: word,
  col: word,
}

layout GameState {
  tick: word,
  score: word inv 0..=999999,
  orbs: Orb[12],
}

export fn init_app() -[Mem]-> i64 {
  GameState.tick = 0;
  GameState.score = 0;
  for i in 0..12 {
    GameState.orbs[i].x = 120 + i * 80;
    GameState.orbs[i].y = 200 + (i % 4) * 90;
    GameState.orbs[i].vx = if i % 2 == 0 { 3 } else { -3 };
    GameState.orbs[i].vy = if i % 3 == 0 { 2 } else { -2 };
    GameState.orbs[i].col = pack_rgba(60 + i * 15, 190 - i * 8, 240, 255);
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  GameState.tick += 1;
  gfx_begin_frame(w, h);
  ui_grad_rect(0, 0, w, h, 0, 0, 0, pack_rgba(14, 18, 28, 255), pack_rgba(24, 30, 46, 255), 0);

  for i in 0..12 {
    GameState.orbs[i].x += GameState.orbs[i].vx;
    GameState.orbs[i].y += GameState.orbs[i].vy;
    let ox = GameState.orbs[i].x;
    let oy = GameState.orbs[i].y;
    if ox < 32 || ox > w - 32 {
      GameState.orbs[i].vx = 0 - GameState.orbs[i].vx;
      GameState.score = clamp_i64(GameState.score + 1, 0, 999999);
      sfx_hop();
    }
    if oy < 64 || oy > h - 32 { GameState.orbs[i].vy = 0 - GameState.orbs[i].vy; }
    ui_orb(ox, oy, 22, GameState.orbs[i].col, 0);
  }
  ui_fmt(20, 18, "2D Infimum Game — Bounces: {GameState.score}", 20, 1, pack_rgba(245, 248, 255, 255), 0);
  gfx_end_frame()
}

test "init populates all 12 orbs" {
  let _ = init_app();
  assert(GameState.orbs[0].x == 120);
  assert(GameState.orbs[11].x == 1000);
}
"#,
        )),
        "game3d" => Some((
            "3D Vulkan Mesh Starter (auto-placed layout + value struct Vec3 + Q16.16 fixed + std/mesh3d + PBR + dynamic lights + ui_fmt)",
            r#"use "std/prelude.ifm";
use "std/mesh3d.ifm";

struct Vec3 {
  x: fixed,
  y: fixed,
  z: fixed,
}

struct Pillar {
  x: word,
  z: word,
  h: word,
}

layout WorldState {
  tick: word,
  pos: Vec3,
  yaw: word,
  pitch: word inv -220..=220,
  pillars: Pillar[8],
}

fn vec3_add(a: Vec3, b: Vec3) -> Vec3 {
  Vec3 { x: a.x + b.x, y: a.y + b.y, z: a.z + b.z }
}

export fn init_app() -[Mem]-> i64 {
  WorldState.tick = 0;
  WorldState.pos = Vec3 { x: 0.0, y: 1.7, z: -4.5 };
  WorldState.yaw = 0;
  WorldState.pitch = 0;
  for i in 0..8 {
    let side = if i % 2 == 0 { -3200 } else { 3200 };
    WorldState.pillars[i].x = side;
    WorldState.pillars[i].z = -2000 + (i / 2) * 2600;
    WorldState.pillars[i].h = 4200;
  }
  1
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, m_down: i64, m_clicked: i64, wheel_dy: i64, key_char: i64, key_special: i64) -[Mem, Gpu]-> i64 {
  let w = clamp_i64(vw, 640, 4096);
  let h = clamp_i64(vh, 480, 2160);
  WorldState.tick += 1;
  gfx_begin_frame(w, h);

  if m_clicked == 1 && !pointer_locked() { set_pointer_lock(true); }
  if pointer_locked() {
    WorldState.yaw = (WorldState.yaw + mouse_dx() * 2) & 4095;
    WorldState.pitch = clamp_i64(WorldState.pitch - mouse_dy(), -220, 220);
  }
  let yw = WorldState.yaw;
  let fx = fixed_sin(yw);
  let fz = fixed_cos(yw);
  let mut delta = Vec3 { x: 0.0, y: 0.0, z: 0.0 };
  if key_held(KEY_W) { delta.x += fx * 0.09; delta.z += fz * 0.09; }
  if key_held(KEY_S) { delta.x -= fx * 0.09; delta.z -= fz * 0.09; }
  if key_held(KEY_A) { delta.x -= fz * 0.08; delta.z += fx * 0.08; }
  if key_held(KEY_D) { delta.x += fz * 0.08; delta.z -= fx * 0.08; }
  let pos = vec3_add(WorldState.pos, delta);
  WorldState.pos = pos;

  let cam_x = fixed_to_int(pos.x * 1000.0);
  let cam_y = fixed_to_int(pos.y * 1000.0);
  let cam_z = fixed_to_int(pos.z * 1000.0);
  mesh3d_begin(cam_x, cam_y, cam_z, WorldState.yaw, WorldState.pitch, 75, 2, 0);
  mesh3d_add_light(0, 3600, 1500, 12000, 255, 220, 175, 260);
  mesh3d_box(0, -10000, -200, -10000, 10000, 0, 10000, 6, pack_rgba(190, 185, 175, 255), 220, 0, 2000);
  for i in 0..8 {
    let x = WorldState.pillars[i].x;
    let z = WorldState.pillars[i].z;
    let ph = WorldState.pillars[i].h;
    mesh3d_box(0, x - 450, 0, z - 450, x + 450, ph, z + 450, 2, pack_rgba(220, 210, 195, 255), 235, 0, 1400);
  }

  ui_fmt(18, 16, "Pos: ({pos.x:.1}, {pos.z:.1}) m | WASD: Move | Click: Look", 15, 1, pack_rgba(255, 255, 255, 230), 0);
  gfx_end_frame()
}

test "init places 8 pillars and Vec3 camera in meters" {
  let _ = init_app();
  assert(WorldState.pillars[0].x == -3200);
  assert(WorldState.pillars[1].x == 3200);
  let p = WorldState.pos;
  assert(p.y == 1.7);
  assert(vec3_add(p, Vec3 { x: 1.0, y: 0.3, z: 0.5 }).y == 2.0);
}
"#,
        )),
        _ => None,
    }
}

const EDITOR_CORE_AXW: &[u8] = include_bytes!("../../cores/editor_core.ifw");

fn editor_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("editor_core.ifw", EDITOR_CORE_AXW))
}

fn apply_batch_edits(
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
    if edits.len() > 64 || src_chars.len() > 131_072 || pool_needed > 65_536 {
        return Err((0, "Batch edit exceeds editor_core.ifw buffer limits".to_string()));
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
    let func = inst
        .get_typed_func::<(), i32>(&store, "apply_batch_edits_ifm")
        .map_err(|e| (0, e.to_string()))?;

    let put_i32 = |mem: &mut [u8], word: usize, val: i32| {
        let off = word * 4;
        if off + 4 <= mem.len() {
            mem[off..off + 4].copy_from_slice(&val.to_le_bytes());
        }
    };
    let get_i32 = |mem: &[u8], word: usize| -> i32 {
        let off = word * 4;
        if off + 4 <= mem.len() {
            i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
        } else {
            0
        }
    };

    {
        let mem = memory.data_mut(&mut store);
        put_i32(mem, 4, src_chars.len() as i32);
        put_i32(mem, 5, edits.len() as i32);
        for (i, &cp) in src_chars.iter().enumerate() {
            put_i32(mem, 66_048 + i, cp as i32);
        }
        let mut p_cur = 0usize;
        for (idx, (target, replacement)) in edits.iter().enumerate() {
            let t_off = p_cur;
            let mut t_len = 0usize;
            for ch in target.chars() {
                put_i32(mem, 512 + p_cur, ch as i32);
                p_cur += 1;
                t_len += 1;
            }
            let r_off = p_cur;
            let mut r_len = 0usize;
            for ch in replacement.chars() {
                put_i32(mem, 512 + p_cur, ch as i32);
                p_cur += 1;
                r_len += 1;
            }
            put_i32(mem, 16 + idx, t_off as i32);
            put_i32(mem, 80 + idx, t_len as i32);
            put_i32(mem, 144 + idx, r_off as i32);
            put_i32(mem, 208 + idx, r_len as i32);
        }
    }

    let out_len = func.call(&mut store, ()).map_err(|e| (0, e.to_string()))?;
    let mem = memory.data(&store);
    let err_code = get_i32(mem, 0);
    let err_idx = get_i32(mem, 1).max(0) as usize;
    let err_aux = get_i32(mem, 2).max(0) as usize;

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
        let cp = get_i32(mem, 197_120 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            out.push(ch);
        }
    }
    Ok(out)
}

fn describe_tool_ru(name: &str, args: &Value) -> String {
    let short_path = |key: &str| -> String {
        args.get(key)
            .and_then(|v| v.as_str())
            .map(|s| s.rsplit('/').take(2).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("/"))
            .unwrap_or_default()
    };
    match name {
        "list_project_files" => format!("Смотрю файлы {}…", short_path("project_dir")),
        "search_project" => format!(
            "Ищу «{}»…",
            args.get("query").and_then(|v| v.as_str()).unwrap_or("")
        ),
        "get_file_outline" => format!("Сканирую структуру {}…", short_path("path")),
        "read_project_file" => format!("Читаю {}…", short_path("path")),
        "edit_project_file" => format!("Редактирую {}…", short_path("path")),
        "batch_edit_project_file" => format!("Пакетно редактирую {}…", short_path("path")),
        "write_project_file" => format!("Записываю {}…", short_path("path")),
        "verify_infimum_file" => format!("Проверяю Z3/Strata {}…", short_path("path")),
        "compile_infimum_wasm" => format!("Компилирую .ifw {}…", short_path("path")),
        "run_infimum_capsule" => format!("Запускаю капсулу {}…", short_path("path")),
        "read_infimum_guide" => "Читаю справочник Infimum Lang…".to_string(),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю std/{f}…")
        }
        "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Читаю символ {sym}…")
        }
        "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Обновляю символ {sym}…")
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Ищу вызовы {sym}…")
        }
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            format!("Читаю шаблон {k}…")
        }
        "read_infimum_example" => {
            let ex = args.get("example").and_then(|v| v.as_str()).unwrap_or("list");
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("");
            if f.is_empty() {
                format!("Читаю пример {ex}…")
            } else {
                format!("Читаю пример {ex}/{f}…")
            }
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            format!("Ищу PBR-текстуры «{q}»…")
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            format!("Привязываю текстуру {id} → слот {slot}…")
        }
        "run_project_command" => {
            let c: String = args
                .get("command")
                .and_then(|v| v.as_str())
                .unwrap_or("")
                .chars()
                .take(40)
                .collect();
            format!("Выполняю: {c}…")
        }
        _ => format!("Инструмент {name}…"),
    }
}

fn short_path_label(raw: &str) -> String {
    let p = raw.trim();
    if p.is_empty() {
        return String::new();
    }
    let parts: Vec<&str> = p.split('/').filter(|s| !s.is_empty()).collect();
    if parts.len() <= 2 {
        p.to_string()
    } else {
        format!("{}/{}", parts[parts.len() - 2], parts[parts.len() - 1])
    }
}

fn describe_tool_step(name: &str, args: &Value) -> (i64, String, String) {
    match name {
        "list_project_files" => {
            let d = args.get("project_dir").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Список файлов проекта".to_string(), short_path_label(d))
        }
        "search_project" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Поиск по коду".to_string(), q.chars().take(80).collect())
        }
        "get_file_outline" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Структура файла".to_string(), short_path_label(p))
        }
        "read_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let s = args.get("start_line").and_then(|v| v.as_i64()).unwrap_or(1);
            let e = args.get("end_line").and_then(|v| v.as_i64()).unwrap_or(s + 100);
            (
                STEP_KIND_TOOL,
                "Чтение файла".to_string(),
                format!("{} (L{}..{})", short_path_label(p), s, e),
            )
        }
        "edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Редактирование файла".to_string(), short_path_label(p))
        }
        "batch_edit_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let n = args.get("edits").and_then(|v| v.as_array()).map(|a| a.len()).unwrap_or(0);
            (
                STEP_KIND_TOOL,
                format!("Пакетная правка ({n})"),
                short_path_label(p),
            )
        }
        "write_project_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Запись файла".to_string(), short_path_label(p))
        }
        "verify_infimum_file" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Верификация Z3".to_string(), short_path_label(p))
        }
        "compile_infimum_wasm" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            (STEP_KIND_TOOL, "Компиляция .ifm → .ifw".to_string(), short_path_label(p))
        }
        "run_infimum_capsule" => {
            let p = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(true);
            let title = if headless {
                "Проверка капсулы (headless)"
            } else {
                "Запуск окна infimum-web"
            };
            (STEP_KIND_TOOL, title.to_string(), short_path_label(p))
        }
        "read_infimum_guide" => (
            STEP_KIND_TOOL,
            "Справочник Infimum Lang".to_string(),
            "docs/INFIMUM_LANG_GUIDE.md".to_string(),
        ),
        "read_infimum_std" => {
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("list");
            (
                STEP_KIND_TOOL,
                "Стандартная библиотека".to_string(),
                format!("std/{}", f.trim_start_matches("std/")),
            )
        }
        "get_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                format!("Символ {sym}"),
                short_path_label(p),
            )
        }
        "replace_ifm_symbol" => {
            let sym = args.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let p = args.get("path").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                format!("Замена {sym}"),
                short_path_label(p),
            )
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                "Поиск вызовов".to_string(),
                sym.to_string(),
            )
        }
        "read_infimum_template" => {
            let k = args.get("kind").and_then(|v| v.as_str()).unwrap_or("list");
            (
                STEP_KIND_TOOL,
                "Шаблон Infimum".to_string(),
                k.to_string(),
            )
        }
        "read_infimum_example" => {
            let ex = args.get("example").and_then(|v| v.as_str()).unwrap_or("list");
            let f = args.get("file").and_then(|v| v.as_str()).unwrap_or("");
            let sub = if f.is_empty() { ex.to_string() } else { format!("{ex}/{f}") };
            (
                STEP_KIND_TOOL,
                "Исходник примера".to_string(),
                sub,
            )
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|v| v.as_str()).unwrap_or("");
            (
                STEP_KIND_TOOL,
                "Поиск PBR-текстур".to_string(),
                q.to_string(),
            )
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(0);
            let id = args.get("texture_id").and_then(|v| v.as_str()).unwrap_or("?");
            (
                STEP_KIND_TOOL,
                "PBR-текстура".to_string(),
                format!("slot {slot} -> {id}"),
            )
        }
        "run_project_command" => {
            let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
            (
                STEP_KIND_CMD,
                "Команда терминала".to_string(),
                format!("$ {}", cmd.chars().take(120).collect::<String>()),
            )
        }
        _ => (STEP_KIND_TOOL, format!("Инструмент {name}"), String::new()),
    }
}

const EMBEDDED_INFIMUM_GUIDE: &str = include_str!("../../docs/INFIMUM_LANG_GUIDE.md");

fn studio_root_candidates() -> Vec<PathBuf> {
    let mut roots = Vec::new();
    if let Ok(home) = env::var("INFIMUM_CODE_HOME") {
        if !home.trim().is_empty() {
            roots.push(PathBuf::from(home));
        }
    }
    if let Ok(exe) = env::current_exe() {
        // daemon/target/{release,debug}/infimum-agentd -> infimum-code/
        if let Some(r) = exe
            .parent()
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
            .and_then(|p| p.parent())
        {
            roots.push(r.to_path_buf());
        }
        // <prefix>/bin/infimum-agentd -> <prefix>/
        if let Some(r) = exe.parent().and_then(|p| p.parent()) {
            roots.push(r.to_path_buf());
        }
    }
    roots
}

fn read_canonical_infimum_guide() -> String {
    for root in studio_root_candidates() {
        let p = root.join("docs/INFIMUM_LANG_GUIDE.md");
        if let Ok(text) = fs::read_to_string(&p) {
            if !text.trim().is_empty() {
                return text;
            }
        }
    }
    EMBEDDED_INFIMUM_GUIDE.to_string()
}

/// Find the most recently modified `.ifw` capsule in `active_project` (depth <= 2) and append a
/// `[PLAY:<rel_path>]` tag to `reply` so Infimum Code renders a clickable Play button at the end
/// of the assistant message (works identically on desktop and in Online Infimum Code).
fn append_play_tag_if_axw_present(reply: &mut String, active_project: Option<&str>) {
    if reply.contains("[PLAY:") || reply.trim_start().starts_with("Ошибка") {
        return;
    }
    let Some(proj_str) = active_project.filter(|s| !s.trim().is_empty()) else {
        return;
    };
    let workspace = Path::new(proj_str);
    if !workspace.is_dir() {
        return;
    }
    let mut best: Option<(std::time::SystemTime, String)> = None;
    let scan_dir = |dir: &Path, prefix: &str, best: &mut Option<(std::time::SystemTime, String)>| {
        let Ok(rd) = fs::read_dir(dir) else { return };
        for entry in rd.flatten() {
            let path = entry.path();
            let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
            if path.is_file() && name.ends_with(".ifw") {
                if let Ok(meta) = entry.metadata() {
                    if let Ok(mtime) = meta.modified() {
                        let rel = if prefix.is_empty() {
                            name.to_string()
                        } else {
                            format!("{prefix}/{name}")
                        };
                        if best.as_ref().map(|(t, _)| mtime > *t).unwrap_or(true) {
                            *best = Some((mtime, rel));
                        }
                    }
                }
            }
        }
    };
    scan_dir(workspace, "", &mut best);
    if best.is_none() {
        if let Ok(rd) = fs::read_dir(workspace) {
            for entry in rd.flatten() {
                let path = entry.path();
                let Some(name) = path.file_name().and_then(|s| s.to_str()) else { continue };
                if path.is_dir() && !name.starts_with('.') && name != "target" && name != "node_modules" {
                    scan_dir(&path, name, &mut best);
                }
            }
        }
    }
    if let Some((_, rel)) = best {
        if !reply.ends_with('\n') {
            reply.push('\n');
        }
        reply.push_str(&format!("[PLAY:{rel}]"));
    }
}

pub(crate) fn resolve_infimum_bin(policy: &CapabilityPolicy) -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    let home = env::var("HOME").unwrap_or_default();
    let mut candidates: Vec<PathBuf> = Vec::new();
    // Monorepo / side-by-side binary in the same target directory as `infimum-agentd`
    if let Ok(exe) = env::current_exe() {
        if let Some(dir) = exe.parent() {
            candidates.push(dir.join("infimum"));
        }
    }
    if let Some(r) = policy.allowed_fs_roots.first() {
        candidates.push(r.join("target/release/infimum"));
        candidates.push(r.join("target/debug/infimum"));
        candidates.push(r.join("infimum-lang/target/release/infimum"));
        candidates.push(r.join("infimum-lang/target/debug/infimum"));
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            candidates.push(mono.join("target/release/infimum"));
            candidates.push(mono.join("target/debug/infimum"));
        }
        if let Some(ws) = root.parent() {
            candidates.push(ws.join("infimum-lang/target/release/infimum"));
            candidates.push(ws.join("infimum-lang/target/debug/infimum"));
        }
    }
    if !home.is_empty() {
        candidates.push(PathBuf::from(&home).join(".local/bin/infimum"));
    }
    candidates.push(PathBuf::from("/usr/local/bin/infimum"));
    for c in candidates {
        if c.is_file() {
            return c;
        }
    }
    PathBuf::from("infimum")
}

fn resolve_infimum_std_dir() -> Option<PathBuf> {
    if let Ok(p) = env::var("INFIMUM_STD_DIR") {
        let pb = PathBuf::from(p);
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(p) = env::var("INFIMUM_LANG_DIR") {
        let pb = PathBuf::from(p).join("std");
        if pb.is_dir() {
            return Some(pb);
        }
    }
    if let Ok(exe) = env::current_exe() {
        if let Some(mono) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            let pb = mono.join("crates/infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
        if let Some(ws) = root.parent() {
            let pb = ws.join("infimum-lang/std");
            if pb.is_dir() {
                return Some(pb);
            }
        }
    }
    None
}

fn resolve_z3_bin() -> Option<PathBuf> {
    let home = env::var("HOME").unwrap_or_default();
    let candidates = [
        PathBuf::from(&home).join(".local/bin/z3"),
        PathBuf::from("/usr/local/bin/z3"),
        PathBuf::from("/usr/bin/z3"),
    ];
    candidates.into_iter().find(|p| p.is_file())
}

fn resolve_infimum_web_bin() -> PathBuf {
    if let Ok(p) = env::var("INFIMUM_WEB_BIN") {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return pb;
        }
    }
    if let Ok(exe) = env::current_exe() {
        if exe.is_file()
            && exe
                .file_name()
                .and_then(|s| s.to_str())
                .map(|n| n == "infimum-web")
                .unwrap_or(false)
        {
            return exe;
        }
        if let Some(dir) = exe.parent() {
            let sibling = dir.join("infimum-web");
            if sibling.is_file() {
                return sibling;
            }
        }
    }
    for root in studio_root_candidates() {
        if let Some(mono) = root.parent().and_then(|p| p.parent()) {
            for rel in ["target/release/infimum-web", "target/debug/infimum-web"] {
                let p = mono.join(rel);
                if p.is_file() {
                    return p;
                }
            }
        }
        if let Some(ws) = root.parent() {
            for rel in ["infimum-web/target/release/infimum-web", "infimum-web/target/debug/infimum-web"] {
                let p = ws.join(rel);
                if p.is_file() {
                    return p;
                }
            }
        }
    }
    if let Ok(home) = env::var("HOME") {
        let p = PathBuf::from(home).join(".local/bin/infimum-web");
        if p.is_file() {
            return p;
        }
    }
    PathBuf::from("infimum-web")
}

fn execute_native_tool(
    policy: &CapabilityPolicy,
    default_proj: &Path,
    name: &str,
    args: &Value,
    step_idx: Option<usize>,
) -> Value {
    // STRICT SINGLE-FOLDER JAIL: every tool is locked strictly inside the active project folder
    // AND blocked from sensitive secret files (.env, *.key, etc.).
    let jail_roots = [default_proj.to_path_buf()];
    let is_jailed = |p: &Path| -> bool {
        is_safe_path(p, &jail_roots) && !is_sensitive_path(p)
    };
    let resolve = |raw: &str| -> PathBuf {
        let p = PathBuf::from(raw);
        if p.is_absolute() {
            p
        } else {
            default_proj.join(p)
        }
    };

    match name {
        "list_project_files" => {
            let dir = args
                .get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if !is_jailed(&dir) {
                return json!({ "error": "Access denied: path is outside the active project sandbox" });
            }
            let mut files = Vec::new();
            collect_files(&dir, &dir, &mut files, 0);
            json!({ "project_dir": dir.display().to_string(), "files": files })
        }
        "search_project" => {
            let query = args.get("query").and_then(|s| s.as_str()).unwrap_or("");
            let dir = args
                .get("project_dir")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if query.is_empty() || !is_jailed(&dir) {
                return json!({ "error": "Invalid search parameters or path outside active project sandbox" });
            }
            match Command::new("grep")
                .args([
                    "-rnI",
                    "--exclude-dir=.git",
                    "--exclude-dir=target",
                    "--exclude-dir=node_modules",
                    "--exclude-dir=.infimum",
                    "--exclude=.env",
                    "--exclude=.env.*",
                    "--exclude=*.pem",
                    "--exclude=*.key",
                    "-m",
                    "40",
                    query,
                    ".",
                ])
                .current_dir(&dir)
                .output()
            {
                Ok(o) => {
                    let s: String = String::from_utf8_lossy(&o.stdout).chars().take(3500).collect();
                    json!({ "ok": true, "matches": s })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "get_file_outline" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied: path is outside active project sandbox or sensitive" });
            }
            let ext = p
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_ascii_lowercase();
            if ext != "ifm" && ext != "rs" {
                return json!({
                    "ok": false,
                    "error": format!("get_file_outline supports .ifm and .rs files (got '.{ext}')")
                });
            }
            match fs::read_to_string(&p) {
                Ok(txt) => {
                    let total_lines = txt.lines().count();
                    let (mut entries, truncated) = if ext == "ifm" {
                        extract_ifm_outline(&txt)
                    } else {
                        extract_rs_outline(&txt)
                    };
                    let entries_count = entries.len();
                    if truncated {
                        entries.push(format!(
                            "... [truncated at {OUTLINE_MAX_ENTRIES} entries; use search_project or read_project_file for remaining items]"
                        ));
                    }
                    json!({
                        "ok": true,
                        "path": p.display().to_string(),
                        "total_lines": total_lines,
                        "entries_count": entries_count,
                        "truncated": truncated,
                        "outline": entries.join("\n"),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "read_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
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
                    let s_idx = s.saturating_sub(1).min(lines.len());
                    let e_idx = e.min(lines.len()).max(s_idx);
                    let capped_end = e_idx.min(s_idx + 500);
                    let slice = lines[s_idx..capped_end].join("\n");
                    json!({
                        "path": p.display().to_string(),
                        "start_line": s_idx + 1,
                        "end_line": capped_end,
                        "total_lines": lines.len(),
                        "content": slice
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "edit_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let target = args.get("target").and_then(|s| s.as_str()).unwrap_or("");
            let replacement = args.get("replacement").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Edit denied: path is outside active project sandbox or sensitive" });
            }
            if target.is_empty() {
                return json!({ "error": "target must not be empty" });
            }
            match fs::read_to_string(&p) {
                Ok(existing) => {
                    let count = existing.matches(target).count();
                    if count == 0 {
                        json!({ "ok": false, "error": "Exact target string not found in file" })
                    } else if count > 1 {
                        json!({
                            "ok": false,
                            "error": format!("Target matched {count} places; include more surrounding lines to make it unique")
                        })
                    } else {
                        let updated = existing.replacen(target, replacement, 1);
                        match fs::write(&p, &updated) {
                            Ok(_) => json!({ "ok": true, "replacements": 1, "path": p.display().to_string() }),
                            Err(e) => json!({ "error": e.to_string() }),
                        }
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "batch_edit_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
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
        "write_project_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let content = args.get("content").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Write denied: path is outside active project sandbox or sensitive" });
            }
            if let Some(parent) = p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            match fs::write(&p, content) {
                Ok(_) => json!({ "ok": true, "written_bytes": content.len(), "path": p.display().to_string() }),
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "read_infimum_guide" => {
            let guide = read_canonical_infimum_guide();
            json!({
                "path": "docs/INFIMUM_LANG_GUIDE.md",
                "content": guide,
            })
        }
        "read_infimum_std" => {
            const ALLOWED_STD: &[&str] = &[
                "audio.ifm",
                "buf.ifm",
                "color.ifm",
                "fmt.ifm",
                "gfx.ifm",
                "icons.ifm",
                "input.ifm",
                "json.ifm",
                "markdown.ifm",
                "math.ifm",
                "mem.ifm",
                "mesh3d.ifm",
                "prelude.ifm",
                "sys_mem.ifm",
                "text.ifm",
                "text_edit.ifm",
                "theme.ifm",
                "ui.ifm",
            ];
            let raw = args
                .get("file")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim();
            if raw.is_empty() || raw == "list" {
                return json!({
                    "available_files": ALLOWED_STD,
                    "hint": "Call read_infimum_std with file='gfx.ifm' or file='input.ifm' to read any embedded std module."
                });
            }
            let norm = raw.trim_start_matches("std/").trim_start_matches('/');
            let file_name = if norm.ends_with(".ifm") {
                norm.to_string()
            } else {
                format!("{norm}.ifm")
            };
            if !ALLOWED_STD.contains(&file_name.as_str()) {
                return json!({
                    "error": format!("Unknown std module '{raw}'. Allowed modules: {:?}", ALLOWED_STD)
                });
            }
            let Some(std_dir) = resolve_infimum_std_dir() else {
                return json!({ "error": format!("Standard library directory not found (set INFIMUM_LANG_DIR or INFIMUM_STD_DIR)") });
            };
            let std_path = std_dir.join(&file_name);
            match fs::read_to_string(&std_path) {
                Ok(content) => json!({
                    "path": format!("std/{file_name}"),
                    "content": content,
                }),
                Err(e) => json!({ "error": format!("Failed to read std/{file_name}: {e}") }),
            }
        }
        "get_ifm_symbol" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            if sym.is_empty() {
                return json!({ "error": "Missing required 'name' parameter" });
            }
            match fs::read_to_string(&p) {
                Ok(src) => {
                    let spans = parse_ifm_symbol_spans(&src);
                    if let Some(hit) = spans.iter().find(|s| s.name == sym) {
                        let lines: Vec<&str> = src.lines().collect();
                        let s_idx = hit.start_line.saturating_sub(1);
                        let e_idx = hit.end_line.min(lines.len());
                        let snippet = lines[s_idx..e_idx].join("\n");
                        json!({
                            "ok": true,
                            "path": p.display().to_string(),
                            "kind": hit.kind,
                            "name": hit.name,
                            "start_line": hit.start_line,
                            "end_line": hit.end_line,
                            "source": snippet,
                        })
                    } else {
                        let avail: Vec<String> = spans
                            .iter()
                            .take(60)
                            .map(|s| format!("{} {} (L{}-L{})", s.kind, s.name, s.start_line, s.end_line))
                            .collect();
                        json!({
                            "error": format!("Symbol '{sym}' not found in {}", p.display()),
                            "available_symbols": avail,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "replace_ifm_symbol" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
            let new_src = args.get("new_source").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Write denied: path is outside active project sandbox" });
            }
            if sym.is_empty() || new_src.trim().is_empty() {
                return json!({ "error": "Both 'name' and non-empty 'new_source' are required" });
            }
            match fs::read_to_string(&p) {
                Ok(src) => {
                    let spans = parse_ifm_symbol_spans(&src);
                    if let Some(hit) = spans.iter().find(|s| s.name == sym) {
                        let lines: Vec<&str> = src.lines().collect();
                        let s_idx = hit.start_line.saturating_sub(1);
                        let e_idx = hit.end_line.min(lines.len());
                        let mut out_lines: Vec<String> = Vec::with_capacity(lines.len() + 16);
                        for l in &lines[..s_idx] {
                            out_lines.push((*l).to_string());
                        }
                        let new_lines: Vec<&str> = new_src.trim_end_matches('\n').lines().collect();
                        let new_cnt = new_lines.len();
                        for l in new_lines {
                            out_lines.push(l.to_string());
                        }
                        for l in &lines[e_idx..] {
                            out_lines.push((*l).to_string());
                        }
                        let mut updated = out_lines.join("\n");
                        if src.ends_with('\n') {
                            updated.push('\n');
                        }
                        match fs::write(&p, &updated) {
                            Ok(_) => json!({
                                "ok": true,
                                "path": p.display().to_string(),
                                "name": sym,
                                "replaced_lines": [hit.start_line, hit.end_line],
                                "new_line_count": new_cnt,
                            }),
                            Err(e) => json!({ "error": e.to_string() }),
                        }
                    } else {
                        let avail: Vec<String> = spans.iter().take(60).map(|s| s.name.clone()).collect();
                        json!({
                            "error": format!("Symbol '{sym}' not found in {}", p.display()),
                            "available_symbols": avail,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "find_ifm_callers" => {
            let sym = args.get("symbol").and_then(|s| s.as_str()).unwrap_or("").trim();
            let opt_path = args.get("path").and_then(|s| s.as_str()).unwrap_or("").trim();
            if sym.is_empty() {
                return json!({ "error": "Missing required 'symbol' parameter" });
            }
            let mut ifm_files: Vec<PathBuf> = Vec::new();
            if !opt_path.is_empty() {
                let p = resolve(opt_path);
                if !is_jailed(&p) {
                    return json!({ "error": "Access denied outside active project sandbox" });
                }
                if p.is_file() {
                    ifm_files.push(p);
                }
            } else {
                let mut stack = vec![default_proj.to_path_buf()];
                while let Some(dir) = stack.pop() {
                    if ifm_files.len() >= 64 {
                        break;
                    }
                    let Ok(rd) = fs::read_dir(&dir) else {
                        continue;
                    };
                    for ent in rd.flatten() {
                        let ep = ent.path();
                        let fname = ent.file_name().to_string_lossy().to_string();
                        if fname.starts_with('.') || fname == "target" || fname == "node_modules" {
                            continue;
                        }
                        if ep.is_dir() {
                            stack.push(ep);
                        } else if ep.extension().and_then(|e| e.to_str()) == Some("ifm") {
                            ifm_files.push(ep);
                        }
                    }
                }
                ifm_files.sort();
            }
            let mut hits: Vec<Value> = Vec::new();
            const MAX_HITS: usize = 60;
            for fp in &ifm_files {
                if hits.len() >= MAX_HITS {
                    break;
                }
                let Ok(src) = fs::read_to_string(fp) else {
                    continue;
                };
                let rel = fp
                    .strip_prefix(default_proj)
                    .map(|r| r.display().to_string())
                    .unwrap_or_else(|_| fp.display().to_string());
                let spans = parse_ifm_symbol_spans(&src);
                for (idx, line) in src.lines().enumerate() {
                    if hits.len() >= MAX_HITS {
                        break;
                    }
                    let ln = idx + 1;
                    if !line_contains_ident(line, sym) {
                        continue;
                    }
                    let enclosing = spans
                        .iter()
                        .find(|s| ln >= s.start_line && ln <= s.end_line);
                    if let Some(enc) = enclosing {
                        if enc.name == sym && ln == enc.start_line {
                            continue; // skip own declaration header
                        }
                    }
                    hits.push(json!({
                        "file": rel,
                        "line": ln,
                        "enclosing": enclosing.map(|e| format!("{} {}", e.kind, e.name)).unwrap_or_else(|| "<top-level>".to_string()),
                        "snippet": line.trim().chars().take(160).collect::<String>(),
                    }));
                }
            }
            json!({
                "symbol": sym,
                "files_scanned": ifm_files.len(),
                "count": hits.len(),
                "callers": hits,
            })
        }
        "read_infimum_template" => {
            let kind = args.get("kind").and_then(|s| s.as_str()).unwrap_or("list").trim();
            if kind.is_empty() || kind == "list" {
                return json!({
                    "available_templates": ["ui", "game2d", "game3d"],
                    "hint": "Call read_infimum_template(kind='ui' | 'game2d' | 'game3d') for a Z3-verified starter capsule."
                });
            }
            match infimum_template_source(kind) {
                Some((title, source)) => json!({
                    "kind": kind,
                    "title": title,
                    "source": source,
                }),
                None => json!({
                    "error": format!("Unknown template '{kind}'. Available: ui, game2d, game3d")
                }),
            }
        }
        "read_infimum_example" => {
            const EXAMPLES: &[(&str, &str, &[&str])] = &[
                (
                    "color-lines",
                    "2D PBR board game (9x9 Color Lines 98: typed structs, BFS pathfinding, animations, undo/redo snapshots, procedural SFX)",
                    &["abi.ifm", "logic.ifm", "render.ifm", "main.ifm"],
                ),
                (
                    "lion-herd",
                    "3D PBR savanna game (Lion Herd: std/mesh3d articulated 3D animals, flocking AI, day sky, typed structs, textures.json Cloud PBR, SFX)",
                    &["abi.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "ceres-drift",
                    "3D 6-DOF Zero-G Sci-Fi Mine Shooter (Ceres Drift: 11-chamber 3-deck mine, 13 octagonal tubes, 3D radar, homing missiles, reactor escape, 9x 1K Cloud PBR)",
                    &["abi.ifm", "maze.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "slipgate",
                    "3D PBR FPS (SlipGate E1M1: 6-zone citadel, 24 point lights, 16 articulated 3D monsters, minimap, typed structs, textures.json Cloud PBR)",
                    &["abi.ifm", "world.ifm", "logic.ifm", "render.ifm", "main.ifm", "textures.json"],
                ),
                (
                    "gomoku",
                    "2D PBR board game (Gomoku 18x18 Five-in-a-Row: wooden board, 3D PBR stones, mod ai, 3 AI difficulties, 2P mode, undo, procedural SFX)",
                    &["main.ifm"],
                ),
                (
                    "lang-demos",
                    "Compiler & language feature demos from infimum-lang (Telos Z3 anti-stub, Forma UI & built-in widgets_rt, Flux payment & secret-leak, Strata framebuffer, std_tests)",
                    &["anti_stub_demo.ifm", "forma_ui_demo.ifm", "payment_demo.ifm", "secret_leak_demo.ifm", "std_tests.ifm", "strata_framebuffer_demo.ifm", "widgets_rt.ifm"],
                ),
                (
                    "web-cores",
                    "Verified embedded runtime cores & text demo of infimum-web (audio_core, passport_core, sanitizer_core, text_demo)",
                    &["audio_core.ifm", "passport_core.ifm", "sanitizer_core.ifm", "text_demo.ifm"],
                ),
                (
                    "infimum-code",
                    "Flagship Infimum Code native IDE & AI studio written 100% in Infimum Lang (Forma UI, Toggle/Slider modals, FrameScratch Markdown tables, file viewer, timeline, daemon HTTP client, and verified agent/editor/outline cores)",
                    &[
                        "abi.ifm", "infimum_code.ifm", "sidebar.ifm", "feed.ifm", "composer.ifm",
                        "modals.ifm", "file_viewer.ifm", "markdown.ifm", "timeline.ifm",
                        "text_buf.ifm", "daemon_client.ifm", "theme.ifm", "icons.ifm",
                        "primitives.ifm", "agent_core.ifm", "editor_core.ifm", "outline_core.ifm",
                    ],
                ),
                (
                    "rust-lang",
                    "Complete Rust source code & integration tests of the infimum-lang compiler (lexer, parser, typechecker, Z3 Telos verifier, Forma UI lower/verifier, Strata Wasm/ELF codegen, eval, wasm_api)",
                    &[
                        "src/abi.rs", "src/ast.rs", "src/canon.rs", "src/codegen.rs", "src/diag.rs",
                        "src/eval.rs", "src/forma/ast.rs", "src/forma/lower.rs", "src/forma/mod.rs",
                        "src/forma/parser.rs", "src/forma/verify_ui.rs", "src/lexer.rs", "src/lib.rs",
                        "src/light_verify.rs", "src/main.rs", "src/mcp.rs", "src/parser.rs",
                        "src/showcase.rs", "src/store.rs", "src/strata_elf.rs", "src/strata_wasm.rs",
                        "src/typecheck.rs", "src/verify.rs", "src/wasm.rs", "src/wasm_api.rs",
                        "tests/proof_cache_tests.rs", "tests/sprint1_tests.rs", "tests/sprint2_z3_tests.rs",
                        "tests/sprint3_security_tests.rs", "tests/sprint4_codegen_tests.rs",
                        "tests/sprint5_wasm_timetravel_tests.rs", "tests/sprint6_showcase_tests.rs",
                        "tests/sprint7_strata_tests.rs", "tests/sprint8_modern_syntax_tests.rs",
                        "tests/sprint9_forma_tests.rs",
                    ],
                ),
                (
                    "rust-web",
                    "Complete Rust & WGSL shader source code of the infimum-web native Vulkan/wgpu & WebGPU runtime (GPU renderer, shader.wgsl, mesh3d.wgsl, 3D PBR textures, TrueType glyph atlas, procedural audio, wasmi runtime, clipboard, web_player)",
                    &[
                        "atlas.rs", "audio.rs", "browser_abi.rs", "capabilities.rs", "clipboard.rs",
                        "gpu.rs", "lib.rs", "main.rs", "mesh3d.wgsl", "passport.rs", "runtime.rs",
                        "sanitizer.rs", "shader.wgsl", "textures3d.rs", "web_player.rs",
                    ],
                ),
                (
                    "rust-code",
                    "Complete Rust source code of the infimum-code daemon (infimum-agentd: AI agent loop, sandbox policy, HTTP server, hot-reloading core_loader, studio_abi)",
                    &[
                        "agent.rs", "core_loader.rs", "http_server.rs", "main.rs", "policy.rs",
                        "studio_abi.rs",
                    ],
                ),
                (
                    "infra",
                    "Build/CI scripts (.sh, .py, GitHub Actions .yml), Cargo.toml manifests, Forma UI spec, and Infimum Code system prompt",
                    &[
                        "run_checks.sh", "ci_verify_corpus.sh", "build_studio.sh",
                        "generate_cleanroom_icon.py", "lang-ci.yml", "code-ci.yml",
                        "lang-Cargo.toml", "web-Cargo.toml", "daemon-Cargo.toml",
                        "INFIMUM_FORMA_SPEC.md", "system_prompt.md",
                    ],
                ),
                (
                    "web-site",
                    "Cloudflare Worker, Online Infimum Code Studio, WebGPU Player, and infimum-lang.org frontend sources",
                    &[
                        "src/worker.js", "wrangler.toml",
                        "public/code/code.js", "public/code/agent_worker.js", "public/code/index.html",
                        "public/player/player.js", "public/player/index.html",
                        "public/index.html", "public/app.js", "public/styles.css",
                    ],
                ),
            ];
            let ex = args.get("example").and_then(|s| s.as_str()).unwrap_or("").trim();
            let raw_file = args.get("file").and_then(|s| s.as_str()).unwrap_or("").trim();
            if ex.is_empty() || ex == "list" {
                let list: Vec<Value> = EXAMPLES
                    .iter()
                    .map(|(name, desc, files)| json!({ "example": name, "description": desc, "files": files }))
                    .collect();
                return json!({
                    "available_examples": list,
                    "hint": "Call read_infimum_example(example='rust-web', file='shader.wgsl', start_line=1, end_line=200) or example='infra', file='ci_verify_corpus.sh'."
                });
            }
            let Some((ex_name, ex_desc, allowed_files)) = EXAMPLES.iter().find(|(n, _, _)| *n == ex) else {
                return json!({
                    "error": format!("Unknown example '{ex}'. Available: infimum-code, ceres-drift, gomoku, color-lines, lion-herd, slipgate, lang-demos, web-cores, rust-lang, rust-web, rust-code, infra, web-site")
                });
            };
            if raw_file.is_empty() || raw_file == "list" {
                return json!({
                    "example": ex_name,
                    "description": ex_desc,
                    "files": allowed_files,
                    "hint": format!("Call read_infimum_example(example='{ex_name}', file='{}') to read a source file.", allowed_files.first().copied().unwrap_or("main.ifm"))
                });
            }
            let norm_raw = raw_file.trim_start_matches('/');
            let file_name = if *ex_name == "rust-lang" {
                if allowed_files.contains(&norm_raw) {
                    norm_raw.to_string()
                } else {
                    let with_src = format!("src/{norm_raw}");
                    if allowed_files.contains(&with_src.as_str()) {
                        with_src
                    } else {
                        norm_raw.to_string()
                    }
                }
            } else if *ex_name == "infra" || *ex_name == "web-site" {
                norm_raw.to_string()
            } else {
                let norm = norm_raw
                    .trim_start_matches("daemon/src/")
                    .trim_start_matches("src/");
                if norm.ends_with(".ifm")
                    || norm.ends_with(".json")
                    || norm.ends_with(".rs")
                    || norm.ends_with(".wgsl")
                {
                    norm.to_string()
                } else if ex_name.starts_with("rust-") {
                    format!("{norm}.rs")
                } else {
                    format!("{norm}.ifm")
                }
            };
            if !allowed_files.contains(&file_name.as_str()) {
                return json!({
                    "error": format!("Unknown file '{raw_file}' in example '{ex_name}'. Allowed: {:?}", allowed_files)
                });
            }
            let mut found = None;
            for root in studio_root_candidates() {
                let cands: Vec<PathBuf> = match *ex_name {
                    "lang-demos" => root
                        .parent()
                        .map(|p| {
                            vec![
                                p.join("infimum-lang").join("examples").join(&file_name),
                                p.join("infimum-lang").join("tests").join(&file_name),
                                p.join("infimum-lang").join("src").join("forma").join(&file_name),
                            ]
                        })
                        .unwrap_or_default(),
                    "web-cores" => root
                        .parent()
                        .map(|p| {
                            vec![
                                p.join("infimum-web").join("capsules").join("core").join(&file_name),
                                p.join("infimum-web").join("capsules").join("m2_text_demo").join("src").join(&file_name),
                            ]
                        })
                        .unwrap_or_default(),
                    "infimum-code" => vec![
                        root.join("capsule").join("src").join(&file_name),
                        root.join("cores").join(&file_name),
                    ],
                    "rust-lang" => root
                        .parent()
                        .map(|p| vec![p.join("infimum-lang").join(&file_name)])
                        .unwrap_or_default(),
                    "rust-web" => root
                        .parent()
                        .map(|p| vec![p.join("infimum-web").join("src").join(&file_name)])
                        .unwrap_or_default(),
                    "rust-code" => vec![root.join("daemon").join("src").join(&file_name)],
                    "infra" => {
                        let parent = root.parent();
                        match file_name.as_str() {
                            "run_checks.sh" | "ci_verify_corpus.sh" => parent
                                .map(|p| vec![p.join("infimum-lang").join("scripts").join(&file_name)])
                                .unwrap_or_default(),
                            "build_studio.sh" | "generate_cleanroom_icon.py" => parent
                                .map(|p| vec![p.join("infimum-web").join("scripts").join(&file_name)])
                                .unwrap_or_default(),
                            "lang-ci.yml" => parent
                                .map(|p| vec![p.join("infimum-lang").join(".github").join("workflows").join("ci.yml")])
                                .unwrap_or_default(),
                            "code-ci.yml" => vec![root.join(".github").join("workflows").join("ci.yml")],
                            "lang-Cargo.toml" => parent
                                .map(|p| vec![p.join("infimum-lang").join("Cargo.toml")])
                                .unwrap_or_default(),
                            "web-Cargo.toml" => parent
                                .map(|p| vec![p.join("infimum-web").join("Cargo.toml")])
                                .unwrap_or_default(),
                            "daemon-Cargo.toml" => vec![root.join("daemon").join("Cargo.toml")],
                            "INFIMUM_FORMA_SPEC.md" => parent
                                .map(|p| vec![p.join("infimum-lang").join("docs").join("INFIMUM_FORMA_SPEC.md")])
                                .unwrap_or_default(),
                            "system_prompt.md" => vec![root.join("capsule").join("system_prompt.md")],
                            _ => Vec::new(),
                        }
                    }
                    "web-site" => root
                        .parent()
                        .map(|p| vec![p.join("infimum-site").join(&file_name)])
                        .unwrap_or_default(),
                    _ => {
                        let mut v = vec![
                            root.join("examples").join(ex_name).join("src").join(&file_name),
                            root.join("examples").join(ex_name).join(&file_name),
                        ];
                        if let Some(p) = root.parent() {
                            v.push(p.join("infimum-lang").join("examples").join("showcase").join(ex_name).join("src").join(&file_name));
                            v.push(p.join("infimum-lang").join("examples").join("showcase").join(ex_name).join(&file_name));
                        }
                        v
                    }
                };
                if let Some(hit) = cands.into_iter().find(|c| c.is_file()) {
                    found = Some(hit);
                    break;
                }
            }
            let Some(path) = found else {
                return json!({ "error": format!("Source '{ex_name}/{file_name}' not found") });
            };
            match fs::read_to_string(&path) {
                Ok(content) => {
                    let all_lines: Vec<&str> = content.lines().collect();
                    let total_lines = all_lines.len();
                    let start_arg = args.get("start_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                    let end_arg = args.get("end_line").and_then(|v| v.as_u64()).map(|v| v as usize);
                    if start_arg.is_some() || end_arg.is_some() {
                        let s = start_arg.unwrap_or(1).max(1);
                        let e = end_arg.unwrap_or(total_lines).max(s).min(total_lines);
                        let slice = if s <= total_lines && total_lines > 0 {
                            all_lines[(s - 1)..e].join("\n")
                        } else {
                            String::new()
                        };
                        json!({
                            "example": ex_name,
                            "file": file_name,
                            "path": format!("{ex_name}/{file_name}"),
                            "start_line": s,
                            "end_line": e,
                            "total_lines": total_lines,
                            "content": slice,
                        })
                    } else {
                        json!({
                            "example": ex_name,
                            "file": file_name,
                            "path": format!("{ex_name}/{file_name}"),
                            "total_lines": total_lines,
                            "content": content,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "search_pbr_textures" => {
            let q = args.get("query").and_then(|s| s.as_str()).unwrap_or("").trim();
            let limit = args.get("limit").and_then(|v| v.as_i64()).unwrap_or(10).clamp(1, 20);
            let encoded_q: String = q
                .bytes()
                .map(|b| match b {
                    b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' => (b as char).to_string(),
                    b' ' => "+".to_string(),
                    _ => format!("%{:02X}", b),
                })
                .collect();
            let url = format!("https://infimum-lang.org/api/textures/search?q={encoded_q}&limit={limit}");
            match Command::new("curl")
                .args(["-fsSL", "--max-time", "15", &url])
                .output()
            {
                Ok(o) if o.status.success() => {
                    let body = String::from_utf8_lossy(&o.stdout);
                    serde_json::from_str::<Value>(&body).unwrap_or_else(|_| json!({ "raw": body }))
                }
                Ok(o) => json!({ "error": format!("Texture search failed: {}", String::from_utf8_lossy(&o.stderr).trim()) }),
                Err(e) => json!({ "error": format!("curl failed: {e}") }),
            }
        }
        "set_pbr_texture" => {
            let slot = args.get("slot").and_then(|v| v.as_i64()).unwrap_or(-1);
            let tex_id = args
                .get("texture_id")
                .and_then(|s| s.as_str())
                .unwrap_or("")
                .trim()
                .to_ascii_lowercase();
            if !(0..=11).contains(&slot) {
                return json!({ "error": format!("Invalid slot {slot}. Must be 0..11.") });
            }
            if tex_id.is_empty() || !tex_id.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_') {
                return json!({ "error": format!("Invalid texture_id '{tex_id}'. Must match ^[a-z0-9_]+$.") });
            }
            let home = std::env::var("HOME").unwrap_or_else(|_| "/tmp".to_string());
            let cache_dir = PathBuf::from(&home).join(".cache").join("infimum-web").join("textures").join(&tex_id);
            let proj_tex_dir = resolve("textures");
            if let Err(e) = fs::create_dir_all(&cache_dir).and_then(|_| fs::create_dir_all(&proj_tex_dir)) {
                return json!({ "error": format!("Failed to create texture dirs: {e}") });
            }
            // Remove any previous files bound to this slot in project/textures/
            let slot_prefix = format!("{slot}_");
            if let Ok(entries) = fs::read_dir(&proj_tex_dir) {
                for ent in entries.flatten() {
                    if let Some(fname) = ent.file_name().to_str() {
                        if fname.starts_with(&slot_prefix) {
                            let _ = fs::remove_file(ent.path());
                        }
                    }
                }
            }
            let mut downloaded = Vec::new();
            for kind in ["diff", "norm", "rough"] {
                let cached_file = cache_dir.join(format!("{kind}.jpg"));
                if !cached_file.is_file() || fs::metadata(&cached_file).map(|m| m.len()).unwrap_or(0) == 0 {
                    let url = format!("https://infimum-lang.org/textures/{tex_id}/{kind}.jpg");
                    let tmp = cache_dir.join(format!("{kind}.jpg.tmp"));
                    let ok = Command::new("curl")
                        .args(["-fsSL", "--max-time", "25", "-o", &tmp.display().to_string(), &url])
                        .status()
                        .map(|s| s.success())
                        .unwrap_or(false);
                    if ok && fs::metadata(&tmp).map(|m| m.len()).unwrap_or(0) > 0 {
                        let _ = fs::rename(&tmp, &cached_file);
                    } else {
                        let _ = fs::remove_file(&tmp);
                    }
                }
                if cached_file.is_file() {
                    let dst = proj_tex_dir.join(format!("{slot}_{tex_id}_{kind}.jpg"));
                    #[cfg(unix)]
                    let linked = std::os::unix::fs::symlink(&cached_file, &dst).is_ok();
                    #[cfg(not(unix))]
                    let linked = false;
                    if linked || fs::copy(&cached_file, &dst).is_ok() {
                        downloaded.push(kind);
                    }
                }
            }
            if downloaded.is_empty() {
                return json!({ "error": format!("Texture '{tex_id}' not found on https://infimum-lang.org/textures/") });
            }
            // Also persist declarative textures.json in project root
            let tex_json_path = resolve("textures.json");
            let mut map = fs::read_to_string(&tex_json_path)
                .ok()
                .and_then(|s| serde_json::from_str::<serde_json::Map<String, Value>>(&s).ok())
                .unwrap_or_default();
            map.insert(slot.to_string(), Value::String(tex_id.clone()));
            if let Ok(pretty) = serde_json::to_string_pretty(&Value::Object(map)) {
                let _ = fs::write(&tex_json_path, format!("{pretty}\n"));
            }
            json!({
                "ok": true,
                "slot": slot,
                "texture_id": tex_id,
                "maps": downloaded,
                "cache_dir": cache_dir.display().to_string(),
            })
        }
        "verify_infimum_file" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            let bin = resolve_infimum_bin(policy);
            match Command::new(&bin).args(["verify", &p.display().to_string()]).output() {
                Ok(o) => {
                    let out = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    json!({
                        "ok": o.status.success(),
                        "output": out.chars().take(3500).collect::<String>(),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "compile_infimum_wasm" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            let out_p = match args.get("output").and_then(|s| s.as_str()).filter(|s| !s.trim().is_empty()) {
                Some(o) => resolve(o),
                None => p.with_extension("axw"),
            };
            if !is_jailed(&out_p) {
                return json!({ "error": "Output path denied outside active project sandbox" });
            }
            if let Some(parent) = out_p.parent() {
                let _ = fs::create_dir_all(parent);
            }
            let bin = resolve_infimum_bin(policy);
            match Command::new(&bin)
                .args(["compile-wasm", &p.display().to_string(), "-o", &out_p.display().to_string()])
                .output()
            {
                Ok(o) => {
                    let out = format!(
                        "{}{}",
                        String::from_utf8_lossy(&o.stdout),
                        String::from_utf8_lossy(&o.stderr)
                    );
                    let size_bytes = fs::metadata(&out_p).map(|m| m.len()).unwrap_or(0);
                    json!({
                        "ok": o.status.success(),
                        "output_path": out_p.display().to_string(),
                        "size_bytes": size_bytes,
                        "output": out.chars().take(3500).collect::<String>(),
                    })
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        "run_infimum_capsule" => {
            let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
            let p = resolve(raw);
            if !is_jailed(&p) {
                return json!({ "error": "Access denied outside active project sandbox" });
            }
            if !p.is_file() {
                return json!({ "error": format!("Capsule file not found: {}", p.display()) });
            }
            let headless = args.get("headless").and_then(|v| v.as_bool()).unwrap_or(true);
            let web_bin = resolve_infimum_web_bin();
            if headless {
                match Command::new(&web_bin)
                    .args(["--headless-check", &p.display().to_string()])
                    .output()
                {
                    Ok(o) => {
                        let out = format!(
                            "{}{}",
                            String::from_utf8_lossy(&o.stdout),
                            String::from_utf8_lossy(&o.stderr)
                        );
                        json!({
                            "ok": o.status.success(),
                            "mode": "headless",
                            "output": out.chars().take(3500).collect::<String>(),
                        })
                    }
                    Err(e) => json!({ "error": e.to_string() }),
                }
            } else {
                match Command::new(&web_bin)
                    .arg(&p)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                {
                    Ok(child) => json!({
                        "ok": true,
                        "mode": "window",
                        "pid": child.id(),
                        "path": p.display().to_string(),
                        "message": format!("Opened native infimum-web window for {}", p.display()),
                    }),
                    Err(e) => json!({ "error": e.to_string() }),
                }
            }
        }
        "run_project_command" => {
            if !policy.allow_proc {
                return json!({ "error": "Proc capability denied (launch with --allow-proc)" });
            }
            let cmd = args.get("command").and_then(|s| s.as_str()).unwrap_or("");
            let cwd = args
                .get("cwd")
                .and_then(|s| s.as_str())
                .filter(|s| !s.is_empty())
                .map(resolve)
                .unwrap_or_else(|| default_proj.to_path_buf());
            if !is_jailed(&cwd) {
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
            command
                .args([
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
                                if cmd_start.elapsed() > Duration::from_secs(180) {
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
                    // On Ubuntu 24.04 launched from GNOME desktop, AppArmor may block unprivileged
                    // user namespaces for `bwrap`. When that happens, fall back to a scrubbed-env
                    // (`env_clear`) bash inside `cwd_canon` with path-escape & network-tool guards.
                    let is_userns_blocked = exit_code != 0
                        && all_output.is_empty()
                        && (bwrap_err.contains("uid map")
                            || bwrap_err.contains("namespace")
                            || bwrap_err.contains("Permission denied")
                            || bwrap_err.contains("Operation not permitted")
                            || bwrap_err.contains("bwrap:"));
                    if is_userns_blocked && !killed_by_user && !timed_out {
                        let lower_cmd = cmd.to_ascii_lowercase();
                        let forbidden = lower_cmd.contains("..")
                            || lower_cmd.contains("/etc/")
                            || lower_cmd.contains("curl ")
                            || lower_cmd.contains("wget ")
                            || lower_cmd.contains("ssh ")
                            || lower_cmd.contains("scp ")
                            || lower_cmd.contains("nc ")
                            || lower_cmd.contains("find /");
                        if forbidden {
                            all_output = format!("Sandbox denied potentially unsafe command in fallback mode: {cmd}");
                            exit_code = 1;
                        } else {
                            let mut fb_path = String::from("/usr/local/bin:/usr/bin:/bin");
                            if let Some(p) = infimum_bin.parent() {
                                fb_path = format!("{}:{fb_path}", p.display());
                            }
                            if let Some(p) = z3_bin.as_ref().and_then(|z| z.parent()) {
                                fb_path = format!("{}:{fb_path}", p.display());
                            }
                            if cargo_dir.is_dir() {
                                fb_path = format!("{}/bin:{fb_path}", cargo_dir.display());
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
                            if let Ok(out) = fb.output() {
                                exit_code = out.status.code().unwrap_or(-1);
                                all_output = String::from_utf8_lossy(&out.stdout).trim_end().to_string();
                                let se = String::from_utf8_lossy(&out.stderr).trim().to_string();
                                if !se.is_empty() {
                                    if !all_output.is_empty() {
                                        all_output.push('\n');
                                    }
                                    all_output.push_str(&se);
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
                    let trunc: String = full.chars().take(5000).collect();
                    if killed_by_user {
                        json!({
                            "exit_code": exit_code,
                            "killed_by_user": true,
                            "output": trunc,
                            "note": "Команда принудительно прервана пользователем через кнопку прерывания."
                        })
                    } else if timed_out {
                        json!({
                            "exit_code": exit_code,
                            "timed_out": true,
                            "output": trunc,
                            "note": "Команда превысила лимит времени (180 с) и была остановлена."
                        })
                    } else {
                        json!({
                            "exit_code": exit_code,
                            "output": trunc,
                        })
                    }
                }
                Err(e) => json!({ "error": e.to_string() }),
            }
        }
        _ => json!({ "error": format!("Unknown tool '{name}'") }),
    }
}

const AGENT_CORE_AXW: &[u8] = include_bytes!("../../cores/agent_core.ifw");

fn agent_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("agent_core.ifw", AGENT_CORE_AXW))
}

const EMBEDDED_STUDIO_SYSTEM_PROMPT: &str = include_str!("../../capsule/system_prompt.md");

/// Load the live `capsule/system_prompt.md` on every request (with embedded fallback)
/// so prompt tweaks take effect immediately without recompiling Rust or restarting Infimum Code.
fn render_studio_system_prompt(proj_dir: &Path, imported_plan: &str, imported_task: &str) -> String {
    let mut tpl = None;
    if let Ok(exe) = env::current_exe() {
        for root in [
            exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()).and_then(|p| p.parent()),
            exe.parent().and_then(|p| p.parent()),
        ]
        .into_iter()
        .flatten()
        {
            if let Ok(s) = fs::read_to_string(root.join("capsule/system_prompt.md")) {
                if !s.trim().is_empty() {
                    tpl = Some(s);
                    break;
                }
            }
        }
    }
    if tpl.is_none() {
        if let Ok(home) = env::var("INFIMUM_CODE_HOME") {
            if let Ok(s) = fs::read_to_string(Path::new(&home).join("capsule/system_prompt.md")) {
                if !s.trim().is_empty() {
                    tpl = Some(s);
                }
            }
        }
    }
    if tpl.is_none() {
        for root in studio_root_candidates() {
            if let Ok(s) = fs::read_to_string(root.join("capsule/system_prompt.md")) {
                if !s.trim().is_empty() {
                    tpl = Some(s);
                    break;
                }
            }
        }
    }
    let raw = tpl.unwrap_or_else(|| EMBEDDED_STUDIO_SYSTEM_PROMPT.to_string());
    let proj_s = proj_dir.display().to_string();
    let plan_s = if imported_plan.trim().is_empty() { "(нет)" } else { imported_plan.trim() };
    let task_s = if imported_task.trim().is_empty() { "(нет)" } else { imported_task.trim() };
    raw.replace("{proj}", &proj_s)
        .replace("{plan}", plan_s)
        .replace("{task}", task_s)
}

fn write_ifm_u32_buf(mem: &mut [u8], base_word: usize, s: &str, max_chars: usize) -> usize {
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

fn calc_cost_milli_ils_via_infimum(
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
    let Ok(func) = inst.get_typed_func::<(i32, i32, i32), i32>(&store, "calc_cost_milli_ils_ifm")
    else {
        return 0;
    };
    let prov_word = inst
        .get_typed_func::<(), i32>(&store, "get_prov_buf_word")
        .ok()
        .and_then(|f| f.call(&mut store, ()).ok())
        .map(|w| w.max(0) as usize)
        .unwrap_or(16);
    let model_word = inst
        .get_typed_func::<(), i32>(&store, "get_model_buf_word")
        .ok()
        .and_then(|f| f.call(&mut store, ()).ok())
        .map(|w| w.max(0) as usize)
        .unwrap_or(96);
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

fn openai_provider_base_url(provider: &str) -> &'static str {
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
fn run_openai_compat_native_agent_loop<F>(
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
    const MAX_OPENAI_AGENT_STEPS: usize = 80;
    let t0 = Instant::now();
    ASYNC_CANCELLED.store(false, Ordering::SeqCst);
    reset_live_timeline(MAX_OPENAI_AGENT_STEPS as i64);

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

    let sys_prompt = render_studio_system_prompt(&proj_dir, &imported_plan, &imported_task);

    let tools_decl = json!([
        {
            "type": "function",
            "function": {
                "name": "list_project_files",
                "description": "List files inside a project directory under ~/Projects.",
                "parameters": { "type": "object", "properties": { "project_dir": { "type": "string" } } }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_project",
                "description": "Fast grep search across files in a project directory.",
                "parameters": {
                    "type": "object",
                    "properties": { "query": { "type": "string" }, "project_dir": { "type": "string" } },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_file_outline",
                "description": "Return a compact structural outline of an .ifm or .rs file without reading its full body.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_project_file",
                "description": "Read a specific line range from a file inside ~/Projects.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "start_line": { "type": "integer" },
                        "end_line": { "type": "integer" }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "edit_project_file",
                "description": "Surgically replace an exact target snippet in an existing file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "target": { "type": "string" },
                        "replacement": { "type": "string" }
                    },
                    "required": ["path", "target", "replacement"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "batch_edit_project_file",
                "description": "Atomically apply multiple independent exact target->replacement edits to a single file in one turn.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "edits": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "properties": {
                                    "target": { "type": "string" },
                                    "replacement": { "type": "string" }
                                },
                                "required": ["target", "replacement"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "write_project_file",
                "description": "Create or overwrite a small file inside ~/Projects.",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" }, "content": { "type": "string" } },
                    "required": ["path", "content"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_infimum_guide",
                "description": "Read the canonical Infimum Lang & infimum-web reference guide (syntax, Telos/Flux/Forma/Strata, GPU protocol, compiler CLI, and capsule launch workflow). Call this first in a new conversation before writing or compiling Infimum code.",
                "parameters": {
                    "type": "object",
                    "properties": {}
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_infimum_std",
                "description": "Read the exact source code of any built-in Infimum Lang standard library module (`std/<name>.ifm`, e.g. `mesh3d.ifm`, `gfx.ifm`, `input.ifm`, `math.ifm`, `audio.ifm`, `text.ifm`, `prelude.ifm`). Pass empty string or `list` to list all available std modules.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "file": {
                            "type": "string",
                            "description": "Module name such as 'gfx.ifm', 'input.ifm', 'math.ifm', or 'list'."
                        }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "get_ifm_symbol",
                "description": "Fetch the exact source code and line range of a single top-level Infimum declaration (fn, forma, struct, enum, layout, const) by name. Much faster and cheaper in tokens than reading a large .ifm file.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the .ifm file." },
                        "name": { "type": "string", "description": "Exact symbol name (e.g. 'step_ui', 'Sheep', 'AppMem')." }
                    },
                    "required": ["path", "name"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "replace_ifm_symbol",
                "description": "Atomically replace an entire top-level Infimum declaration (fn, forma, struct, enum, layout, const) by symbol name, avoiding brittle whitespace matching.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string", "description": "Relative path to the .ifm file." },
                        "name": { "type": "string", "description": "Exact symbol name to replace." },
                        "new_source": { "type": "string", "description": "Complete replacement declaration source code." }
                    },
                    "required": ["path", "name", "new_source"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "find_ifm_callers",
                "description": "Find all references/callers of a symbol across .ifm files in the project (or a single file), returning enclosing function names and line numbers.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "symbol": { "type": "string", "description": "Symbol name to search for." },
                        "path": { "type": "string", "description": "Optional relative path to restrict search to one .ifm file." }
                    },
                    "required": ["symbol"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_infimum_template",
                "description": "Read a compact, Z3-verified starter capsule template ('ui', 'game2d', 'game3d', or 'list').",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "kind": { "type": "string", "description": "'ui', 'game2d', 'game3d', or 'list'." }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "read_infimum_example",
                "description": "Read reference source code of open-source Infimum ecosystem apps, cores, and Rust internals: Infimum Code IDE ('infimum-code'), games ('ceres-drift', 'gomoku', 'color-lines', 'lion-herd', 'slipgate'), compiler demos ('lang-demos'), infimum-web cores ('web-cores'), and full Rust sources of infimum-lang ('rust-lang'), infimum-web ('rust-web'), and infimum-agentd ('rust-code'). Pass example='list' to list all collections and files.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "example": { "type": "string", "description": "'infimum-code', 'ceres-drift', 'gomoku', 'color-lines', 'lion-herd', 'slipgate', 'lang-demos', 'web-cores', 'rust-lang', 'rust-web', 'rust-code', 'infra', 'web-site', or 'list'." },
                        "file": { "type": "string", "description": "Source file name (e.g. 'main.ifm', 'src/parser.rs', 'shader.wgsl', 'ci_verify_corpus.sh', 'src/worker.js') or 'list'." },
                        "start_line": { "type": "integer", "description": "Optional 1-based start line for reading large files." },
                        "end_line": { "type": "integer", "description": "Optional 1-based end line (inclusive) for reading large files." }
                    }
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "verify_infimum_file",
                "description": "Verify an Infimum (.ifm) file with Z3 Telos/Flux/Forma contracts (`infimum verify`).",
                "parameters": {
                    "type": "object",
                    "properties": { "path": { "type": "string" } },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "search_pbr_textures",
                "description": "Search 800+ free CC0 1K PBR textures (Poly Haven catalog via infimum-lang.org) by keywords such as 'sci-fi metal', 'brick', 'concrete', 'wood', 'rock'.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "query": { "type": "string", "description": "Search keywords, e.g. 'rusty metal' or 'stone brick'" },
                        "limit": { "type": "integer", "description": "Max results (1..20, default 10)" }
                    },
                    "required": ["query"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "set_pbr_texture",
                "description": "Bind a CC0 1K PBR texture ID (from search_pbr_textures) to a 3D texture layer slot (0..11). Downloads once to ~/.cache/infimum-web/textures/ and links into project textures/ + textures.json.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "slot": { "type": "integer", "description": "Texture layer slot 0..11" },
                        "texture_id": { "type": "string", "description": "Exact texture id from search_pbr_textures" }
                    },
                    "required": ["slot", "texture_id"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "compile_infimum_wasm",
                "description": "Compile an Infimum (.ifm) file into a WebAssembly capsule (.ifw) using `infimum compile-wasm` inside the active project.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "output": { "type": "string", "description": "Optional output .ifw path inside the project (defaults to <path>.ifw)." }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_infimum_capsule",
                "description": "Run a compiled .ifw capsule in native infimum-web (sandboxed Gpu+Mem only). Use headless=true for smoke-check verification, or headless=false to open the GUI window for the user.",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": { "type": "string" },
                        "headless": { "type": "boolean", "description": "If true (default), run --headless-check and return primitive/fuel stats. If false, launch the native GUI window." }
                    },
                    "required": ["path"]
                }
            }
        },
        {
            "type": "function",
            "function": {
                "name": "run_project_command",
                "description": "Run a shell command in the workspace.",
                "parameters": {
                    "type": "object",
                    "properties": { "cwd": { "type": "string" }, "command": { "type": "string" } },
                    "required": ["command"]
                }
            }
        }
    ]);

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

    for step in 0..MAX_OPENAI_AGENT_STEPS {
        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break;
        }
        let is_last_step = step + 1 >= MAX_OPENAI_AGENT_STEPS;
        timeline_set_round((step + 1) as i64);
        set_status(format!("Шаг {}/{MAX_OPENAI_AGENT_STEPS}: думаю ({display_model})…", step + 1));

        let body = if is_last_step {
            messages.push(json!({
                "role": "user",
                "content": "Подведи краткий итог выполненных изменений и ответь пользователю на русском языке."
            }));
            json!({
                "model": model_id,
                "messages": messages,
                "stream": false,
            })
        } else {
            json!({
                "model": model_id,
                "messages": messages,
                "tools": tools_decl,
                "tool_choice": "auto",
                "stream": false,
            })
        };

        let pid_self = std::process::id();
        let req_path = PathBuf::from(format!("/tmp/infimum_web_oai_req_{pid_self}.json"));
        let hdr_path = PathBuf::from(format!("/tmp/infimum_web_oai_hdr_{pid_self}.cfg"));
        let _ = fs::write(&req_path, body.to_string());
        let mut hdrs = format!(
            "header = \"Content-Type: application/json\"\nheader = \"Authorization: Bearer {api_key}\"\n"
        );
        if provider == "openrouter" {
            hdrs.push_str("header = \"HTTP-Referer: https://infimum-lang.org\"\n");
            hdrs.push_str("header = \"X-Title: Infimum Code\"\n");
        }
        let _ = fs::write(&hdr_path, hdrs);
        #[cfg(unix)]
        let _ = fs::set_permissions(&hdr_path, fs::Permissions::from_mode(0o600));

        let mut parsed_resp: Option<Value> = None;
        let mut last_err_msg = String::new();
        for attempt in 0..3usize {
            if ASYNC_CANCELLED.load(Ordering::SeqCst) {
                break;
            }
            if attempt > 0 {
                set_status(format!(
                    "Шаг {}/{MAX_OPENAI_AGENT_STEPS}: повтор запроса ({display_model}, попытка {}/3)…",
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
                    "--connect-timeout",
                    "20",
                    "--max-time",
                    "240",
                    "-K",
                    &hdr_path.display().to_string(),
                    "-d",
                    &format!("@{}", req_path.display()),
                    &url,
                ])
                .stdout(Stdio::piped())
                .stderr(Stdio::null())
                .spawn();

            let curl_out = match spawn_res {
                Ok(child) => {
                    ASYNC_GEMINI_PID.store(child.id(), Ordering::SeqCst);
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
        let _ = fs::remove_file(&hdr_path);

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
    const MAX_OPENAI_AGENT_STEPS: usize = 80;
    let t0 = Instant::now();
    ASYNC_CANCELLED.store(false, Ordering::SeqCst);
    reset_live_timeline(MAX_OPENAI_AGENT_STEPS as i64);

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

    let sys_prompt = render_studio_system_prompt(&proj_dir, &imported_plan, &imported_task);

    let tools_decl = json!([{
        "functionDeclarations": [
            {
                "name": "list_project_files",
                "description": "List files inside a project directory under ~/Projects.",
                "parameters": { "type": "OBJECT", "properties": { "project_dir": { "type": "STRING" } } }
            },
            {
                "name": "search_project",
                "description": "Fast grep search across files in a project directory.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": { "query": { "type": "STRING" }, "project_dir": { "type": "STRING" } },
                    "required": ["query"]
                }
            },
            {
                "name": "get_file_outline",
                "description": "Return a compact structural outline (declarations, signatures, effects, req/ens presence, and line ranges) of an .ifm or .rs file without reading its full body.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "read_project_file",
                "description": "Read a specific line range from a file inside ~/Projects.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "start_line": { "type": "INTEGER" },
                        "end_line": { "type": "INTEGER" }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "edit_project_file",
                "description": "Surgically replace an exact target snippet in an existing file.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "target": { "type": "STRING" },
                        "replacement": { "type": "STRING" }
                    },
                    "required": ["path", "target", "replacement"]
                }
            },
            {
                "name": "batch_edit_project_file",
                "description": "Atomically apply multiple independent exact target->replacement edits to a single file in one turn. Validates all edits first; rolls back completely if any target is missing, ambiguous, or overlapping.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "edits": {
                            "type": "ARRAY",
                            "items": {
                                "type": "OBJECT",
                                "properties": {
                                    "target": { "type": "STRING" },
                                    "replacement": { "type": "STRING" }
                                },
                                "required": ["target", "replacement"]
                            }
                        }
                    },
                    "required": ["path", "edits"]
                }
            },
            {
                "name": "write_project_file",
                "description": "Create or overwrite a small file inside ~/Projects.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": { "path": { "type": "STRING" }, "content": { "type": "STRING" } },
                    "required": ["path", "content"]
                }
            },
            {
                "name": "read_infimum_guide",
                "description": "Read the canonical Infimum Lang & infimum-web reference guide (syntax, Telos/Flux/Forma/Strata, GPU protocol, compiler CLI, and capsule launch workflow). Call this first in a new conversation before writing or compiling Infimum code.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {}
                }
            },
            {
                "name": "read_infimum_std",
                "description": "Read the exact source code of any built-in Infimum Lang standard library module (`std/<name>.ifm`, e.g. `mesh3d.ifm`, `gfx.ifm`, `input.ifm`, `math.ifm`, `audio.ifm`, `text.ifm`, `prelude.ifm`). Pass empty string or `list` to list all available std modules.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "file": {
                            "type": "STRING",
                            "description": "Module name such as 'gfx.ifm', 'input.ifm', 'math.ifm', or 'list'."
                        }
                    }
                }
            },
            {
                "name": "get_ifm_symbol",
                "description": "Fetch the exact source code and line range of a single top-level Infimum declaration (fn, forma, struct, enum, layout, const) by name. Much faster and cheaper in tokens than reading a large .ifm file.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path to the .ifm file." },
                        "name": { "type": "STRING", "description": "Exact symbol name (e.g. 'step_ui', 'Sheep', 'AppMem')." }
                    },
                    "required": ["path", "name"]
                }
            },
            {
                "name": "replace_ifm_symbol",
                "description": "Atomically replace an entire top-level Infimum declaration (fn, forma, struct, enum, layout, const) by symbol name, avoiding brittle whitespace matching.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING", "description": "Relative path to the .ifm file." },
                        "name": { "type": "STRING", "description": "Exact symbol name to replace." },
                        "new_source": { "type": "STRING", "description": "Complete replacement declaration source code." }
                    },
                    "required": ["path", "name", "new_source"]
                }
            },
            {
                "name": "find_ifm_callers",
                "description": "Find all references/callers of a symbol across .ifm files in the project (or a single file), returning enclosing function names and line numbers.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "symbol": { "type": "STRING", "description": "Symbol name to search for." },
                        "path": { "type": "STRING", "description": "Optional relative path to restrict search to one .ifm file." }
                    },
                    "required": ["symbol"]
                }
            },
            {
                "name": "read_infimum_template",
                "description": "Read a compact, Z3-verified starter capsule template ('ui', 'game2d', 'game3d', or 'list').",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "kind": { "type": "STRING", "description": "'ui', 'game2d', 'game3d', or 'list'." }
                    }
                }
            },
            {
                "name": "read_infimum_example",
                "description": "Read reference source code of open-source Infimum ecosystem apps, cores, and Rust internals: Infimum Code IDE ('infimum-code'), games ('ceres-drift', 'gomoku', 'color-lines', 'lion-herd', 'slipgate'), compiler demos ('lang-demos'), infimum-web cores ('web-cores'), and full Rust sources of infimum-lang ('rust-lang'), infimum-web ('rust-web'), and infimum-agentd ('rust-code'). Pass example='list' to list all collections and files.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "example": { "type": "STRING", "description": "'infimum-code', 'ceres-drift', 'gomoku', 'color-lines', 'lion-herd', 'slipgate', 'lang-demos', 'web-cores', 'rust-lang', 'rust-web', 'rust-code', 'infra', 'web-site', or 'list'." },
                        "file": { "type": "STRING", "description": "Source file name (e.g. 'main.ifm', 'src/parser.rs', 'shader.wgsl', 'ci_verify_corpus.sh', 'src/worker.js') or 'list'." },
                        "start_line": { "type": "INTEGER", "description": "Optional 1-based start line for reading large files." },
                        "end_line": { "type": "INTEGER", "description": "Optional 1-based end line (inclusive) for reading large files." }
                    }
                }
            },
            {
                "name": "verify_infimum_file",
                "description": "Verify an Infimum (.ifm) file with Z3 Telos/Flux/Forma contracts (`infimum verify`).",
                "parameters": {
                    "type": "OBJECT",
                    "properties": { "path": { "type": "STRING" } },
                    "required": ["path"]
                }
            },
            {
                "name": "search_pbr_textures",
                "description": "Search 800+ free CC0 1K PBR textures (Poly Haven catalog via infimum-lang.org) by keywords such as 'sci-fi metal', 'brick', 'concrete', 'wood', 'rock'.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "query": { "type": "STRING", "description": "Search keywords, e.g. 'rusty metal' or 'stone brick'" },
                        "limit": { "type": "INTEGER", "description": "Max results (1..20, default 10)" }
                    },
                    "required": ["query"]
                }
            },
            {
                "name": "set_pbr_texture",
                "description": "Bind a CC0 1K PBR texture ID (from search_pbr_textures) to a 3D texture layer slot (0..11). Downloads once to ~/.cache/infimum-web/textures/ and links into project textures/ + textures.json.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "slot": { "type": "INTEGER", "description": "Texture layer slot 0..11" },
                        "texture_id": { "type": "STRING", "description": "Exact texture id from search_pbr_textures" }
                    },
                    "required": ["slot", "texture_id"]
                }
            },
            {
                "name": "compile_infimum_wasm",
                "description": "Compile an Infimum (.ifm) file into a WebAssembly capsule (.ifw) using `infimum compile-wasm` inside the active project.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "output": { "type": "STRING", "description": "Optional output .ifw path inside the project (defaults to <path>.ifw)." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "run_infimum_capsule",
                "description": "Run a compiled .ifw capsule in native infimum-web (sandboxed Gpu+Mem only). Use headless=true for smoke-check verification, or headless=false to open the GUI window for the user.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": {
                        "path": { "type": "STRING" },
                        "headless": { "type": "BOOLEAN", "description": "If true (default), run --headless-check and return primitive/fuel stats. If false, launch the native GUI window." }
                    },
                    "required": ["path"]
                }
            },
            {
                "name": "run_project_command",
                "description": "Run a shell command in the workspace.",
                "parameters": {
                    "type": "OBJECT",
                    "properties": { "cwd": { "type": "STRING" }, "command": { "type": "STRING" } },
                    "required": ["command"]
                }
            }
        ]
    }]);

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
    const MAX_AGENT_STEPS: usize = 80;

    let pid = std::process::id();
    let req_path = PathBuf::from(format!("/tmp/infimum_web_agent_{pid}.json"));
    let hdr_path = PathBuf::from(format!("/tmp/infimum_web_agent_{pid}.cfg"));
    let _ = fs::write(
        &hdr_path,
        format!("header = \"Content-Type: application/json\"\nheader = \"x-goog-api-key: {api_key}\"\n"),
    );
    let _ = fs::set_permissions(&hdr_path, fs::Permissions::from_mode(0o600));
    let url = format!("https://{GEMINI_HOST}/v1beta/models/{model}:streamGenerateContent?alt=sse");

    let mut include_thoughts = true;
    for step in 0..MAX_AGENT_STEPS {
        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break;
        }
        let is_last = step == MAX_AGENT_STEPS - 1;
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
            format!("Шаг 1/{MAX_AGENT_STEPS}: анализирую ({model})…")
        } else if is_last {
            format!("Шаг {MAX_AGENT_STEPS}/{MAX_AGENT_STEPS}: подвожу итог…")
        } else {
            format!("Шаг {}/{MAX_AGENT_STEPS}: думаю ({model})…", step + 1)
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

        let _ = fs::write(&req_path, build_req_body(include_thoughts).to_string());

        let mut turn_res = call_gemini_sse_with_retry(
            &url,
            &hdr_path,
            &req_path,
            model,
            step + 1,
            MAX_AGENT_STEPS,
            &mut set_status,
        );

        // Graceful fallback if the selected model rejects `thinkingConfig`
        if include_thoughts {
            if let Err(ref e) = turn_res {
                let low = e.to_lowercase();
                if low.contains("thinkingconfig") || low.contains("includethoughts") || low.contains("thinking") {
                    include_thoughts = false;
                    let _ = fs::write(&req_path, build_req_body(false).to_string());
                    turn_res = call_gemini_sse_with_retry(
                        &url,
                        &hdr_path,
                        &req_path,
                        model,
                        step + 1,
                        MAX_AGENT_STEPS,
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
        if step == MAX_AGENT_STEPS - 2 {
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
    let _ = fs::remove_file(&hdr_path);

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

const COMPACT_RESPONSE_THRESHOLD_CHARS: usize = 900;

/// Compact older large read-only `functionResponse` payloads (`read_project_file`, `search_project`,
/// `list_project_files`, `get_file_outline`) while strictly preserving Gemini 4 protocol invariants:
/// 1. Never modifies `model` turns (`thoughtSignature`, `functionCall`) or `user`/`model` alternation.
/// 2. Keeps the latest `keep_recent_tool_steps` messages containing `functionResponse` completely intact.
/// 3. Never compacts mutation/verification/command tools or any response containing an error.
fn compact_old_tool_responses(contents: &mut [Value], keep_recent_tool_steps: usize) {
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
                "read_project_file" | "search_project" | "list_project_files" | "get_file_outline"
            ) {
                continue;
            }
            let Some(resp) = fr.get_mut("response").and_then(|r| r.as_object_mut()) else {
                continue;
            };
            // Never compact error responses
            if resp.contains_key("error")
                || resp.get("ok").and_then(|v| v.as_bool()) == Some(false)
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
                _ => {}
            }
        }
    }
}

struct GeminiTurn {
    parts: Vec<Value>,
    in_tokens: u64,
    out_tokens: u64,
}

#[derive(Default)]
struct SseTurnAccumulator {
    parts: Vec<Value>,
    in_tokens: u64,
    out_tokens: u64,
    thought_chars: usize,
    thought_text: String,
    answer_chars: usize,
    fn_calls_seen: usize,
    chunks_parsed: usize,
    api_error: Option<String>,
    raw_non_sse: String,
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

    fn ingest_line(&mut self, raw_line: &str) -> bool {
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

fn extract_gemini_error_from_raw(raw: &str) -> Option<String> {
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
    let func = inst
        .get_typed_func::<i32, i32>(&store, "extract_api_error_ifm")
        .ok()?;
    let json_in_word = inst
        .get_typed_func::<(), i32>(&store, "get_json_in_word")
        .ok()
        .and_then(|f| f.call(&mut store, ()).ok())
        .map(|w| w.max(0) as usize)
        .unwrap_or(512);
    let json_out_word = inst
        .get_typed_func::<(), i32>(&store, "get_json_out_word")
        .ok()
        .and_then(|f| f.call(&mut store, ()).ok())
        .map(|w| w.max(0) as usize)
        .unwrap_or(17_000);
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

fn is_transient_api_error(msg: &str) -> bool {
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

fn call_gemini_sse_with_retry<F>(
    url: &str,
    hdr_path: &Path,
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
        match call_gemini_sse_once(url, hdr_path, req_path, model, step_num, max_steps, set_status) {
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

fn call_gemini_sse_once<F>(
    url: &str,
    hdr_path: &Path,
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
            "-K",
            &hdr_path.display().to_string(),
            "--data-binary",
            &format!("@{}", req_path.display()),
            url,
        ])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| (format!("Не удалось запустить curl для Gemini API: {e}"), false))?;

    ASYNC_GEMINI_PID.store(child.id(), Ordering::SeqCst);

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
fn normalize_contents_roles(contents: &mut Vec<Value>) {
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

#[cfg(test)]
mod tests {
    use super::*;

    fn test_policy_with_root(root: PathBuf) -> CapabilityPolicy {
        CapabilityPolicy {
            allowed_fs_roots: vec![root],
            ..CapabilityPolicy::default()
        }
    }

    #[test]
    fn get_file_outline_extracts_real_ifm_and_rs_files() {
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .unwrap()
            .to_path_buf();
        let policy = test_policy_with_root(project_root.clone());

        // 1. Real .ifm files (modularized studio capsule: primitives.ifm + infimum_code.ifm)
        let prim_path = project_root.join("capsule/src/primitives.ifm");
        if prim_path.is_file() {
            let res = execute_native_tool(
                &policy,
                &project_root,
                "get_file_outline",
                &json!({ "path": prim_path.display().to_string() }),
                None,
            );
            assert_eq!(res["ok"], true, "primitives.ifm outline failed: {res}");
            let outline = res["outline"].as_str().unwrap();
            assert!(outline.contains("fn buf_can_insert("), "missing buf_can_insert in outline");
            assert!(outline.contains("[req]"), "missing [req] summary in outline");
            assert!(outline.contains("-[Mem]-> i64"), "missing effect signature in outline");
            assert!(res["entries_count"].as_u64().unwrap() >= 3);
            for line in outline.lines() {
                assert!(line.chars().count() <= OUTLINE_MAX_LINE_CHARS + 1);
            }
        }
        let ifm_path = project_root.join("capsule/src/infimum_code.ifm");
        if ifm_path.is_file() {
            let res = execute_native_tool(
                &policy,
                &project_root,
                "get_file_outline",
                &json!({ "path": ifm_path.display().to_string() }),
                None,
            );
            assert_eq!(res["ok"], true, "infimum_code.ifm outline failed: {res}");
            let outline = res["outline"].as_str().unwrap();
            assert!(outline.contains("export fn "), "missing export fn in outline");
            assert!(res["entries_count"].as_u64().unwrap() > 10);
            for line in outline.lines() {
                assert!(line.chars().count() <= OUTLINE_MAX_LINE_CHARS + 1);
            }
        }

        // 2. Real .rs file (daemon/src/agent.rs)
        let rs_path = project_root.join("daemon/src/agent.rs");
        let res_rs = execute_native_tool(
            &policy,
            &project_root,
            "get_file_outline",
            &json!({ "path": rs_path.display().to_string() }),
            None,
        );
        assert_eq!(res_rs["ok"], true, "rs outline failed: {res_rs}");
        let rs_outline = res_rs["outline"].as_str().unwrap();
        assert!(rs_outline.contains("pub struct AttachedImage"), "missing struct");
        assert!(rs_outline.contains("fn execute_native_tool("), "missing execute_native_tool");
        assert!(rs_outline.contains("impl SseTurnAccumulator"), "missing impl");
        assert!(rs_outline.contains("fn ingest_line("), "missing method inside impl");
        assert!(rs_outline.contains("mod tests"), "missing mod tests");

        // 3. Synthetic .ifm with all supported declaration kinds
        let synthetic_ifm = r#"use std.math;
const MAX_ITEMS: i64 = 128;
enum Mode { Chat, Web }
struct Point { x: i64, y: i64 }
layout Panel { w: i64, h: i64 }
forma Button { label: str }
export fn render(x: i64) -[Mem, Gpu]-> unit
  req x >= 0
  ens result == ()
{
  ()
}
"#;
        let (entries, truncated) = extract_ifm_outline(synthetic_ifm);
        assert!(!truncated);
        assert_eq!(entries.len(), 7);
        assert!(entries[0].contains("L1..L1: use std.math"));
        assert!(entries[1].contains("L2..L2: const MAX_ITEMS: i64 = 128"));
        assert!(entries[2].contains("L3..L3: enum Mode"));
        assert!(entries[3].contains("L4..L4: struct Point"));
        assert!(entries[4].contains("L5..L5: layout Panel"));
        assert!(entries[5].contains("L6..L6: forma Button"));
        assert!(entries[6].contains("L7..L12: export fn render(x: i64) -[Mem, Gpu]-> unit [req, ens]"));
    }

    #[test]
    fn batch_edit_project_file_applies_atomically_and_rolls_back_on_error() {
        let tmp_dir = env::temp_dir().join(format!("infimum_web_batch_test_{}", now_millis()));
        fs::create_dir_all(&tmp_dir).unwrap();
        let file_path = tmp_dir.join("sample.rs");
        let original = "const A: i64 = 1;\nconst B: i64 = 2;\nconst C: i64 = 3;\n";
        fs::write(&file_path, original).unwrap();

        let policy = test_policy_with_root(tmp_dir.clone());

        // 1. Failing batch (edit #1 is valid, edit #2 is missing) -> must roll back atomically!
        let fail_res = execute_native_tool(
            &policy,
            &tmp_dir,
            "batch_edit_project_file",
            &json!({
                "path": file_path.display().to_string(),
                "edits": [
                    { "target": "const A: i64 = 1;", "replacement": "const A: i64 = 10;" },
                    { "target": "const NON_EXISTENT: i64 = 99;", "replacement": "const X: i64 = 0;" }
                ]
            }),
            None,
        );
        assert_eq!(fail_res["ok"], false);
        assert_eq!(fail_res["failed_index"], 1);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), original);

        // 2. Overlapping edits -> must roll back atomically!
        let overlap_res = execute_native_tool(
            &policy,
            &tmp_dir,
            "batch_edit_project_file",
            &json!({
                "path": file_path.display().to_string(),
                "edits": [
                    { "target": "const A: i64 = 1;\nconst B: i64 = 2;", "replacement": "X" },
                    { "target": "const B: i64 = 2;\nconst C: i64 = 3;", "replacement": "Y" }
                ]
            }),
            None,
        );
        assert_eq!(overlap_res["ok"], false);
        assert_eq!(fs::read_to_string(&file_path).unwrap(), original);

        // 3. Valid multi-edit -> applies all 3 edits in a single write!
        let ok_res = execute_native_tool(
            &policy,
            &tmp_dir,
            "batch_edit_project_file",
            &json!({
                "path": file_path.display().to_string(),
                "edits": [
                    { "target": "const C: i64 = 3;", "replacement": "const C: i64 = 30;" },
                    { "target": "const A: i64 = 1;", "replacement": "const A: i64 = 10;" },
                    { "target": "const B: i64 = 2;", "replacement": "const B: i64 = 20;" }
                ]
            }),
            None,
        );
        assert_eq!(ok_res["ok"], true, "batch edit failed: {ok_res}");
        assert_eq!(ok_res["applied_count"], 3);
        assert_eq!(ok_res["total_lines"], 3);
        assert_eq!(
            fs::read_to_string(&file_path).unwrap(),
            "const A: i64 = 10;\nconst B: i64 = 20;\nconst C: i64 = 30;\n"
        );

        let _ = fs::remove_dir_all(&tmp_dir);
    }

    #[test]
    fn compact_old_tool_responses_preserves_thought_signature_recent_4_and_errors() {
        let big = "x".repeat(1500);
        let big_files: Vec<String> = (0..80).map(|i| format!("src/module_long_file_name_{i}.rs")).collect();

        // Build 6 tool steps (each is a model functionCall + user functionResponse)
        let mut contents = vec![
            json!({ "role": "user", "parts": [{ "text": "Analyze project" }] }),
            // Step 1 (older, should compact read_project_file + list_project_files + get_file_outline)
            json!({
                "role": "model",
                "parts": [{
                    "functionCall": { "name": "read_project_file", "args": { "path": "a.rs" } },
                    "thoughtSignature": "sig-step-1"
                }]
            }),
            json!({
                "role": "user",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "read_project_file",
                            "response": { "path": "a.rs", "start_line": 10, "end_line": 90, "content": big }
                        }
                    },
                    {
                        "functionResponse": {
                            "name": "list_project_files",
                            "response": { "project_dir": "/proj", "files": big_files }
                        }
                    },
                    {
                        "functionResponse": {
                            "name": "get_file_outline",
                            "response": { "ok": true, "path": "a.rs", "outline": big }
                        }
                    }
                ]
            }),
            // Step 2 (older, verify_infimum_file error MUST NOT be compacted; search_project SHOULD compact)
            json!({
                "role": "model",
                "parts": [{
                    "functionCall": { "name": "verify_infimum_file", "args": { "path": "a.ifm" } },
                    "thoughtSignature": "sig-step-2"
                }]
            }),
            json!({
                "role": "user",
                "parts": [
                    {
                        "functionResponse": {
                            "name": "verify_infimum_file",
                            "response": { "ok": false, "output": big }
                        }
                    },
                    {
                        "functionResponse": {
                            "name": "search_project",
                            "response": { "ok": true, "matches": big }
                        }
                    }
                ]
            }),
            // Steps 3..6 (latest 4 tool steps -> MUST stay 100% full!)
            json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-3" }] }),
            json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s3.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
            json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-4" }] }),
            json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s4.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
            json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-5" }] }),
            json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s5.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
            json!({ "role": "model", "parts": [{ "functionCall": { "name": "read_project_file", "args": {} }, "thoughtSignature": "sig-step-6" }] }),
            json!({ "role": "user", "parts": [{ "functionResponse": { "name": "read_project_file", "response": { "path": "s6.rs", "start_line": 1, "end_line": 50, "content": big } } }] }),
        ];

        compact_old_tool_responses(&mut contents, 4);

        // 1. Model turns and thoughtSignatures are untouched
        assert_eq!(contents[1]["parts"][0]["thoughtSignature"], "sig-step-1");
        assert_eq!(contents[3]["parts"][0]["thoughtSignature"], "sig-step-2");
        assert_eq!(contents[11]["parts"][0]["thoughtSignature"], "sig-step-6");

        // 2. Older read-only tools (>900 chars) are compacted with metadata
        let s1_read = contents[2]["parts"][0]["functionResponse"]["response"]["content"]
            .as_str()
            .unwrap();
        assert!(s1_read.contains("compacted older tool output: read_project_file a.rs lines 10..90"));
        assert!(s1_read.contains("1500 chars"));

        let s1_files = contents[2]["parts"][1]["functionResponse"]["response"]["files"]
            .as_str()
            .unwrap();
        assert!(s1_files.contains("compacted older tool output: list_project_files /proj (80 files)"));

        let s1_outline = contents[2]["parts"][2]["functionResponse"]["response"]["outline"]
            .as_str()
            .unwrap();
        assert!(s1_outline.contains("compacted older tool output: get_file_outline a.rs"));

        let s2_search = contents[4]["parts"][1]["functionResponse"]["response"]["matches"]
            .as_str()
            .unwrap();
        assert!(s2_search.contains("compacted older tool output: search_project"));

        // 3. Older verify_infimum_file error is preserved in full
        let s2_verify = contents[4]["parts"][0]["functionResponse"]["response"]["output"]
            .as_str()
            .unwrap();
        assert_eq!(s2_verify.len(), 1500);

        // 4. Latest 4 tool steps (indices 6, 8, 10, 12) remain completely full
        for idx in [6usize, 8, 10, 12] {
            let c = contents[idx]["parts"][0]["functionResponse"]["response"]["content"]
                .as_str()
                .unwrap();
            assert_eq!(c.len(), 1500, "recent tool step at idx {idx} must not be compacted");
        }
    }

    #[test]
    fn sse_accumulator_merges_text_and_preserves_thought_signature_on_tool_call() {
        let mut acc = SseTurnAccumulator::default();
        acc.ingest_line(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"step 1 "}]}}]}"#,
        );
        acc.ingest_line(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"thought":true,"text":"step 2"}]}}]}"#,
        );
        acc.ingest_line(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"Привет, "}]}}]}"#,
        );
        acc.ingest_line(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"text":"мир!"}]}}]}"#,
        );
        acc.ingest_line(
            r#"data: {"candidates":[{"content":{"role":"model","parts":[{"functionCall":{"name":"search_project","args":{"query":"foo"}},"thoughtSignature":"sig-abc-123"}]}}],"usageMetadata":{"promptTokenCount":1200,"candidatesTokenCount":85,"thoughtsTokenCount":315}}"#,
        );

        assert_eq!(acc.chunks_parsed, 5);
        assert_eq!(acc.parts.len(), 3);
        assert_eq!(acc.parts[0]["thought"], true);
        assert_eq!(acc.parts[0]["text"], "step 1 step 2");
        assert_eq!(acc.parts[1]["text"], "Привет, мир!");
        assert_eq!(acc.parts[2]["functionCall"]["name"], "search_project");
        assert_eq!(acc.parts[2]["thoughtSignature"], "sig-abc-123");
        assert_eq!(acc.in_tokens, 1200);
        assert_eq!(acc.out_tokens, 400);
    }

    #[test]
    fn extract_gemini_error_handles_object_and_array_payloads() {
        let obj = r#"{"error":{"code":503,"message":"The model is overloaded."}}"#;
        let arr = r#"[{"error":{"code":429,"message":"Resource has been exhausted."}}]"#;
        assert_eq!(
            extract_gemini_error_from_raw(obj).as_deref(),
            Some("The model is overloaded.")
        );
        assert_eq!(
            extract_gemini_error_from_raw(arr).as_deref(),
            Some("Resource has been exhausted.")
        );
    }

    #[test]
    fn normalize_contents_roles_drops_leading_model_and_merges_adjacent() {
        let mut contents = vec![
            json!({"role": "model", "parts": [{"text": "stale"}]}),
            json!({"role": "user", "parts": [{"text": "hi"}]}),
            json!({"role": "user", "parts": [{"text": "again"}]}),
            json!({"role": "model", "parts": [{"text": "ok"}]}),
        ];
        normalize_contents_roles(&mut contents);
        assert_eq!(contents.len(), 2);
        assert_eq!(contents[0]["role"], "user");
        assert_eq!(contents[0]["parts"].as_array().unwrap().len(), 2);
        assert_eq!(contents[1]["role"], "model");
    }

    #[test]
    fn timeline_encode_roundtrip_and_bounds() {
        let snap = TimelineSnapshot {
            round: 3,
            max_rounds: 30,
            total_elapsed_tenths: 47,
            has_running_cmd: true,
            thought_snippet: "Проверяю раскладку ленты".to_string(),
            steps: vec![
                AgentStep {
                    status: STEP_STATUS_DONE,
                    kind: STEP_KIND_THINK,
                    dur_tenths: 12,
                    title: "Анализ задачи".to_string(),
                    detail: "gemini-4-pro".to_string(),
                    output_tail: String::new(),
                    started_at: None,
                },
                AgentStep {
                    status: STEP_STATUS_RUNNING,
                    kind: STEP_KIND_CMD,
                    dur_tenths: 35,
                    title: "Команда терминала".to_string(),
                    detail: "$ cargo test".to_string(),
                    output_tail: "running 19 tests\ntest ok".to_string(),
                    started_at: None,
                },
            ],
        };
        let words = encode_timeline_words(&snap, LIVE_TIMELINE_MAX_WORDS as usize);
        assert_eq!(words[0] as usize, words.len());
        assert_eq!(words[1], 3);
        assert_eq!(words[2], 30);
        assert_eq!(words[3], 47);
        assert_eq!(words[4], 1);
        assert_eq!(words[5], 2);
        let th_len = words[6] as usize;
        assert_eq!(th_len, "Проверяю раскладку ленты".chars().count());
        let p0 = 7 + th_len;
        assert_eq!(words[p0], STEP_STATUS_DONE);
        assert_eq!(words[p0 + 1], STEP_KIND_THINK);
        assert_eq!(words[p0 + 2], 12);
        let t0_len = words[p0 + 3] as usize;
        let q0 = p0 + 4 + t0_len;
        let d0_len = words[q0] as usize;
        let r0 = q0 + 1 + d0_len;
        let tail0_len = words[r0] as usize;
        assert_eq!(tail0_len, 0);
        let p1 = r0 + 1 + tail0_len;
        assert_eq!(words[p1], STEP_STATUS_RUNNING);
        assert_eq!(words[p1 + 1], STEP_KIND_CMD);
        assert_eq!(words[p1 + 2], 35);
    }

    #[test]
    fn kill_active_shell_command_interrupts_sleeping_bash() {
        let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
        let project_root = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let mut policy = test_policy_with_root(project_root.clone());
        policy.allow_proc = true;
        ASYNC_CANCELLED.store(false, Ordering::SeqCst);
        ASYNC_CMD_KILL_REQUESTED.store(false, Ordering::SeqCst);

        let killer = std::thread::spawn(|| {
            let wait_t0 = Instant::now();
            while ASYNC_CMD_PID.load(Ordering::SeqCst) == 0 && wait_t0.elapsed() < Duration::from_secs(3) {
                std::thread::sleep(Duration::from_millis(25));
            }
            std::thread::sleep(Duration::from_millis(120));
            kill_active_shell_command()
        });

        let t0 = Instant::now();
        let res = execute_native_tool(
            &policy,
            &project_root,
            "run_project_command",
            &json!({ "command": "echo started; sleep 15; echo finished" }),
            None,
        );
        let elapsed = t0.elapsed();
        let did_kill = killer.join().unwrap();

        assert!(did_kill, "expected kill_active_shell_command to find running pid");
        assert_eq!(res["killed_by_user"], true, "expected killed_by_user=true, got: {res}");
        assert!(elapsed < Duration::from_secs(4), "command took too long to die: {elapsed:?}");
        assert!(res["output"].as_str().unwrap_or("").contains("started"));
    }

    #[test]
    fn timeline_json_save_and_load_roundtrip() {
        let tmp_path = std::env::temp_dir().join(format!(
            "infimum_web_tl_roundtrip_{}.json",
            std::process::id()
        ));
        let payload = json!({
            "project": "infimum-web",
            "title": "Тест таймлайна",
            "messages": [
                { "role": "user", "content": "Проверь сборку" },
                {
                    "role": "assistant",
                    "content": "Сборка прошла успешно.",
                    "timeline": {
                        "round": 2,
                        "max_rounds": 30,
                        "total_elapsed_tenths": 18,
                        "has_running_cmd": false,
                        "thought_snippet": "Запускаю cargo check",
                        "steps": [
                            {
                                "status": STEP_STATUS_DONE,
                                "kind": STEP_KIND_CMD,
                                "dur_tenths": 14,
                                "title": "Команда терминала",
                                "detail": "$ cargo check",
                                "output_tail": "Finished dev profile"
                            }
                        ]
                    }
                }
            ]
        });
        fs::write(&tmp_path, payload.to_string()).expect("write temp conv json");
        let loaded = load_conversation_from_path(&tmp_path).expect("load conv json");
        let _ = fs::remove_file(&tmp_path);
        assert_eq!(loaded.len(), 2);
        assert_eq!(loaded[0].0, 0);
        assert!(loaded[0].2.is_none());
        assert_eq!(loaded[1].0, 1);
        let tl = loaded[1].2.as_ref().expect("expected assistant timeline");
        assert_eq!(tl.round, 2);
        assert_eq!(tl.total_elapsed_tenths, 18);
        assert_eq!(tl.thought_snippet, "Запускаю cargo check");
        assert_eq!(tl.steps.len(), 1);
        assert_eq!(tl.steps[0].title, "Команда терминала");
        assert_eq!(tl.steps[0].output_tail, "Finished dev profile");
    }

    #[test]
    fn single_folder_jail_blocks_sibling_project_and_sensitive_files() {
        let base = std::env::temp_dir().join(format!("infimum_jail_test_{}", std::process::id()));
        let proj_a = base.join("proj_a");
        let proj_b = base.join("proj_b");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        fs::write(proj_a.join("ok.txt"), "hello_safe").unwrap();
        fs::write(proj_a.join(".env"), "SECRET=123").unwrap();
        fs::write(proj_b.join("sibling.txt"), "TOP_SECRET").unwrap();

        let mut pol = CapabilityPolicy::default();
        pol.allowed_fs_roots.push(base.clone());
        pol.allow_proc = true;

        // Reading inside active project succeeds
        let ok_res = execute_native_tool(
            &pol,
            &proj_a,
            "read_project_file",
            &json!({ "path": "ok.txt" }),
            None,
        );
        assert_eq!(ok_res.get("content").and_then(|v| v.as_str()), Some("hello_safe"));

        // Reading sibling project via absolute path is DENIED
        let sib_res = execute_native_tool(
            &pol,
            &proj_a,
            "read_project_file",
            &json!({ "path": proj_b.join("sibling.txt").display().to_string() }),
            None,
        );
        assert!(sib_res.get("error").is_some(), "sibling read must be denied: {sib_res}");

        // Reading .env even inside active project is DENIED
        let env_res = execute_native_tool(
            &pol,
            &proj_a,
            "read_project_file",
            &json!({ "path": ".env" }),
            None,
        );
        assert!(env_res.get("error").is_some(), ".env read must be denied: {env_res}");

        // list_project_files hides .env
        let list_res = execute_native_tool(&pol, &proj_a, "list_project_files", &json!({}), None);
        let files = list_res.get("files").and_then(|v| v.as_array()).unwrap();
        assert!(files.iter().any(|f| f.as_str() == Some("ok.txt")));
        assert!(!files.iter().any(|f| f.as_str() == Some(".env")));

        // Writing inside a brand-new nested subdirectory succeeds
        let nested_ok = execute_native_tool(
            &pol,
            &proj_a,
            "write_project_file",
            &json!({ "path": "src/sub/main.ifm", "content": "fn main() -> i64 { 0 }" }),
            None,
        );
        assert_eq!(nested_ok.get("ok").and_then(|v| v.as_bool()), Some(true), "nested write must succeed: {nested_ok}");
        assert!(proj_a.join("src/sub/main.ifm").is_file());

        // Traversal via non-existent parent + `..` is DENIED
        let trav_bad = execute_native_tool(
            &pol,
            &proj_a,
            "write_project_file",
            &json!({ "path": "src/../../proj_b/escape.ifm", "content": "bad" }),
            None,
        );
        assert!(trav_bad.get("error").is_some(), "traversal write must be denied: {trav_bad}");
        assert!(!proj_b.join("escape.ifm").exists());

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bwrap_sandbox_blocks_sibling_and_clears_env() {
        let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("infimum_bwrap_test_{}", std::process::id()));
        let proj_a = base.join("proj_a");
        let proj_b = base.join("proj_b");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&proj_a).unwrap();
        fs::create_dir_all(&proj_b).unwrap();
        fs::write(proj_a.join("inside.txt"), "INSIDE_OK").unwrap();
        fs::write(proj_b.join("outside.txt"), "OUTSIDE_LEAK").unwrap();

        let mut pol = CapabilityPolicy::default();
        pol.allowed_fs_roots.push(base.clone());
        pol.allow_proc = true;

        let cmd = format!(
            "cat inside.txt; cat {} 2>&1 || true; env",
            proj_b.join("outside.txt").display()
        );
        let res = execute_native_tool(
            &pol,
            &proj_a,
            "run_project_command",
            &json!({ "command": cmd }),
            None,
        );
        let out = res.get("output").and_then(|v| v.as_str()).unwrap_or("");
        assert!(out.contains("INSIDE_OK"), "sandbox must allow active folder: {out}");
        assert!(!out.contains("OUTSIDE_LEAK"), "sandbox leaked sibling file! {out}");
        assert!(!out.contains("GEMINI_API_KEY="), "sandbox leaked GEMINI_API_KEY! {out}");
        assert!(!out.contains("OPENROUTER_API_KEY="), "sandbox leaked OPENROUTER_API_KEY! {out}");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn bwrap_sandbox_exposes_infimum_cli_without_source_leak() {
        let _guard = test_cmd_lock().lock().unwrap_or_else(|e| e.into_inner());
        let base = std::env::temp_dir().join(format!("infimum_cli_test_{}", std::process::id()));
        let proj = base.join("proj");
        let _ = fs::remove_dir_all(&base);
        fs::create_dir_all(&proj).unwrap();

        let mut pol = CapabilityPolicy::default();
        pol.allowed_fs_roots.push(base.clone());
        pol.allow_proc = true;

        // 1. Inside bwrap, `which infimum` must resolve to /tmp/infimum-bin/infimum,
        // while the host infimum-lang source tree remains unreachable.
        let host_lang_dir = resolve_infimum_std_dir()
            .and_then(|d| d.parent().map(|p| p.display().to_string()))
            .unwrap_or_else(|| "/nonexistent/infimum-lang".to_string());
        let res = execute_native_tool(
            &pol,
            &proj,
            "run_project_command",
            &json!({ "command": format!("which infimum; ls '{host_lang_dir}' 2>&1 || true") }),
            None,
        );
        let out = res.get("output").and_then(|v| v.as_str()).unwrap_or("");
        if resolve_infimum_bin(&pol).is_file() {
            assert!(
                out.contains("/tmp/infimum-bin/infimum"),
                "Expected read-only infimum binary in sandbox PATH, got: {out}"
            );
        }
        assert!(
            !out.contains("Cargo.toml"),
            "Sandbox must not expose infimum-lang source tree: {out}"
        );

        // 2. compile_infimum_wasm and run_infimum_capsule must block paths outside active project
        let bad_compile = execute_native_tool(
            &pol,
            &proj,
            "compile_infimum_wasm",
            &json!({ "path": "../outside.ifm" }),
            None,
        );
        assert!(bad_compile.get("error").is_some(), "compile_infimum_wasm must enforce jail");

        let bad_run = execute_native_tool(
            &pol,
            &proj,
            "run_infimum_capsule",
            &json!({ "path": "../outside.ifw", "headless": true }),
            None,
        );
        assert!(bad_run.get("error").is_some(), "run_infimum_capsule must enforce jail");

        // 3. read_infimum_guide returns canonical docs with compiler and launcher workflow
        let guide_res = execute_native_tool(&pol, &proj, "read_infimum_guide", &json!({}), None);
        let guide_txt = guide_res.get("content").and_then(|v| v.as_str()).unwrap_or("");
        assert!(
            guide_txt.contains("infimum compile-wasm") && guide_txt.contains("run_infimum_capsule"),
            "Expected canonical Infimum guide with toolchain docs, got len={}",
            guide_txt.len()
        );

        // 4. read_infimum_std reads std/*.ifm safely and blocks path traversal
        if resolve_infimum_std_dir().is_some() {
            let std_res = execute_native_tool(&pol, &proj, "read_infimum_std", &json!({ "file": "input.ifm" }), None);
            let std_txt = std_res.get("content").and_then(|v| v.as_str()).unwrap_or("");
            assert!(std_txt.contains("STD_KEYS_HELD_WORD"), "Expected std/input.ifm content: {std_res}");
        }
        let std_bad = execute_native_tool(&pol, &proj, "read_infimum_std", &json!({ "file": "../Cargo.toml" }), None);
        assert!(std_bad.get("error").is_some(), "Path traversal in read_infimum_std must fail: {std_bad}");

        let _ = fs::remove_dir_all(&base);
    }

    #[test]
    fn semantic_ifm_tools_and_templates_work_and_verify() {
        let base = std::env::temp_dir().join(format!("ifm_sem_tools_{}", now_millis()));
        let proj = base.join("proj");
        fs::create_dir_all(&proj).unwrap();
        let pol = CapabilityPolicy {
            allowed_net_domains: std::collections::BTreeSet::new(),
            allowed_fs_roots: vec![proj.clone()],
            allow_proc: false,
            allow_user_workspaces: false,
        };

        let sample = r#"struct Vec2 {
  x: word,
  y: word,
}

fn helper(a: i64) -> i64 {
  a + 1
}

fn main_fn(v: i64) -> i64 {
  helper(v) + helper(2)
}
"#;
        fs::write(proj.join("demo.ifm"), sample).unwrap();

        let got = execute_native_tool(
            &pol,
            &proj,
            "get_ifm_symbol",
            &json!({ "path": "demo.ifm", "name": "helper" }),
            None,
        );
        assert_eq!(got.get("ok").and_then(|v| v.as_bool()), Some(true));
        assert!(got.get("source").and_then(|v| v.as_str()).unwrap().contains("a + 1"));

        let rep = execute_native_tool(
            &pol,
            &proj,
            "replace_ifm_symbol",
            &json!({ "path": "demo.ifm", "name": "helper", "new_source": "fn helper(a: i64) -> i64 {\n  a + 42\n}" }),
            None,
        );
        assert_eq!(rep.get("ok").and_then(|v| v.as_bool()), Some(true));
        let updated = fs::read_to_string(proj.join("demo.ifm")).unwrap();
        assert!(updated.contains("a + 42") && updated.contains("main_fn"));

        let callers = execute_native_tool(
            &pol,
            &proj,
            "find_ifm_callers",
            &json!({ "symbol": "helper" }),
            None,
        );
        assert_eq!(callers.get("count").and_then(|v| v.as_u64()), Some(1));

        // Verify all 3 starter templates with Z3 when local infimum CLI is present
        let bin = resolve_infimum_bin(&pol);
        for kind in ["ui", "game2d", "game3d"] {
            let tpl = execute_native_tool(
                &pol,
                &proj,
                "read_infimum_template",
                &json!({ "kind": kind }),
                None,
            );
            let src = tpl.get("source").and_then(|v| v.as_str()).unwrap_or("");
            assert!(src.contains("step_ui"), "template {kind} missing step_ui");
            if bin.is_file() {
                let p = proj.join(format!("tpl_{kind}.ifm"));
                fs::write(&p, src).unwrap();
                let out = Command::new(&bin)
                    .args(["verify", &p.display().to_string()])
                    .output()
                    .unwrap();
                assert!(
                    out.status.success(),
                    "Template '{kind}' failed Z3 verify:\n{}{}",
                    String::from_utf8_lossy(&out.stdout),
                    String::from_utf8_lossy(&out.stderr)
                );
            }
        }

        // Verify read_infimum_example lists, reads struct-based sources, and blocks path traversal
        let ex_list = execute_native_tool(&pol, &proj, "read_infimum_example", &json!({}), None);
        assert!(ex_list.get("available_examples").is_some(), "ex_list={ex_list}");
        let ex_read = execute_native_tool(
            &pol,
            &proj,
            "read_infimum_example",
            &json!({ "example": "slipgate", "file": "abi.ifm" }),
            None,
        );
        if let Some(content) = ex_read.get("content").and_then(|v| v.as_str()) {
            assert!(content.contains("struct Enemy"), "slipgate abi.ifm should use typed structs");
        }
        let ex_bad = execute_native_tool(
            &pol,
            &proj,
            "read_infimum_example",
            &json!({ "example": "slipgate", "file": "../Cargo.toml" }),
            None,
        );
        assert!(ex_bad.get("error").is_some(), "Path traversal in read_infimum_example must fail: {ex_bad}");

        let _ = fs::remove_dir_all(&base);
    }
}
