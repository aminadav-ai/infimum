// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

/// B2B governance policy enforced by `CapsuleEngine` before and during `.ifw` execution.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecutionPolicy {
    /// Trusted Ed25519 public keys (32 bytes each). If non-empty, a valid signature from one of
    /// these keys is mandatory.
    pub trusted_pubkeys: Vec<[u8; 32]>,
    /// Fail-closed in production: reject unsigned capsules even if `trusted_pubkeys` is empty.
    pub require_signature: bool,
    /// Require the passport to attest Z3 formal verification (`z3_obligations_proved > 0` or
    /// verified status).
    pub require_z3_proved: bool,
    /// Allowed outbound HTTPS domains (intersected with `passport.capabilities` `Net("...")`).
    pub allowed_net_domains: BTreeSet<String>,
    /// Hard linear memory ceiling in bytes (default: 2 MiB).
    pub max_memory_bytes: usize,
    /// Deterministic Wasm instruction fuel limit per `execute_json` call (default: 5,000,000).
    pub fuel_limit: u64,
    /// Outbound HTTP request timeout in milliseconds (default: 5,000 ms).
    pub http_timeout_ms: u64,
    /// Maximum number of outbound HTTP requests allowed per `execute_json` call.
    pub max_http_requests: u32,
    /// Maximum HTTP response payload size in bytes (default: 256 KiB).
    pub max_http_response_bytes: usize,
}

impl Default for ExecutionPolicy {
    fn default() -> Self {
        Self {
            trusted_pubkeys: Vec::new(),
            require_signature: false,
            require_z3_proved: false,
            allowed_net_domains: BTreeSet::new(),
            max_memory_bytes: 2 * 1024 * 1024,
            fuel_limit: 5_000_000,
            http_timeout_ms: 5_000,
            max_http_requests: 4,
            max_http_response_bytes: 256 * 1024,
        }
    }
}

impl ExecutionPolicy {
    /// Strict production policy: requires Ed25519 signature from `trusted_pubkeys`,
    /// 2 MiB memory cap, 5M fuel cap, and zero network domains by default.
    pub fn strict(trusted_pubkeys: Vec<[u8; 32]>) -> Self {
        Self {
            trusted_pubkeys,
            require_signature: true,
            require_z3_proved: true,
            ..Self::default()
        }
    }
}
