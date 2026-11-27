// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use std::collections::HashMap;
use std::sync::atomic::{compiler_fence, Ordering};

/// Zero-Trust secret provider trait (implemented by ENV, in-memory map, AWS Secrets Manager,
/// HashiCorp Vault, etc.).
pub trait SecretProvider: Send + Sync {
    fn resolve(&self, secret_name: &str) -> Option<String>;
}

/// In-memory map-backed `SecretProvider` for tests and local sandboxes.
#[derive(Default, Clone)]
pub struct MapSecretProvider {
    secrets: HashMap<String, String>,
}

impl MapSecretProvider {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn insert(&mut self, name: impl Into<String>, value: impl Into<String>) {
        self.secrets.insert(name.into(), value.into());
    }
}

impl SecretProvider for MapSecretProvider {
    fn resolve(&self, secret_name: &str) -> Option<String> {
        self.secrets.get(secret_name).cloned()
    }
}

/// Environment-variable-backed `SecretProvider` with optional prefix (e.g. `"INFIMUM_SECRET_"`).
#[derive(Clone)]
pub struct EnvSecretProvider {
    prefix: String,
}

impl EnvSecretProvider {
    pub fn new(prefix: impl Into<String>) -> Self {
        Self {
            prefix: prefix.into(),
        }
    }
}

impl SecretProvider for EnvSecretProvider {
    fn resolve(&self, secret_name: &str) -> Option<String> {
        let key = format!("{}{}", self.prefix, secret_name);
        std::env::var(key).ok()
    }
}

/// Volatile zeroization of a byte slice (prevents compiler dead-store elimination after `cap_execute`).
pub fn zeroize_bytes(buf: &mut [u8]) {
    for b in buf.iter_mut() {
        // SAFETY: `b` is a valid, aligned, mutable reference to a `u8` inside `buf`.
        unsafe {
            std::ptr::write_volatile(b, 0u8);
        }
    }
    compiler_fence(Ordering::SeqCst);
}
