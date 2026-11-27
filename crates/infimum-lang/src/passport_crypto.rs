// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial
//
// Cryptographic Integrity & Ed25519 Signing for Infimum Security Passport v2.
//
// Guarantees:
// 1. `wasm_code_hash` = blake3(magic || version || all non-custom Wasm sections).
//    Mutating even 1 byte of Wasm bytecode, memory size, imports, exports, or data
//    immediately triggers `E084_PASSPORT_WASM_HASH_MISMATCH`.
// 2. Optional `"infimum.signature"` custom section holds an Ed25519 signature over
//    `blake3("infimum-passport-v2\0" || passport_json_bytes)`.
//    Mutating 1 byte of the passport or verifying with an untrusted key triggers
//    `E085_PASSPORT_SIGNATURE_INVALID`.
// 3. Duplicate `"infimum.passport"` or `"infimum.signature"` custom sections are
//    rejected with `E086_PASSPORT_DUPLICATE_SECTION` (section-smuggling defense).

use crate::diag::{InfimumDiagnostic, Span};
use crate::store::SecurityPassport;
use ed25519_dalek::{Signature, Signer, SigningKey, Verifier, VerifyingKey};
use serde::{Deserialize, Serialize};

pub const PASSPORT_SECTION_NAME: &str = "infimum.passport";
pub const SIGNATURE_SECTION_NAME: &str = "infimum.signature";
const SIGNATURE_DOMAIN_PREFIX: &[u8] = b"infimum-passport-v2\0";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct SignatureBlock {
    pub alg: String,
    pub key_id: String,
    pub public_key: String,
    pub signature: String,
}

#[derive(Debug, Clone)]
pub struct ExtractedIfw {
    pub passport_json: Vec<u8>,
    pub passport: SecurityPassport,
    pub signature: Option<SignatureBlock>,
    pub computed_wasm_code_hash: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerifiedPassport {
    pub passport: SecurityPassport,
    pub computed_wasm_code_hash: String,
    pub signature: Option<SignatureBlock>,
    pub signature_verified: bool,
}

pub fn encode_hex(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for &b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 0x0F) as usize] as char);
    }
    out
}

pub fn decode_hex(s: &str) -> Result<Vec<u8>, String> {
    let trimmed = s.trim();
    if !trimmed.len().is_multiple_of(2) {
        return Err("Hex string must have even length".to_string());
    }
    let bytes = trimmed.as_bytes();
    let mut out = Vec::with_capacity(bytes.len() / 2);
    for pair in bytes.as_chunks::<2>().0 {
        let hi = hex_nibble(pair[0]).ok_or_else(|| format!("Invalid hex char '{}'", pair[0] as char))?;
        let lo = hex_nibble(pair[1]).ok_or_else(|| format!("Invalid hex char '{}'", pair[1] as char))?;
        out.push((hi << 4) | lo);
    }
    Ok(out)
}

fn hex_nibble(b: u8) -> Option<u8> {
    match b {
        b'0'..=b'9' => Some(b - b'0'),
        b'a'..=b'f' => Some(b - b'a' + 10),
        b'A'..=b'F' => Some(b - b'A' + 10),
        _ => None,
    }
}

pub fn decode_key_32(hex_str: &str) -> Result<[u8; 32], InfimumDiagnostic> {
    let raw = decode_hex(hex_str).map_err(|e| {
        InfimumDiagnostic::new(
            "E085_PASSPORT_SIGNATURE_INVALID",
            "passport-crypto",
            format!("Invalid 32-byte hex key: {e}"),
            Span::zero(),
            "Provide a 64-character hex-encoded Ed25519 key",
        )
    })?;
    let arr: [u8; 32] = raw.try_into().map_err(|_| {
        InfimumDiagnostic::new(
            "E085_PASSPORT_SIGNATURE_INVALID",
            "passport-crypto",
            "Ed25519 key must be exactly 32 bytes (64 hex chars)",
            Span::zero(),
            "Generate a valid keypair with `infimum keygen --out-dir <dir>`",
        )
    })?;
    Ok(arr)
}

