// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use crate::net::{safe_https_request, validate_outbound_url};
use crate::policy::ExecutionPolicy;
use crate::vault::zeroize_bytes;
use crate::RtError;
use infimum_lang::passport_crypto::verify_passport_signature;
use infimum_lang::store::SecurityPassport;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;
use std::time::Instant;
use wasmi::{Caller, Config, Engine, Linker, Module, ResourceLimiter, Store};

/// Pre-verified, pre-parsed `.ifw` capsule ready for fast repeated sandboxed execution.
pub struct CompiledCapsule {
    pub ifw_bytes: Vec<u8>,
    pub passport: SecurityPassport,
    pub verified_signature: bool,
    pub wasm_engine: Engine,
    pub wasm_module: Module,
    pub passport_net_domains: BTreeSet<String>,
}

/// Deterministic execution result returned by `CapsuleEngine::execute_json`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ExecuteResult {
    pub ok: bool,
    pub output_json: String,
    pub error_code: Option<i64>,
    pub fuel_consumed: u64,
    pub elapsed_us: u64,
    pub http_requests_made: u32,
    pub passport_root_hash: String,
    /// Diagnostic check for tests: confirms `secret_buf` was zeroized after execution.
    #[serde(skip)]
    pub secret_buffer_zeroized: bool,
}

/// Optional HTTP transport hook (defaults to hardened `net::safe_https_request`; tests may inject
/// a deterministic mock transport while still running full SSRF + allowlist validation).
pub type HttpTransportFn = dyn Fn(&str, &str, Option<&str>, Option<&str>) -> Result<Vec<u8>, RtError>;

struct RtLimiter {
    max_memory_bytes: usize,
}

impl ResourceLimiter for RtLimiter {
    fn memory_growing(
        &mut self,
        _current: usize,
        desired: usize,
        _maximum: Option<usize>,
    ) -> Result<bool, wasmi::errors::MemoryError> {
        Ok(desired <= self.max_memory_bytes)
    }

    fn table_growing(
        &mut self,
        _current: u32,
        desired: u32,
        _maximum: Option<u32>,
    ) -> Result<bool, wasmi::errors::TableError> {
        Ok(desired <= 1024)
    }
}

struct HostState<'a> {
    limiter: RtLimiter,
    effective_domains: BTreeSet<String>,
    max_http_requests: u32,
    http_requests_made: u32,
    http_timeout_ms: u64,
    max_http_response_bytes: usize,
    secret_value: Option<String>,
    custom_http: Option<&'a HttpTransportFn>,
}

/// Extract `Net("domain")` allowlist entries from a `SecurityPassport`.
fn extract_passport_net_domains(passport: &SecurityPassport) -> BTreeSet<String> {
    let mut domains = BTreeSet::new();
    for cap in &passport.capabilities {
        let trimmed = cap.effect.trim();
        if let Some(inner) = trimmed
            .strip_prefix("Net(\"")
            .and_then(|s| s.strip_suffix("\")"))
        {
            domains.insert(inner.to_ascii_lowercase());
        }
    }
    domains
}

/// Zero-GPU Headless B2B Capsule Engine.
pub struct CapsuleEngine;

impl CapsuleEngine {
    /// Load a `.ifw` binary, verify `blake3` code integrity, Ed25519 signature, and
    /// `ExecutionPolicy` **before** instantiating `wasmi`.
    pub fn load_ifw(
        ifw_bytes: Vec<u8>,
        policy: &ExecutionPolicy,
    ) -> Result<CompiledCapsule, RtError> {
        let verified = verify_passport_signature(&ifw_bytes, &policy.trusted_pubkeys)
            .map_err(|d| RtError::PassportInvalid(d.to_string()))?;

        if policy.require_signature && !verified.signature_verified {
            return Err(RtError::PolicyViolation(
                "ExecutionPolicy requires a valid Ed25519 signature, but capsule is unsigned"
                    .to_string(),
            ));
        }

        if policy.require_z3_proved && verified.passport.z3_obligations_proved == 0 {
            return Err(RtError::PolicyViolation(
                "ExecutionPolicy requires Z3 formal proof, but z3_obligations_proved=0".to_string(),
            ));
        }

        if (verified.passport.max_memory_bytes as usize) > policy.max_memory_bytes {
            return Err(RtError::PolicyViolation(format!(
                "Capsule declares max_memory_bytes={} which exceeds policy.max_memory_bytes={}",
                verified.passport.max_memory_bytes, policy.max_memory_bytes
            )));
        }

        let passport_net_domains = extract_passport_net_domains(&verified.passport);

        let mut cfg = Config::default();
        cfg.consume_fuel(true);
        let wasm_engine = Engine::new(&cfg);
        let wasm_module = Module::new(&wasm_engine, &ifw_bytes)
            .map_err(|e| RtError::WasmError(format!("Failed to parse Wasm module: {e}")))?;

        Ok(CompiledCapsule {
            ifw_bytes,
            passport: verified.passport,
            verified_signature: verified.signature_verified,
            wasm_engine,
            wasm_module,
            passport_net_domains,
        })
    }

