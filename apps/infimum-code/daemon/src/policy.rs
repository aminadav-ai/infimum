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
}

impl CapabilityPolicy {
    pub fn fs_enabled(&self) -> bool {
        !self.allowed_fs_roots.is_empty() || self.allow_user_workspaces
    }

    pub fn is_dev_mode(&self) -> bool {
        if self.dev_mode {
            return true;
        }
        #[cfg(test)]
        {
            false
        }
        #[cfg(not(test))]
        {
            std::env::var("INFIMUM_DEV_MODE")
                .map(|v| matches!(v.trim(), "1" | "true" | "TRUE" | "yes" | "on"))
                .unwrap_or(false)
                || crate::agent::load_native_settings()
                    .map(|s| s.dev_mode)
                    .unwrap_or(false)
        }
    }
}
