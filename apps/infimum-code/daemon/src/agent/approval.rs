//! Interactive external-mutation approval gate for Developer Mode.
//!
//! When `CapabilityPolicy::confirm_external_mutations()` is enabled, mutating
//! remote commands (`git push`, `wrangler deploy`, mutating `gh` CLI actions)
//! pause in the background agent thread and surface a structured prompt to the
//! Studio UI via `GET /agent/poll` + `GET /agent/approval` + `POST /agent/approval`.
//! Read-only commands (`git status/diff/log/fetch/pull`, `gh pr list/view`,
//! `wrangler whoami/deployments list`) never block.

use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use crate::agent::ASYNC_CANCELLED;
use crate::policy::CapabilityPolicy;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExternalMutationInfo {
    pub title: String,
    pub command: String,
    pub workdir: String,
    pub risk: String,
}

#[derive(Clone, Debug)]
pub struct PendingApprovalSnapshot {
    pub id: u64,
    pub title: String,
    pub command: String,
    pub workdir: String,
    pub risk: String,
}

struct PendingApprovalState {
    id: u64,
    info: ExternalMutationInfo,
    /// `None` while waiting for user decision, `Some(true)` = allow, `Some(false)` = deny.
    decision: Option<bool>,
}

struct ApprovalGate {
    state: Mutex<Option<PendingApprovalState>>,
    cv: Condvar,
    next_id: AtomicU64,
}

impl ApprovalGate {
    fn new() -> Self {
        Self {
            state: Mutex::new(None),
            cv: Condvar::new(),
            next_id: AtomicU64::new(1),
        }
    }
}

fn gate() -> &'static ApprovalGate {
    static GATE: OnceLock<ApprovalGate> = OnceLock::new();
    GATE.get_or_init(ApprovalGate::new)
}

/// Returns the currently pending approval ID (0 if none is waiting).
pub fn pending_approval_id() -> u64 {
    let guard = gate().state.lock().unwrap_or_else(|e| e.into_inner());
    match guard.as_ref() {
        Some(p) if p.decision.is_none() => p.id,
        _ => 0,
    }
}

/// Returns the full snapshot of the currently pending approval, if any.
pub fn pending_approval_snapshot() -> Option<PendingApprovalSnapshot> {
    let guard = gate().state.lock().unwrap_or_else(|e| e.into_inner());
    guard
        .as_ref()
        .filter(|p| p.decision.is_none())
        .map(|p| PendingApprovalSnapshot {
            id: p.id,
            title: p.info.title.clone(),
            command: p.info.command.clone(),
            workdir: p.info.workdir.clone(),
            risk: p.info.risk.clone(),
        })
}

/// Resolves a pending approval request from the UI (`POST /agent/approval`).
/// Returns `true` if a matching pending request was updated.
pub fn resolve_pending_approval(id: u64, allow: bool) -> bool {
    let g = gate();
    let mut guard = g.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = guard.as_mut() {
        if p.id == id && p.decision.is_none() {
            p.decision = Some(allow);
            g.cv.notify_all();
            return true;
        }
    }
    false
}

/// Cancels any waiting approval prompt (called on agent cancel / shutdown).
pub fn cancel_pending_approval() {
    let g = gate();
    let mut guard = g.state.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(p) = guard.as_mut() {
        if p.decision.is_none() {
            p.decision = Some(false);
            g.cv.notify_all();
        }
    }
}