    /// Execute a loaded capsule with `input_json`, optional Zero-Trust `secret_value`,
    /// and strict `ExecutionPolicy` enforcement (`fuel_limit`, `max_memory_bytes`, `zeroize`).
    pub fn execute_json(
        capsule: &CompiledCapsule,
        input_json: &str,
        secret_value: Option<&str>,
        policy: &ExecutionPolicy,
    ) -> Result<ExecuteResult, RtError> {
        Self::execute_json_with_http(capsule, input_json, secret_value, policy, None)
    }

    /// Execute a loaded capsule with an optional custom HTTP transport (used by deterministic tests
    /// while still enforcing full `validate_outbound_url` + passport∩policy domain intersection).
    pub fn execute_json_with_http(
        capsule: &CompiledCapsule,
        input_json: &str,
        secret_value: Option<&str>,
        policy: &ExecutionPolicy,
        custom_http: Option<&HttpTransportFn>,
    ) -> Result<ExecuteResult, RtError> {
        let start = Instant::now();

        // Effective network allowlist = intersection(passport Net("..."), policy.allowed_net_domains)
        let effective_domains: BTreeSet<String> = capsule
            .passport_net_domains
            .intersection(&policy.allowed_net_domains)
            .cloned()
            .collect();

        let state = HostState {
            limiter: RtLimiter {
                max_memory_bytes: policy.max_memory_bytes,
            },
            effective_domains,
            max_http_requests: policy.max_http_requests,
            http_requests_made: 0,
            http_timeout_ms: policy.http_timeout_ms,
            max_http_response_bytes: policy.max_http_response_bytes,
            secret_value: secret_value.map(str::to_string),
            custom_http,
        };

        let mut store = Store::new(&capsule.wasm_engine, state);
        store.limiter(|s| &mut s.limiter);
        store
            .set_fuel(policy.fuel_limit)
            .map_err(|e| RtError::WasmError(format!("Failed to set Wasm fuel: {e}")))?;

        let mut linker = Linker::<HostState<'_>>::new(&capsule.wasm_engine);
        linker
            .func_wrap(
                "env",
                "host_call",
                |mut caller: Caller<'_, HostState<'_>>,
                 op: i64,
                 a0: i64,
                 a1: i64,
                 a2: i64|
                 -> i64 { handle_host_call(&mut caller, op, a0, a1, a2) },
            )
            .map_err(|e| RtError::WasmError(format!("Failed to link env.host_call: {e}")))?;

        let instance = linker
            .instantiate(&mut store, &capsule.wasm_module)
            .map_err(|e| RtError::WasmError(format!("Wasm instantiation failed: {e}")))?
            .start(&mut store)
            .map_err(|e| RtError::WasmError(format!("Wasm start failed: {e}")))?;

        let memory = instance
            .get_memory(&store, "memory")
            .ok_or_else(|| RtError::InvalidCapsuleAbi("Missing exported 'memory'".to_string()))?;

        let mem_len = memory.data(&store).len();
        if mem_len > policy.max_memory_bytes {
            return Err(RtError::PolicyViolation(format!(
                "Instantiated Wasm memory ({mem_len} bytes) exceeds policy.max_memory_bytes ({})",
                policy.max_memory_bytes
            )));
        }

        let abi_ver = call_i64_probe(&instance, &mut store, "ifw_capsule_abi_version")?;
        if abi_ver != 1 {
            return Err(RtError::InvalidCapsuleAbi(format!(
                "Unsupported ifw_capsule_abi_version={abi_ver} (expected 1)"
            )));
        }
        let in_off = call_i64_probe(&instance, &mut store, "ifw_cap_input_byte_offset")? as usize;
        let in_cap = call_i64_probe(&instance, &mut store, "ifw_cap_input_cap_bytes")? as usize;
        let out_off = call_i64_probe(&instance, &mut store, "ifw_cap_output_byte_offset")? as usize;
        let out_cap = call_i64_probe(&instance, &mut store, "ifw_cap_output_cap_bytes")? as usize;
        let sec_off = call_i64_probe(&instance, &mut store, "ifw_cap_secret_byte_offset")? as usize;
        let sec_cap = call_i64_probe(&instance, &mut store, "ifw_cap_secret_cap_bytes")? as usize;

        if in_off.saturating_add(in_cap) > mem_len
            || out_off.saturating_add(out_cap) > mem_len
            || sec_off.saturating_add(sec_cap) > mem_len
        {
            return Err(RtError::InvalidCapsuleAbi(
                "Capsule I/O buffer offsets exceed linear memory bounds".to_string(),
            ));
        }

        let in_bytes = input_json.as_bytes();
        if in_bytes.len() > in_cap {
            return Err(RtError::InputTooLarge {
                actual: in_bytes.len(),
                capacity: in_cap,
            });
        }

        let sec_bytes = secret_value.map(str::as_bytes).unwrap_or(&[]);
        if sec_bytes.len() > sec_cap {
            return Err(RtError::InputTooLarge {
                actual: sec_bytes.len(),
                capacity: sec_cap,
            });
        }

        // Write input_json and optional Vault secret into linear memory
        {
            let mem = memory.data_mut(&mut store);
            mem[in_off..in_off + in_bytes.len()].copy_from_slice(in_bytes);
            if !sec_bytes.is_empty() {
                mem[sec_off..sec_off + sec_bytes.len()].copy_from_slice(sec_bytes);
            }
        }

        // Invoke `cap_execute_i64(in_len)` (or fallback to `cap_execute`)
        let exec_res: Result<i64, wasmi::Error> =
            if let Ok(f64) = instance.get_typed_func::<i64, i64>(&store, "cap_execute_i64") {
                f64.call(&mut store, in_bytes.len() as i64)
            } else {
                let f32 = instance
                    .get_typed_func::<i32, i32>(&store, "cap_execute")
                    .map_err(|e| {
                        RtError::InvalidCapsuleAbi(format!("Missing 'cap_execute' export: {e}"))
                    })?;
                f32.call(&mut store, in_bytes.len() as i32).map(i64::from)
            };

        // CRITICAL: Always zeroize `secret_buf` immediately after `cap_execute`, even on trap!
        let secret_buffer_zeroized = {
            let mem = memory.data_mut(&mut store);
            zeroize_bytes(&mut mem[sec_off..sec_off + sec_cap]);
            mem[sec_off..sec_off + sec_cap].iter().all(|&b| b == 0)
        };

        let rc = match exec_res {
            Ok(v) => v,
            Err(e) => {
                if let Some(wasmi::core::TrapCode::OutOfFuel) = e.as_trap_code() {
                    return Err(RtError::OutOfFuel {
                        fuel_limit: policy.fuel_limit,
                    });
                }
                return Err(RtError::WasmError(format!("cap_execute trapped: {e}")));
            }
        };

        let remaining_fuel = store.get_fuel().unwrap_or(0);
        let fuel_consumed = policy.fuel_limit.saturating_sub(remaining_fuel);
        let elapsed_us = start.elapsed().as_micros() as u64;
        let http_requests_made = store.data().http_requests_made;

        if rc >= 0 {
            let out_len = (rc as usize).min(out_cap);
            let mem = memory.data(&store);
            let output_json = String::from_utf8_lossy(&mem[out_off..out_off + out_len]).to_string();
            Ok(ExecuteResult {
                ok: true,
                output_json,
                error_code: None,
                fuel_consumed,
                elapsed_us,
                http_requests_made,
                passport_root_hash: capsule.passport.root_hash.clone(),
                secret_buffer_zeroized,
            })
        } else {
            Ok(ExecuteResult {
                ok: false,
                output_json: String::new(),
                error_code: Some(rc.wrapping_neg()),
                fuel_consumed,
                elapsed_us,
                http_requests_made,
                passport_root_hash: capsule.passport.root_hash.clone(),
                secret_buffer_zeroized,
            })
        }
    }
}

