//! Shared conversation history, native settings, pending image attachments,
//! and Antigravity Brain session importer.

use serde_json::{json, Value};
use std::env;
use std::fs;
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};
use std::sync::{Mutex, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use super::timeline::TimelineSnapshot;

#[derive(Clone, Debug)]
pub struct AttachedImage {
    pub mime_type: String,
    pub data: String, // base64
    #[allow(dead_code)]
    pub width: u32,
    #[allow(dead_code)]
    pub height: u32,
}

pub(super) const MAX_ATTACHED_IMAGES: usize = 6;

pub(super) fn pending_images_cell() -> &'static Mutex<Vec<AttachedImage>> {
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

pub type GpuImageSlot = (u32, u32, Vec<u8>);
pub type LoadedConvEntry = (i32, String, Option<TimelineSnapshot>, Vec<AttachedImage>);

#[allow(dead_code)]
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

#[allow(dead_code)]
pub(super) fn loaded_conv_gpu_images_cell() -> &'static Mutex<Option<Vec<GpuImageSlot>>> {
    static CELL: OnceLock<Mutex<Option<Vec<GpuImageSlot>>>> = OnceLock::new();
    CELL.get_or_init(|| Mutex::new(None))
}

#[allow(dead_code)]
pub fn set_loaded_conv_gpu_images(imgs: Vec<GpuImageSlot>) {
    if let Ok(mut g) = loaded_conv_gpu_images_cell().lock() {
        *g = Some(imgs);
    }
}

#[allow(dead_code)]
pub fn take_loaded_conv_gpu_images() -> Option<Vec<GpuImageSlot>> {
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

pub(super) fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

pub(super) fn now_millis() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis())
        .unwrap_or(0)
}

pub(super) fn conversations_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    let dir = PathBuf::from(home).join(".config/infimum-code/conversations");
    let _ = fs::create_dir_all(&dir);
    Some(dir)
}

pub(super) fn native_settings_path() -> Option<PathBuf> {
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
    pub dev_mode: bool,
}

impl Default for NativeSettings {
    fn default() -> Self {
        Self {
            theme: 0,
            font_scale_pct: 120,
            currency_id: 0,
            model_idx: 0,
            dev_mode: false,
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
        dev_mode: v
            .get("dev_mode")
            .and_then(|x| x.as_bool().or_else(|| x.as_i64().map(|n| n != 0)))
            .unwrap_or(d.dev_mode),
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
        "dev_mode": s.dev_mode,
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

#[allow(dead_code)]
pub(super) fn brain_dir() -> Option<PathBuf> {
    let home = env::var("HOME").ok()?;
    Some(PathBuf::from(home).join(".gemini/antigravity/brain"))
}

/// Strip Antigravity XML wrappers (`<USER_REQUEST>`, `<ADDITIONAL_METADATA>`, `<CONTEXT_SUMMARY>`).
#[allow(dead_code)]
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

pub fn load_conversation_from_path(path: &Path) -> Option<Vec<LoadedConvEntry>> {
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

#[allow(dead_code)]
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

#[allow(dead_code)]
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

