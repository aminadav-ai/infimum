//! Pre-execution `infimum.passport` Gatekeeper.
//!
//! Parses the embedded Wasm custom section named `infimum.passport` BEFORE
//! instantiating the module. Any capsule requesting unauthorized capabilities
//! is rejected before byte 0 executes.

use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::path::PathBuf;
use std::sync::OnceLock;
use wasmi::{Engine, Linker, Module, Store};

const PASSPORT_AXW: &[u8] = include_bytes!("../capsules/core/passport_core.ifw");
const EFF_BUF_WORD: usize = 16;
const MAX_EFF_CHARS: usize = 256;
const CAP_NET: i32 = 32;

fn passport_module() -> &'static (Engine, Module) {
    static CELL: OnceLock<(Engine, Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::runtime::load_core_axw("passport_core.ifw", PASSPORT_AXW))
}

fn policy_to_allow_mask(policy: &CapabilityPolicy) -> i32 {
    let mut mask = 0i32;
    if policy.allow_gpu {
        mask |= 1;
    }
    if policy.allow_input {
        mask |= 2;
    }
    if policy.allow_clipboard {
        mask |= 4;
    }
    if policy.allow_proc {
        mask |= 8;
    }
    if policy.fs_enabled() {
        mask |= 16;
    }
    if !policy.allowed_net_domains.is_empty() {
        mask |= 32;
    }
    mask
}

/// A single capability entry in `infimum.passport` (either a plain effect string
/// or a structured witness object emitted by `infimum-lang`).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(untagged)]
pub enum CapabilityEntry {
    Name(String),
    Witness {
        effect: String,
        #[serde(default)]
        witness_chain: Vec<String>,
    },
}

impl CapabilityEntry {
    pub fn effect_name(&self) -> &str {
        match self {
            Self::Name(s) => s.as_str(),
            Self::Witness { effect, .. } => effect.as_str(),
        }
    }
}

/// Parsed security passport embedded by `infimum-lang`'s Strata Wasm emitter.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SecurityPassport {
    #[serde(default)]
    pub entrypoint: String,
    #[serde(default)]
    pub root_hash: String,
    #[serde(default)]
    pub short_hash: String,
    #[serde(default)]
    pub aliases: Vec<String>,
    #[serde(default)]
    pub total_reachable_nodes: usize,
    #[serde(default)]
    pub is_pure: bool,
    #[serde(default)]
    pub uses_secret_types: bool,
    #[serde(default)]
    pub effects: Vec<String>,
    #[serde(default)]
    pub capabilities: Vec<CapabilityEntry>,
}

impl SecurityPassport {
    pub fn effect_names(&self) -> Vec<String> {
        let mut out = self.effects.clone();
        for cap in &self.capabilities {
            out.push(cap.effect_name().to_string());
        }
        out
    }

    pub fn display_entry(&self) -> &str {
        if !self.entrypoint.is_empty() {
            &self.entrypoint
        } else if let Some(first) = self.aliases.first() {
            first.as_str()
        } else if !self.short_hash.is_empty() {
            &self.short_hash
        } else {
            "capsule"
        }
    }
}

/// Host capability policy configured via CLI flags or user prompt.
#[derive(Debug, Clone)]
pub struct CapabilityPolicy {
    pub require_passport: bool,
    pub allow_gpu: bool,
    pub allow_input: bool,
    pub allow_clipboard: bool,
    pub allow_proc: bool,
    pub allow_user_workspaces: bool,
    pub allowed_net_domains: BTreeSet<String>,
    pub allowed_fs_roots: Vec<PathBuf>,
    /// Explicit key=value launch arguments passed via `--cap-arg=KEY=VAL`.
    /// Never exposes arbitrary OS environment variables to capsules.
    pub cap_args: BTreeMap<String, String>,
}

impl CapabilityPolicy {
    pub fn fs_enabled(&self) -> bool {
        self.allow_user_workspaces || !self.allowed_fs_roots.is_empty()
    }
}

