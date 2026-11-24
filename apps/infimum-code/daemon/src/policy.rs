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
}

impl CapabilityPolicy {
    pub fn fs_enabled(&self) -> bool {
        !self.allowed_fs_roots.is_empty() || self.allow_user_workspaces
    }
}
