//! Live agent execution timeline, cancellation atomics, and POSIX process signaling.

use crate::studio_abi::*;
use serde::{Deserialize, Serialize};
#[cfg(not(unix))]
use std::process::Command;
use std::sync::atomic::{AtomicBool, AtomicU32, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

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

pub(super) struct LiveTimelineState {
    started_at: Option<Instant>,
    snap: TimelineSnapshot,
}

pub(super) fn live_timeline_cell() -> &'static Mutex<LiveTimelineState> {
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

pub(super) fn timeline_set_round(round: i64) {
    if let Ok(mut g) = live_timeline_cell().lock() {
        g.snap.round = round;
    }
}

pub(super) fn timeline_set_thought(raw_thought: &str) {
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

pub(super) fn timeline_push_thought(raw_thought: &str) {
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

pub(super) fn timeline_push_step(kind: i64, title: String, detail: String) -> usize {
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

pub(super) fn timeline_update_step_tail(step_idx: usize, tail: String) {
    if let Ok(mut g) = live_timeline_cell().lock() {
        if let Some(st) = g.snap.steps.get_mut(step_idx) {
            st.output_tail = tail;
        }
    }
}

pub(super) fn timeline_finish_step(step_idx: usize, status: i64, tail_opt: Option<String>) {
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

pub(super) fn timeline_finalize(total_elapsed_tenths: i64) -> TimelineSnapshot {
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

