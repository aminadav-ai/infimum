// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

#![deny(clippy::todo, clippy::unimplemented, dead_code, unused_must_use)]

pub mod engine;
pub mod net;
pub mod policy;
pub mod vault;

pub use engine::{CapsuleEngine, CompiledCapsule, ExecuteResult};
pub use policy::ExecutionPolicy;
pub use vault::{EnvSecretProvider, MapSecretProvider, SecretProvider};

use std::fmt;

/// Error returned by `infimum-rt` when loading, validating, or executing a `.ifw` capsule.
#[derive(Debug, Clone)]
pub enum RtError {
    /// Cryptographic passport / blake3 / Ed25519 signature validation failed.
    PassportInvalid(String),
    /// Capsule violated `ExecutionPolicy` (missing signature, memory limit, disallowed capability, etc.).
    PolicyViolation(String),
    /// Wasm module failed validation or instantiation.
    WasmError(String),
    /// Capsule does not implement the Headless B2B Capsule ABI (`std/capsule.ifm`).
    InvalidCapsuleAbi(String),
    /// Execution exhausted the deterministic fuel limit (anti-DoS / infinite loop guard).
    OutOfFuel { fuel_limit: u64 },
    /// Input JSON or secret exceeds capsule buffer capacity.
    InputTooLarge { actual: usize, capacity: usize },
    /// Network / SSRF guard rejected an outbound request.
    NetworkBlocked(String),
}

impl fmt::Display for RtError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            RtError::PassportInvalid(m) => write!(f, "passport invalid: {m}"),
            RtError::PolicyViolation(m) => write!(f, "policy violation: {m}"),
            RtError::WasmError(m) => write!(f, "wasm error: {m}"),
            RtError::InvalidCapsuleAbi(m) => write!(f, "invalid capsule ABI: {m}"),
            RtError::OutOfFuel { fuel_limit } => {
                write!(f, "capsule exhausted fuel limit ({fuel_limit})")
            }
            RtError::InputTooLarge { actual, capacity } => {
                write!(f, "input size {actual} exceeds capsule buffer capacity {capacity}")
            }
            RtError::NetworkBlocked(m) => write!(f, "network blocked: {m}"),
        }
    }
}

impl std::error::Error for RtError {}