pub fn compute_key_id(pubkey: &[u8; 32]) -> String {
    let hash_hex = blake3::hash(pubkey).to_hex().to_string();
    hash_hex[..16].to_string()
}

fn passport_signing_digest(passport_json: &[u8]) -> [u8; 32] {
    let mut hasher = blake3::Hasher::new();
    hasher.update(SIGNATURE_DOMAIN_PREFIX);
    hasher.update(passport_json);
    *hasher.finalize().as_bytes()
}

/// Generate a fresh Ed25519 `(secret_key, public_key)` pair using OS CSPRNG.
#[cfg(not(target_arch = "wasm32"))]
pub fn generate_keypair() -> ([u8; 32], [u8; 32]) {
    use rand_core::RngCore;
    let mut sk_bytes = [0u8; 32];
    rand_core::OsRng.fill_bytes(&mut sk_bytes);
    let signing = SigningKey::from_bytes(&sk_bytes);
    let verifying = signing.verifying_key();
    (sk_bytes, verifying.to_bytes())
}

/// Sign canonical `passport_json` bytes with an Ed25519 32-byte secret key.
pub fn sign_passport(passport_json: &[u8], secret_key: &[u8; 32]) -> SignatureBlock {
    let signing = SigningKey::from_bytes(secret_key);
    let verifying = signing.verifying_key();
    let pub_bytes = verifying.to_bytes();
    let digest = passport_signing_digest(passport_json);
    let sig = signing.sign(&digest);
    SignatureBlock {
        alg: "Ed25519".to_string(),
        key_id: compute_key_id(&pub_bytes),
        public_key: encode_hex(&pub_bytes),
        signature: encode_hex(&sig.to_bytes()),
    }
}

fn read_u32_leb(bytes: &[u8], pos: &mut usize) -> Option<u32> {
    let mut result = 0u32;
    let mut shift = 0u32;
    for _ in 0..5 {
        let b = *bytes.get(*pos)?;
        *pos += 1;
        result |= u32::from(b & 0x7F) << shift;
        if (b & 0x80) == 0 {
            return Some(result);
        }
        shift += 7;
    }
    None
}

