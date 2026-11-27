// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial
//
// End-to-end security, governance, and isolation tests for `infimum-rt` (P6).

use infimum_lang::parser::parse_module_from_path;
use infimum_lang::passport_crypto::generate_keypair;
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module_signed;
use infimum_rt::net::validate_outbound_url;
use infimum_rt::{CapsuleEngine, ExecutionPolicy, MapSecretProvider, RtError, SecretProvider};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Instant, SystemTime, UNIX_EPOCH};

fn workspace_root() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .parent()
        .expect("crates/")
        .parent()
        .expect("workspace root")
        .to_path_buf()
}

fn temp_dir(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_rt_test_{tag}_{nanos}"));
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn compile_ifm_file(path: &Path, sign_key: Option<&[u8; 32]>) -> Vec<u8> {
    let dir = temp_dir("compile");
    let store_dir = dir.join("store");
    let out_ifw = dir.join("out.ifw");
    let module = parse_module_from_path(path).expect("parse_module_from_path");
    let mut store = GraphStore::open(&store_dir).expect("GraphStore::open");
    let _artifact =
        build_strata_wasm_from_module_signed(&mut store, &module, &out_ifw, true, sign_key)
            .expect("build_strata_wasm_from_module_signed");
    let bytes = std::fs::read(&out_ifw).expect("read .ifw");
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

fn compile_ifm_inline(source: &str, sign_key: Option<&[u8; 32]>) -> Vec<u8> {
    let dir = temp_dir("inline");
    let src_path = dir.join("capsule.ifm");
    std::fs::write(&src_path, source).expect("write inline .ifm");
    let bytes = compile_ifm_file(&src_path, sign_key);
    let _ = std::fs::remove_dir_all(&dir);
    bytes
}

#[test]
fn test_e2e_three_b2b_reference_capsules() {
    let root = workspace_root();
    let b2b_dir = root.join("crates/infimum-lang/examples/b2b");

    // 1. FinTech Pricing & Tax Engine (`dec64` exact money)
    let pricing_ifw = compile_ifm_file(&b2b_dir.join("fintech_pricing.ifm"), None);
    let policy = ExecutionPolicy::default();
    let pricing_cap = CapsuleEngine::load_ifw(pricing_ifw, &policy).expect("load fintech_pricing");
    let res = CapsuleEngine::execute_json(
        &pricing_cap,
        r#"{"amount": 100.00, "discount_pct": 0.10, "tax_rate": 0.20, "currency": "EUR"}"#,
        None,
        &policy,
    )
    .expect("execute fintech_pricing");
    assert!(res.ok, "fintech_pricing failed: {res:?}");
    let v: Value = serde_json::from_str(&res.output_json).expect("valid JSON output");
    assert_eq!(v["status"], "ok");
    assert_eq!(v["currency"], "EUR");
    assert_eq!(v["total_cents"], 10800);
    assert!(res.fuel_consumed > 0);

    // 2. RBAC & Quota Policy Validator
    let policy_ifw = compile_ifm_file(&b2b_dir.join("policy_validator.ifm"), None);
    let policy_cap = CapsuleEngine::load_ifw(policy_ifw, &policy).expect("load policy_validator");
    let ok_res = CapsuleEngine::execute_json(
        &policy_cap,
        r#"{"role": "analyst", "action": "write", "daily_used": 2500, "requested": 500}"#,
        None,
        &policy,
    )
    .expect("execute policy_validator (allow)");
    let ok_v: Value = serde_json::from_str(&ok_res.output_json).expect("valid JSON");
    assert_eq!(ok_v["allowed"], true);
    assert_eq!(ok_v["reason"], "approved");
    assert_eq!(ok_v["remaining_quota"], 7000);

    let deny_res = CapsuleEngine::execute_json(
        &policy_cap,
        r#"{"role": "viewer", "action": "delete", "daily_used": 0, "requested": 1}"#,
        None,
        &policy,
    )
    .expect("execute policy_validator (deny)");
    let deny_v: Value = serde_json::from_str(&deny_res.output_json).expect("valid JSON");
    assert_eq!(deny_v["allowed"], false);
    assert_eq!(deny_v["reason"], "action_forbidden_for_role");

    // 3. Zero-Trust Secret API Rule
    let stripe_ifw = compile_ifm_file(&b2b_dir.join("secret_api_rule.ifm"), None);
    let mut stripe_policy = ExecutionPolicy::default();
    stripe_policy
        .allowed_net_domains
        .insert("api.stripe.com".to_string());
    let stripe_cap =
        CapsuleEngine::load_ifw(stripe_ifw, &stripe_policy).expect("load secret_api_rule");
    let mut secrets = MapSecretProvider::new();
    secrets.insert("api.stripe.com", "sk_live_mock_key_0123456789abcdef");
    let secret_val = secrets.resolve("api.stripe.com").expect("vault secret");

    let stripe_res = CapsuleEngine::execute_json_with_http(
        &stripe_cap,
        r#"{"amount": 49.95, "currency": "USD"}"#,
        Some(&secret_val),
        &stripe_policy,
        Some(&|_method, _url, _auth, _body| Ok(b"{\"id\":\"ch_ok\"}".to_vec())),
    )
    .expect("execute secret_api_rule");
    let sv: Value = serde_json::from_str(&stripe_res.output_json).expect("valid JSON");
    assert_eq!(sv["status"], "dispatched");
    assert_eq!(sv["provider"], "api.stripe.com");
    assert_eq!(sv["currency"], "USD");
    assert!(stripe_res.secret_buffer_zeroized);
}

#[test]
fn test_signed_capsule_and_tamper_rejection() {
    let root = workspace_root();
    let src = root.join("crates/infimum-lang/examples/b2b/fintech_pricing.ifm");
    let (sk, pk) = generate_keypair();

    let signed_ifw = compile_ifm_file(&src, Some(&sk));
    let unsigned_ifw = compile_ifm_file(&src, None);

    let strict_policy = ExecutionPolicy::strict(vec![pk]);

    // Valid signed capsule passes load & execution
    let cap = CapsuleEngine::load_ifw(signed_ifw.clone(), &strict_policy)
        .expect("signed capsule must pass strict policy");
    let res = CapsuleEngine::execute_json(
        &cap,
        r#"{"amount": 50.00, "tax_rate": 0.10}"#,
        None,
        &strict_policy,
    )
    .expect("execute signed capsule");
    let v: Value = serde_json::from_str(&res.output_json).expect("json");
    assert_eq!(v["total_cents"], 5500);

    // Unsigned capsule is rejected BEFORE wasmi instantiation
    let unsigned_err = CapsuleEngine::load_ifw(unsigned_ifw, &strict_policy)
        .err()
        .expect("unsigned capsule must fail strict policy");
    assert!(
        matches!(
            unsigned_err,
            RtError::PassportInvalid(_) | RtError::PolicyViolation(_)
        ),
        "expected PassportInvalid or PolicyViolation, got {unsigned_err:?}"
    );

    // 1-byte tamper in signed .ifw is rejected BEFORE wasmi instantiation
    let mut tampered = signed_ifw;
    let mid = tampered.len() / 2;
    tampered[mid] ^= 0x01;
    let tamper_err = CapsuleEngine::load_ifw(tampered, &strict_policy)
        .err()
        .expect("tampered .ifw must be rejected");
    assert!(
        matches!(tamper_err, RtError::PassportInvalid(_)),
        "expected PassportInvalid, got {tamper_err:?}"
    );
}

#[test]
fn test_fuel_limit_stops_infinite_loop_fast() {
    let src = r#"
use "std/capsule.ifm";

export fn cap_execute(in_len: i64) -[Mem]-> i64 {
  let _in = cap_input(in_len);
  let mut i: i64 = 0;
  while i >= 0 max 10000000 {
    i = i + 1;
  }
  0
}
"#;
    let ifw = compile_ifm_inline(src, None);
    let mut policy = ExecutionPolicy::default();
    policy.require_z3_proved = false;
    policy.fuel_limit = 100_000;

    let cap = CapsuleEngine::load_ifw(ifw, &policy).expect("load infinite loop capsule");
    let t0 = Instant::now();
    let err = CapsuleEngine::execute_json(&cap, "{}", None, &policy)
        .err()
        .expect("infinite loop must trap with OutOfFuel");
    let elapsed_ms = t0.elapsed().as_millis();

    assert!(
        matches!(err, RtError::OutOfFuel { .. }),
        "expected RtError::OutOfFuel, got {err:?}"
    );
    assert!(
        elapsed_ms < 50,
        "fuel exhaustion should be fast (<50ms), took {elapsed_ms}ms"
    );
}

#[test]
fn test_memory_limit_rejected_on_load() {
    let root = workspace_root();
    let src = root.join("crates/infimum-lang/examples/b2b/fintech_pricing.ifm");
    let ifw = compile_ifm_file(&src, None);

    let mut tight_policy = ExecutionPolicy::default();
    tight_policy.max_memory_bytes = 64 * 1024; // 64 KiB < 512 KiB capsule memory

    let err = CapsuleEngine::load_ifw(ifw, &tight_policy)
        .err()
        .expect("capsule exceeding max_memory_bytes must be rejected on load");
    assert!(
        matches!(err, RtError::PolicyViolation(_)),
        "expected PolicyViolation, got {err:?}"
    );
}

#[test]
fn test_vault_secret_injected_and_zeroized() {
    let root = workspace_root();
    let src = root.join("crates/infimum-lang/examples/b2b/secret_api_rule.ifm");
    let ifw = compile_ifm_file(&src, None);

    let mut policy = ExecutionPolicy::default();
    policy
        .allowed_net_domains
        .insert("api.stripe.com".to_string());

    let cap = CapsuleEngine::load_ifw(ifw, &policy).expect("load secret_api_rule");
    let mut vault = MapSecretProvider::new();
    let secret_raw = "sk_live_zero_trust_vault_9876543";
    vault.insert("api.stripe.com", secret_raw);
    let secret_val = vault.resolve("api.stripe.com").expect("vault secret");

    let observed_calls: Arc<Mutex<Vec<(String, Option<String>)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let obs_clone = Arc::clone(&observed_calls);

    let res = CapsuleEngine::execute_json_with_http(
        &cap,
        r#"{"amount": 125.50, "currency": "USD"}"#,
        Some(&secret_val),
        &policy,
        Some(&move |_method, url, auth, _body| {
            obs_clone
                .lock()
                .unwrap()
                .push((url.to_string(), auth.map(str::to_string)));
            Ok(b"{\"ok\":true}".to_vec())
        }),
    )
    .expect("execute secret_api_rule");
    assert!(res.ok, "secret_api_rule failed: {res:?}");

    // 1. Secret was passed to the allowed Net("api.stripe.com") endpoint via Authorization header
    let calls = observed_calls.lock().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].0, "https://api.stripe.com/v1/charges");
    assert_eq!(
        calls[0].1.as_deref(),
        Some("Bearer sk_live_zero_trust_vault_9876543")
    );

    // 2. Secret did NOT leak into JSON output
    assert!(
        !res.output_json.contains(secret_raw),
        "secret must never appear in capsule output_json"
    );

    // 3. Linear memory secret_buf was zeroized via volatile write after cap_execute
    assert!(
        res.secret_buffer_zeroized,
        "secret_buf must be zeroized after cap_execute"
    );
}