/// Classifies whether a `git_tool`, `wrangler_cli`, or `gh_cli` invocation
/// performs a remote/external mutation that should be confirmed by the user.
pub fn classify_external_mutation(
    tool_name: &str,
    subcommand: &str,
    args: &[String],
    workdir: &str,
) -> Option<ExternalMutationInfo> {
    let cmd_preview = {
        let mut parts = Vec::with_capacity(args.len() + 2);
        match tool_name {
            "git_tool" => {
                parts.push("git".to_string());
                parts.push(subcommand.to_string());
            }
            "wrangler_cli" => parts.push("wrangler".to_string()),
            "gh_cli" => parts.push("gh".to_string()),
            other => parts.push(other.to_string()),
        }
        for a in args {
            parts.push(a.clone());
        }
        let joined = parts.join(" ");
        if joined.chars().count() > 220 {
            let cut: String = joined.chars().take(217).collect();
            format!("{}...", cut)
        } else {
            joined
        }
    };

    match tool_name {
        "git_tool" => match subcommand {
            "push" => Some(ExternalMutationInfo {
                title: "Подтвердить git push".to_string(),
                command: cmd_preview,
                workdir: workdir.to_string(),
                risk: "Отправит локальные коммиты в удалённый Git-репозиторий.".to_string(),
            }),
            _ => None,
        },
        "wrangler_cli" => {
            let lowered: Vec<String> = args.iter().map(|s| s.to_ascii_lowercase()).collect();
            let first = lowered.first().map(|s| s.as_str()).unwrap_or("");
            let second = lowered.get(1).map(|s| s.as_str()).unwrap_or("");
            let third = lowered.get(2).map(|s| s.as_str()).unwrap_or("");
            let is_mutating = matches!(first, "deploy" | "publish" | "delete" | "rollback")
                || (first == "pages" && matches!(second, "deploy" | "publish" | "project"))
                || (first == "secret" && matches!(second, "put" | "delete" | "bulk"))
                || (first == "d1"
                    && matches!(second, "execute" | "create" | "delete" | "migrations")
                    && lowered.iter().any(|a| a == "--remote"))
                || (matches!(first, "kv" | "kv:key" | "kv:namespace" | "r2")
                    && (matches!(second, "put" | "delete" | "create" | "bulk")
                        || matches!(third, "put" | "delete" | "create" | "bulk")));
            if is_mutating {
                Some(ExternalMutationInfo {
                    title: "Подтвердить операцию Cloudflare Wrangler".to_string(),
                    command: cmd_preview,
                    workdir: workdir.to_string(),
                    risk: "Выполнит деплой или изменение ресурсов в Cloudflare.".to_string(),
                })
            } else {
                None
            }
        }
        "gh_cli" => {
            let lowered: Vec<String> = args.iter().map(|s| s.to_ascii_lowercase()).collect();
            let first = lowered.first().map(|s| s.as_str()).unwrap_or("");
            let second = lowered.get(1).map(|s| s.as_str()).unwrap_or("");
            let is_mutating = match first {
                "pr" => matches!(
                    second,
                    "create" | "merge" | "close" | "reopen" | "comment" | "edit" | "review" | "ready"
                ),
                "issue" => matches!(
                    second,
                    "create" | "close" | "reopen" | "comment" | "edit" | "delete" | "transfer"
                ),
                "release" => matches!(second, "create" | "delete" | "edit" | "upload"),
                "repo" => matches!(second, "create" | "delete" | "edit" | "archive" | "rename"),
                "workflow" => matches!(second, "run" | "enable" | "disable"),
                "run" => matches!(second, "rerun" | "cancel" | "delete"),
                "secret" | "variable" => matches!(second, "set" | "delete" | "remove"),
                "api" => {
                    // `gh api` mutates if HTTP method is not GET or body fields are supplied.
                    let mut mutating = false;
                    for (idx, tok) in lowered.iter().enumerate() {
                        if matches!(tok.as_str(), "-f" | "-F" | "--field" | "--raw-field" | "--input")
                        {
                            mutating = true;
                        }
                        if matches!(tok.as_str(), "-x" | "--method") {
                            if let Some(m) = lowered.get(idx + 1) {
                                if m != "get" && m != "head" {
                                    mutating = true;
                                }
                            }
                        }
                    }
                    mutating
                }
                _ => false,
            };
            if is_mutating {
                Some(ExternalMutationInfo {
                    title: "Подтвердить действие GitHub CLI".to_string(),
                    command: cmd_preview,
                    workdir: workdir.to_string(),
                    risk: "Изменит состояние репозитория, PR, релиза или workflow на GitHub."
                        .to_string(),
                })
            } else {
                None
            }
        }
        _ => None,
    }
}

/// Requests interactive approval from the user if policy requires it.
/// Blocks until the UI responds, the agent run is cancelled, or timeout elapses.
pub fn request_external_approval(
    info: ExternalMutationInfo,
    policy: &CapabilityPolicy,
) -> Result<(), String> {
    if !policy.confirm_external_mutations() {
        return Ok(());
    }
    request_external_approval_with_timeout(info, Duration::from_secs(180))
}

pub fn request_external_approval_with_timeout(
    info: ExternalMutationInfo,
    timeout: Duration,
) -> Result<(), String> {
    let g = gate();
    let id = g.next_id.fetch_add(1, Ordering::SeqCst);
    {
        let mut guard = g.state.lock().unwrap_or_else(|e| e.into_inner());
        *guard = Some(PendingApprovalState {
            id,
            info: info.clone(),
            decision: None,
        });
    }

    let deadline = Instant::now() + timeout;
    let mut guard = g.state.lock().unwrap_or_else(|e| e.into_inner());
    let decision = loop {
        if let Some(p) = guard.as_ref() {
            if p.id == id {
                if let Some(allow) = p.decision {
                    break Some(allow);
                }
            } else {
                break Some(false);
            }
        } else {
            break Some(false);
        }

        if ASYNC_CANCELLED.load(Ordering::SeqCst) {
            break Some(false);
        }

        let now = Instant::now();
        if now >= deadline {
            break None;
        }
        let step = (deadline - now).min(Duration::from_millis(120));
        let (new_guard, _) = g
            .cv
            .wait_timeout(guard, step)
            .unwrap_or_else(|e| e.into_inner());
        guard = new_guard;
    };

    if guard.as_ref().map(|p| p.id) == Some(id) {
        *guard = None;
    }

    match decision {
        Some(true) => Ok(()),
        Some(false) => Err(format!(
            "External mutation denied by user: `{}`",
            info.command
        )),
        None => Err(format!(
            "External mutation approval timed out after {}s: `{}`",
            timeout.as_secs(),
            info.command
        )),
    }
}