/// Parse a `.ifw` / `.wasm` binary, recompute `blake3` over all non-custom sections,
/// reject duplicate `"infimum.passport"` / `"infimum.signature"` sections, and verify
/// `passport.wasm_code_hash` when `schema_version >= 2`.
pub fn extract_wasm_sections(ifw_bytes: &[u8]) -> Result<ExtractedIfw, InfimumDiagnostic> {
    if ifw_bytes.len() < 8 || &ifw_bytes[0..8] != b"\x00asm\x01\x00\x00\x00" {
        return Err(InfimumDiagnostic::new(
            "E083_PASSPORT_MALFORMED_WASM",
            "passport-crypto",
            "Not a valid Wasm 1.0 binary (missing magic header)",
            Span::zero(),
            "Compile a valid .ifw capsule with `infimum compile-wasm`",
        ));
    }

    let mut hasher = blake3::Hasher::new();
    hasher.update(&ifw_bytes[0..8]);

    let mut pos = 8usize;
    let mut passport_json: Option<Vec<u8>> = None;
    let mut signature_json: Option<Vec<u8>> = None;

    while pos < ifw_bytes.len() {
        let sec_start = pos;
        let sec_id = ifw_bytes[pos];
        pos += 1;
        let sec_len = read_u32_leb(ifw_bytes, &mut pos).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E083_PASSPORT_MALFORMED_WASM",
                "passport-crypto",
                "Truncated LEB128 section length in .ifw binary",
                Span::zero(),
                "The .ifw file appears corrupted",
            )
        })? as usize;
        let sec_end = pos.checked_add(sec_len).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E083_PASSPORT_MALFORMED_WASM",
                "passport-crypto",
                "Section length overflow in .ifw binary",
                Span::zero(),
                "The .ifw file appears corrupted",
            )
        })?;
        if sec_end > ifw_bytes.len() {
            return Err(InfimumDiagnostic::new(
                "E083_PASSPORT_MALFORMED_WASM",
                "passport-crypto",
                "Section extends past end of .ifw binary",
                Span::zero(),
                "The .ifw file appears truncated",
            ));
        }

        if sec_id != 0 {
            // Non-custom section: hash exact raw section bytes (id + leb len + payload)
            hasher.update(&ifw_bytes[sec_start..sec_end]);
        } else {
            // Custom section: parse name
            let mut cpos = pos;
            let name_len = read_u32_leb(ifw_bytes, &mut cpos).ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E083_PASSPORT_MALFORMED_WASM",
                    "passport-crypto",
                    "Malformed custom section name length",
                    Span::zero(),
                    "The .ifw file appears corrupted",
                )
            })? as usize;
            let name_end = cpos.checked_add(name_len).ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E083_PASSPORT_MALFORMED_WASM",
                    "passport-crypto",
                    "Custom section name length overflow",
                    Span::zero(),
                    "The .ifw file appears corrupted",
                )
            })?;
            if name_end > sec_end {
                return Err(InfimumDiagnostic::new(
                    "E083_PASSPORT_MALFORMED_WASM",
                    "passport-crypto",
                    "Custom section name extends past section boundary",
                    Span::zero(),
                    "The .ifw file appears corrupted",
                ));
            }
            if let Ok(name) = std::str::from_utf8(&ifw_bytes[cpos..name_end]) {
                let payload = ifw_bytes[name_end..sec_end].to_vec();
                if name == PASSPORT_SECTION_NAME {
                    if passport_json.is_some() {
                        return Err(InfimumDiagnostic::new(
                            "E086_PASSPORT_DUPLICATE_SECTION",
                            "passport-crypto",
                            "Duplicate custom section 'infimum.passport' detected (possible section-smuggling attack)",
                            Span::zero(),
                            "Reject tampered .ifw artifacts with duplicate passport sections",
                        ));
                    }
                    passport_json = Some(payload);
                } else if name == SIGNATURE_SECTION_NAME {
                    if signature_json.is_some() {
                        return Err(InfimumDiagnostic::new(
                            "E086_PASSPORT_DUPLICATE_SECTION",
                            "passport-crypto",
                            "Duplicate custom section 'infimum.signature' detected (possible section-smuggling attack)",
                            Span::zero(),
                            "Reject tampered .ifw artifacts with duplicate signature sections",
                        ));
                    }
                    signature_json = Some(payload);
                }
            }
        }
        pos = sec_end;
    }

    let computed_wasm_code_hash = hasher.finalize().to_hex().to_string();

    let passport_json = passport_json.ok_or_else(|| {
        InfimumDiagnostic::new(
            "E083_PASSPORT_MISSING",
            "passport-crypto",
            "Missing custom section 'infimum.passport' in .ifw binary",
            Span::zero(),
            "Compile with `infimum compile-wasm` to embed a Security Passport",
        )
    })?;

    let passport: SecurityPassport = serde_json::from_slice(&passport_json).map_err(|e| {
        InfimumDiagnostic::new(
            "E083_PASSPORT_INVALID_JSON",
            "passport-crypto",
            format!("Invalid JSON in 'infimum.passport': {e}"),
            Span::zero(),
            "Ensure the .ifw artifact was produced by `infimum compile-wasm`",
        )
    })?;

    if (passport.schema_version >= 2 || !passport.wasm_code_hash.is_empty())
        && passport.wasm_code_hash != computed_wasm_code_hash
    {
        return Err(InfimumDiagnostic::new(
            "E084_PASSPORT_WASM_HASH_MISMATCH",
            "passport-crypto",
            format!(
                "Wasm code hash mismatch: passport declares '{}', computed '{}'",
                passport.wasm_code_hash, computed_wasm_code_hash
            ),
            Span::zero(),
            "The .ifw bytecode was modified after compilation; reject this artifact",
        ));
    }

    let signature = match signature_json {
        Some(raw) => {
            let block: SignatureBlock = serde_json::from_slice(&raw).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    format!("Invalid JSON in 'infimum.signature': {e}"),
                    Span::zero(),
                    "Reject tampered .ifw signature section",
                )
            })?;
            Some(block)
        }
        None => None,
    };

    Ok(ExtractedIfw {
        passport_json,
        passport,
        signature,
        computed_wasm_code_hash,
    })
}

