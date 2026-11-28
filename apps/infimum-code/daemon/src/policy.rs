//! Daemon capability policy for `infimum-agentd`.
//!
//! Mirrors the filesystem / process / network policy fields used by `agent.rs`,
//! strictly scoped to the developer daemon (completely decoupled from `infimum-web`).

use std::collections::BTreeSet;
use std::path::PathBuf;

#[derive(Debug, Clone, Default)]
pub struct CapabilityPolicy {
    pub allowed_fs_roots: Vec<PathBuf>,
    pub allow_user_workspaces: bool,
    pub allow_proc: bool,
    pub allowed_net_domains: BTreeSet<String>,
    /// Developer Mode: unlocks `gh_cli`, `wrangler_cli`, network `git` ops,
    /// `web_fetch`, and `web_search` (Gemini Search grounding). Disabled by
    /// default for regular users.
    pub dev_mode: bool,
    /// Turbo Mode: enables Developer Mode, disables all mutation confirmation
    /// prompts, and doubles the per-turn agent step limit.
    pub turbo_mode: bool,
    /// Optional explicit override for external-mutation confirmation prompts
    /// (used in tests or headless automation). `None` delegates to persisted
    /// `NativeSettings::confirm_external_mutations` (default `true`).
    pub confirm_external_mutations_override: Option<bool>,
}

impl CapabilityPolicy {
    pub fn fs_enabled(&self) -> bool {
        !self.allowed_fs_roots.is_empty() || self.allow_user_workspaces
    }

    pub fn is_turbo_mode(&self) -> bool {
        #[cfg(test)]
        {
            self.turbo_mode
        }
        #[cfg(not(test))]
        {
            self.turbo_mode
                || std::env::var("INFIMUM_TURBO_MODE")
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
                .unwrap_or(false)
                || crate::agent::load_native_settings()
                    .map(|s| s.turbo_mode)
                    .unwrap_or(false)
        }
    }

    pub fn is_dev_mode(&self) -> bool {
        #[cfg(test)]
        {
            self.dev_mode || self.is_turbo_mode()
        }
        #[cfg(not(test))]
        {
            self.dev_mode
                || self.is_turbo_mode()
                || std::env::var("INFIMUM_DEV_MODE")
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
                .unwrap_or(false)
                || crate::agent::load_native_settings()
                    .map(|s| s.dev_mode)
                    .unwrap_or(false)
        }
    }

    /// Whether external mutations (`git push`, `wrangler deploy`, mutating `gh`
    /// commands) require interactive Approve/Deny confirmation in the Studio UI.
    pub fn confirm_external_mutations(&self) -> bool {
        if let Some(v) = self.confirm_external_mutations_override {
            return v;
        }
        if self.is_turbo_mode() {
            return false;
        }
        #[cfg(test)]
        {
            false
        }
        #[cfg(not(test))]
        {
            if let Ok(v) = std::env::var("INFIMUM_CONFIRM_MUTATIONS") {
                return matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on");
            }
            crate::agent::load_native_settings()
                .map(|s| s.confirm_external_mutations)
                .unwrap_or(true)
        }
    }

    /// Maximum tool-calling rounds per user message (doubled in Turbo Mode).
    pub fn max_agent_steps(&self) -> usize {
        if self.is_turbo_mode() { 160 } else { 80 }
    }
}