fn call_i64_probe(
    instance: &wasmi::Instance,
    store: &mut Store<HostState<'_>>,
    name: &str,
) -> Result<i64, RtError> {
    let i64_name = format!("{name}_i64");
    if let Ok(f) = instance.get_typed_func::<(), i64>(&*store, &i64_name) {
        f.call(store, ())
            .map_err(|e| RtError::InvalidCapsuleAbi(format!("Probe '{i64_name}' failed: {e}")))
    } else {
        let f = instance
            .get_typed_func::<(), i32>(&*store, name)
            .map_err(|e| RtError::InvalidCapsuleAbi(format!("Missing ABI export '{name}': {e}")))?;
        f.call(store, ())
            .map(i64::from)
            .map_err(|e| RtError::InvalidCapsuleAbi(format!("Probe '{name}' failed: {e}")))
    }
}

/// Minimal `env.host_call` bridge for Headless B2B `Net` calls:
/// - `op`: FNV-1a hash of `"Net.call"` (or any `Net.*` op)
/// - `a0`: byte pointer to URL (`str` fat-ptr or raw byte offset)
/// - `a1`: URL byte length
/// - `a2`: destination byte pointer in linear memory (writes response bytes up to 4096)
///
/// Returns bytes written (`>= 0`) on success, or negative error code (`-1` denied, `-2` budget).
fn handle_host_call(
    caller: &mut Caller<'_, HostState<'_>>,
    _op: i64,
    a0: i64,
    a1: i64,
    a2: i64,
) -> i64 {
    let Some(ext) = caller.get_export("memory") else {
        return -1;
    };
    let Some(mem) = ext.into_memory() else {
        return -1;
    };
    let (mem_bytes, host) = mem.data_and_store_mut(caller);
    if host.http_requests_made >= host.max_http_requests {
        return -2;
    }
    let url_off = usize::try_from(a0).unwrap_or(usize::MAX);
    let url_len = usize::try_from(a1).unwrap_or(usize::MAX);
    let dst_off = usize::try_from(a2).unwrap_or(usize::MAX);
    if url_len == 0
        || url_len > 2048
        || url_off.saturating_add(url_len) > mem_bytes.len()
        || dst_off >= mem_bytes.len()
    {
        return -1;
    }
    let Ok(url) = std::str::from_utf8(&mem_bytes[url_off..url_off + url_len]) else {
        return -1;
    };

    // Always enforce SSRF + allowlist validation
    let resolve_dns = host.custom_http.is_none();
    if validate_outbound_url(url, &host.effective_domains, resolve_dns).is_err() {
        return -1;
    }

    let auth_header = host
        .secret_value
        .as_deref()
        .filter(|s| !s.is_empty())
        .map(|s| format!("Bearer {s}"));

    let resp_res = if let Some(custom) = host.custom_http {
        custom("GET", url, auth_header.as_deref(), None)
    } else {
        safe_https_request(
            "GET",
            url,
            auth_header.as_deref(),
            None,
            &host.effective_domains,
            host.http_timeout_ms,
            host.max_http_response_bytes,
        )
    };

    host.http_requests_made += 1;

    match resp_res {
        Ok(resp_bytes) => {
            let avail = mem_bytes.len().saturating_sub(dst_off);
            let n = resp_bytes.len().min(avail).min(host.max_http_response_bytes);
            mem_bytes[dst_off..dst_off + n].copy_from_slice(&resp_bytes[..n]);
            n as i64
        }
        Err(_) => -1,
    }
}