impl Default for CapabilityPolicy {
    fn default() -> Self {
        Self {
            require_passport: true,
            allow_gpu: true,
            allow_input: true,
            allow_clipboard: false,
            allow_proc: false,
            allow_user_workspaces: false,
            allowed_net_domains: BTreeSet::new(),
            allowed_fs_roots: Vec::new(),
            cap_args: BTreeMap::new(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum GatekeeperError {
    InvalidWasmMagic,
    MalformedSection,
    MissingPassport,
    InvalidPassportJson(String),
    UnauthorizedCapability(String),
}

impl std::fmt::Display for GatekeeperError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::InvalidWasmMagic => write!(f, "Invalid WebAssembly binary magic header"),
            Self::MalformedSection => write!(f, "Malformed WebAssembly section structure"),
            Self::MissingPassport => {
                write!(f, "Blocked: capsule is missing required `infimum.passport` section")
            }
            Self::InvalidPassportJson(e) => write!(f, "Corrupted `infimum.passport` JSON: {e}"),
            Self::UnauthorizedCapability(cap) => {
                write!(f, "Blocked by infimum.passport Gatekeeper: unauthorized capability `{cap}`")
            }
        }
    }
}

impl std::error::Error for GatekeeperError {}

fn read_leb_u32(bytes: &[u8], cursor: &mut usize) -> Result<u32, GatekeeperError> {
    let mut result: u32 = 0;
    let mut shift: u32 = 0;
    loop {
        let byte = *bytes.get(*cursor).ok_or(GatekeeperError::MalformedSection)?;
        *cursor += 1;
        result |= u32::from(byte & 0x7F) << shift;
        if (byte & 0x80) == 0 {
            return Ok(result);
        }
        shift += 7;
        if shift >= 35 {
            return Err(GatekeeperError::MalformedSection);
        }
    }
}

/// Extract raw JSON bytes of the custom section `infimum.passport` from a Wasm binary.
pub fn extract_passport_raw(wasm_bytes: &[u8]) -> Result<Option<Vec<u8>>, GatekeeperError> {
    if wasm_bytes.len() < 8 || &wasm_bytes[0..4] != b"\x00asm" {
        return Err(GatekeeperError::InvalidWasmMagic);
    }

    let mut cursor = 8usize;
    while cursor < wasm_bytes.len() {
        let sec_id = wasm_bytes[cursor];
        cursor += 1;
        let sec_len = read_leb_u32(wasm_bytes, &mut cursor)? as usize;
        let sec_end = cursor
            .checked_add(sec_len)
            .ok_or(GatekeeperError::MalformedSection)?;
        if sec_end > wasm_bytes.len() {
            return Err(GatekeeperError::MalformedSection);
        }

        if sec_id == 0 {
            // Custom section: name_len + utf8 name + payload
            let mut inner = cursor;
            let name_len = read_leb_u32(wasm_bytes, &mut inner)? as usize;
            let name_end = inner
                .checked_add(name_len)
                .ok_or(GatekeeperError::MalformedSection)?;
            if name_end > sec_end {
                return Err(GatekeeperError::MalformedSection);
            }
            if let Ok(name) = std::str::from_utf8(&wasm_bytes[inner..name_end]) {
                if name == "infimum.passport" {
                    return Ok(Some(wasm_bytes[name_end..sec_end].to_vec()));
                }
            }
        }
        cursor = sec_end;
    }

    Ok(None)
}

/// Inspect and enforce `infimum.passport` against the active host capability policy.
pub fn verify_capsule_passport(
    wasm_bytes: &[u8],
    policy: &CapabilityPolicy,
) -> Result<SecurityPassport, GatekeeperError> {
    let raw_opt = extract_passport_raw(wasm_bytes)?;
    let passport = match raw_opt {
        Some(raw) => serde_json::from_slice::<SecurityPassport>(&raw)
            .map_err(|e| GatekeeperError::InvalidPassportJson(e.to_string()))?,
        None => {
            if policy.require_passport {
                return Err(GatekeeperError::MissingPassport);
            }
            SecurityPassport::default()
        }
    };

    // Enforce every declared effect & capability via the Z3-proved `passport_core.ifw` Gatekeeper.
    let (engine, module) = passport_module();
    let mut store = Store::new(engine, ());
    store
        .set_fuel(5_000_000)
        .map_err(|_| GatekeeperError::MalformedSection)?;
    let linker = Linker::new(engine);
    let inst = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
        .map_err(|_| GatekeeperError::MalformedSection)?;
    let memory = inst
        .get_memory(&store, "memory")
        .ok_or(GatekeeperError::MalformedSection)?;
    let classify_fn = inst
        .get_typed_func::<i32, i32>(&store, "classify_effect_ifm")
        .map_err(|_| GatekeeperError::MalformedSection)?;
    let check_fn = inst
        .get_typed_func::<(i32, i32), i32>(&store, "check_policy_mask_ifm")
        .map_err(|_| GatekeeperError::MalformedSection)?;

    let allow_mask = policy_to_allow_mask(policy);
    for token in passport.effect_names() {
        let norm = token.trim();
        let chars: Vec<u32> = norm.chars().take(MAX_EFF_CHARS).map(|c| c as u32).collect();
        {
            let mem = memory.data_mut(&mut store);
            for (idx, &cp) in chars.iter().enumerate() {
                let off = (EFF_BUF_WORD + idx) * 4;
                if off + 4 <= mem.len() {
                    mem[off..off + 4].copy_from_slice(&cp.to_le_bytes());
                }
            }
        }
        let cap_bit = classify_fn
            .call(&mut store, chars.len() as i32)
            .map_err(|_| GatekeeperError::UnauthorizedCapability(norm.to_string()))?;
        let denied = check_fn
            .call(&mut store, (cap_bit, allow_mask))
            .map_err(|_| GatekeeperError::UnauthorizedCapability(norm.to_string()))?;
        if denied != 0 {
            return Err(GatekeeperError::UnauthorizedCapability(norm.to_string()));
        }
        // If Net capability specifies an explicit domain like `Net(example.com)`, verify allowlist membership.
        if cap_bit == CAP_NET {
            if let (Some(l), Some(r)) = (norm.find('('), norm.rfind(')')) {
                if l + 1 < r {
                    let dom = norm[l + 1..r].trim().to_ascii_lowercase();
                    if !policy.allowed_net_domains.contains(&dom) {
                        return Err(GatekeeperError::UnauthorizedCapability(norm.to_string()));
                    }
                }
            }
        }
    }

    Ok(passport)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn build_minimal_wasm_with_passport(passport_json: Option<&str>) -> Vec<u8> {
        let mut out = b"\x00asm\x01\x00\x00\x00".to_vec();
        if let Some(json) = passport_json {
            let name = b"infimum.passport";
            let mut payload = Vec::new();
            payload.push(name.len() as u8);
            payload.extend_from_slice(name);
            payload.extend_from_slice(json.as_bytes());

            out.push(0x00); // custom section
            // LEB128 length (valid up to 16383 bytes for tests)
            let mut n = payload.len();
            loop {
                let mut byte = (n & 0x7F) as u8;
                n >>= 7;
                if n != 0 {
                    byte |= 0x80;
                }
                out.push(byte);
                if n == 0 {
                    break;
                }
            }
            out.extend_from_slice(&payload);
        }
        out
    }

    #[test]
    fn blocks_missing_passport_by_default() {
        let wasm = build_minimal_wasm_with_passport(None);
        let res = verify_capsule_passport(&wasm, &CapabilityPolicy::default());
        assert_eq!(res.unwrap_err(), GatekeeperError::MissingPassport);
    }

    #[test]
    fn allows_gpu_and_mem_capsule() {
        let wasm = build_minimal_wasm_with_passport(Some(
            r#"{"entrypoint":"step_studio","root_hash":"abc123","effects":["Mem","Gpu"]}"#,
        ));
        let pass = verify_capsule_passport(&wasm, &CapabilityPolicy::default()).unwrap();
        assert_eq!(pass.entrypoint, "step_studio");
    }

    #[test]
    fn blocks_unauthorized_spyware_net_and_fs() {
        let wasm = build_minimal_wasm_with_passport(Some(
            r#"{"entrypoint":"evil","effects":["Mem","Net(spy.evil.com)"]}"#,
        ));
        let err = verify_capsule_passport(&wasm, &CapabilityPolicy::default()).unwrap_err();
        assert_eq!(
            err,
            GatekeeperError::UnauthorizedCapability("Net(spy.evil.com)".to_string())
        );
    }

    #[test]
    fn allows_whitelisted_net_domain() {
        let wasm = build_minimal_wasm_with_passport(Some(
            r#"{"entrypoint":"studio","effects":["Gpu","Net(generativelanguage.googleapis.com)"]}"#,
        ));
        let mut pol = CapabilityPolicy::default();
        pol.allowed_net_domains
            .insert("generativelanguage.googleapis.com".to_string());
        assert!(verify_capsule_passport(&wasm, &pol).is_ok());
    }
}