/// Verify both `wasm_code_hash` integrity and (if present or required by `trusted_pubkeys`)
/// the Ed25519 cryptographic signature in `"infimum.signature"`.
pub fn verify_passport_signature(
    ifw_bytes: &[u8],
    trusted_pubkeys: &[[u8; 32]],
) -> Result<VerifiedPassport, InfimumDiagnostic> {
    let extracted = extract_wasm_sections(ifw_bytes)?;
    let require_sig = !trusted_pubkeys.is_empty();

    let signature_verified = match &extracted.signature {
        None => {
            if require_sig {
                return Err(InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    "Missing 'infimum.signature' custom section, but a trusted public key was required",
                    Span::zero(),
                    "Sign the capsule with `infimum compile-wasm ... --sign-key <key>`",
                ));
            }
            false
        }
        Some(sig_block) => {
            if sig_block.alg != "Ed25519" {
                return Err(InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    format!("Unsupported signature algorithm '{}'", sig_block.alg),
                    Span::zero(),
                    "Only 'Ed25519' signatures are supported",
                ));
            }
            let pub_bytes = decode_key_32(&sig_block.public_key)?;
            if require_sig && !trusted_pubkeys.iter().any(|k| k == &pub_bytes) {
                return Err(InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    format!(
                        "Signature public key (key_id={}) is not in the trusted key set",
                        sig_block.key_id
                    ),
                    Span::zero(),
                    "Provide the matching trusted public key or re-sign with an authorized key",
                ));
            }
            let expected_key_id = compute_key_id(&pub_bytes);
            if sig_block.key_id != expected_key_id {
                return Err(InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    "Signature key_id does not match public_key hash",
                    Span::zero(),
                    "Reject tampered signature metadata",
                ));
            }
            let sig_raw = decode_hex(&sig_block.signature).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    format!("Invalid signature hex: {e}"),
                    Span::zero(),
                    "Reject tampered signature bytes",
                )
            })?;
            let sig_arr: [u8; 64] = sig_raw.try_into().map_err(|_| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    "Ed25519 signature must be exactly 64 bytes",
                    Span::zero(),
                    "Reject malformed signature",
                )
            })?;
            let verifying_key = VerifyingKey::from_bytes(&pub_bytes).map_err(|e| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    format!("Invalid Ed25519 public key: {e}"),
                    Span::zero(),
                    "Reject malformed public key",
                )
            })?;
            let signature = Signature::from_bytes(&sig_arr);
            let digest = passport_signing_digest(&extracted.passport_json);
            verifying_key.verify(&digest, &signature).map_err(|_| {
                InfimumDiagnostic::new(
                    "E085_PASSPORT_SIGNATURE_INVALID",
                    "passport-crypto",
                    "Ed25519 signature verification failed (passport or signature was modified)",
                    Span::zero(),
                    "Reject tampered .ifw artifact",
                )
            })?;
            true
        }
    };

    Ok(VerifiedPassport {
        passport: extracted.passport,
        computed_wasm_code_hash: extracted.computed_wasm_code_hash,
        signature: extracted.signature,
        signature_verified,
    })
}
