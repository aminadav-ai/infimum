// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_lang::parser::parse_module;
use infimum_lang::passport_crypto::{
    extract_wasm_sections, generate_keypair, verify_passport_signature,
};
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::{
    build_strata_wasm_from_module, build_strata_wasm_from_module_signed,
};
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_ws(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_p1_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn push_u32_leb(out: &mut Vec<u8>, mut v: u32) {
    loop {
        let mut b = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            b |= 0x80;
        }
        out.push(b);
        if v == 0 {
            break;
        }
    }
}

fn append_custom_section(wasm: &mut Vec<u8>, name: &str, payload: &[u8]) {
    let mut body = Vec::new();
    push_u32_leb(&mut body, name.len() as u32);
    body.extend_from_slice(name.as_bytes());
    body.extend_from_slice(payload);
    wasm.push(0); // custom section id
    push_u32_leb(wasm, body.len() as u32);
    wasm.extend_from_slice(&body);
}

#[test]
fn multi_export_headless_merges_all_capabilities_and_is_not_pure() {
    let ws = temp_ws("multi_export");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        export fn pure_entry(x: i64) -> i64
          ens result == x + 1
        {
          x + 1
        }

        export fn net_entry(amount: i64) -[Net("api.stripe.com")]-> i64
          req amount > 0
        {
          do Net.call(1, amount, 0, 0);
          amount
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("headless.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("compile ok");

    let bytes = fs::read(&ifw_path).unwrap();
    let extracted = extract_wasm_sections(&bytes).expect("extract passport ok");
    assert_eq!(extracted.passport.schema_version, 2);
    assert!(!extracted.passport.is_pure);
    assert!(
        extracted
            .passport
            .capabilities
            .iter()
            .any(|c| c.effect == "Net(\"api.stripe.com\")"),
        "Expected Net(\"api.stripe.com\") in merged passport, got {:?}",
        extracted.passport.capabilities
    );
    assert_eq!(
        extracted.passport.exported_functions,
        vec!["pure_entry".to_string(), "net_entry".to_string()]
    );
    assert!(!extracted.passport.wasm_code_hash.is_empty());
    assert!(extracted.passport.max_memory_bytes >= 8 * 65_536);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn signed_ifw_passes_verify_and_detects_1_byte_code_tampering() {
    let ws = temp_ws("signed_code_tamper");
    let mut store = GraphStore::open(&ws).unwrap();
    let (sk, pk) = generate_keypair();
    let (_other_sk, other_pk) = generate_keypair();

    let src = r#"
        export fn add_safe(a: i64, b: i64) -> i64
          req a >= 0 && b >= 0
          ens result == a + b
        {
          a + b
        }
    "#;
    let module = parse_module(src).unwrap();
    let ifw_path = ws.join("signed.ifw");
    build_strata_wasm_from_module_signed(&mut store, &module, &ifw_path, true, Some(&sk))
        .expect("signed compile ok");

    let bytes = fs::read(&ifw_path).unwrap();

    // 1. Valid verification with trusted pubkey
    let verified = verify_passport_signature(&bytes, &[pk]).expect("valid signature must pass");
    assert!(verified.signature_verified);
    assert_eq!(verified.passport.schema_version, 2);
    assert!(verified.passport.is_pure);

    // 2. Wrong pubkey -> E085_PASSPORT_SIGNATURE_INVALID
    let err_wrong_pk = verify_passport_signature(&bytes, &[other_pk]).unwrap_err();
    assert_eq!(err_wrong_pk.code, "E085_PASSPORT_SIGNATURE_INVALID");

    // 3. Flip 1 byte inside the Code section (id=10) -> E084_PASSPORT_WASM_HASH_MISMATCH
    let mut tampered_code = bytes.clone();
    let mut pos = 8usize;
    let mut flipped = false;
    while pos < tampered_code.len() {
        let sec_id = tampered_code[pos];
        pos += 1;
        let mut sec_len = 0u32;
        let mut shift = 0u32;
        loop {
            let b = tampered_code[pos];
            pos += 1;
            sec_len |= u32::from(b & 0x7F) << shift;
            if (b & 0x80) == 0 {
                break;
            }
            shift += 7;
        }
        if sec_id == 10 && sec_len > 2 {
            tampered_code[pos + (sec_len as usize) - 1] ^= 0x01;
            flipped = true;
            break;
        }
        pos += sec_len as usize;
    }
    assert!(flipped, "Code section must be present");
    let err_code = verify_passport_signature(&tampered_code, &[pk]).unwrap_err();
    assert_eq!(err_code.code, "E084_PASSPORT_WASM_HASH_MISMATCH");

    // 4. Flip 1 byte inside `"infimum.passport"` JSON -> E085_PASSPORT_SIGNATURE_INVALID
    let mut tampered_passport = bytes.clone();
    let needle = b"\"add_safe\"";
    let idx = tampered_passport
        .windows(needle.len())
        .position(|w| w == needle)
        .expect("passport contains add_safe");
    tampered_passport[idx + 1] = b'A'; // "Add_safe" — still valid JSON & same wasm_code_hash!
    let err_passport = verify_passport_signature(&tampered_passport, &[pk]).unwrap_err();
    assert_eq!(err_passport.code, "E085_PASSPORT_SIGNATURE_INVALID");

    // 5. Append duplicate `"infimum.passport"` custom section (section-smuggling) -> E086
    let mut smuggled = bytes.clone();
    append_custom_section(&mut smuggled, "infimum.passport", b"{\"is_pure\":true}");
    let err_smuggle = verify_passport_signature(&smuggled, &[pk]).unwrap_err();
    assert_eq!(err_smuggle.code, "E086_PASSPORT_DUPLICATE_SECTION");

    let _ = fs::remove_dir_all(&ws);
}