#[test]
fn test_anti_ssrf_blocks_imds_loopback_and_userinfo() {
    let mut allowed = BTreeSet::new();
    allowed.insert("api.stripe.com".to_string());
    allowed.insert("169.254.169.254".to_string());
    allowed.insert("127.0.0.1".to_string());
    allowed.insert("10.0.0.1".to_string());

    let blocked_urls = [
        "http://169.254.169.254/latest/meta-data/",
        "https://169.254.169.254/latest/meta-data/",
        "http://127.0.0.1:8080/admin",
        "https://127.0.0.1:8080/admin",
        "https://api.stripe.com:443@evil.com/steal",
        "https://10.0.0.1/internal",
        "https://[::1]/",
        "https://api.stripe.com/v1\r\nX-Injected: true",
        "https://evil.com/v1/charges",
    ];

    for url in blocked_urls {
        let res = validate_outbound_url(url, &allowed, false);
        assert!(
            res.is_err(),
            "anti-SSRF MUST block URL '{url}', but it succeeded!"
        );
    }
}

#[test]
fn test_infimum_rt_cargo_tree_zero_gpu() {
    let output = Command::new("cargo")
        .args(["tree", "-p", "infimum-rt"])
        .current_dir(workspace_root())
        .output()
        .expect("run cargo tree -p infimum-rt");
    assert!(
        output.status.success(),
        "cargo tree -p infimum-rt failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let tree = String::from_utf8_lossy(&output.stdout);
    for forbidden in ["wgpu", "winit", "fontdue", "arboard"] {
        assert!(
            !tree.contains(forbidden),
            "infimum-rt dependency tree MUST NOT contain '{forbidden}'"
        );
    }
}
