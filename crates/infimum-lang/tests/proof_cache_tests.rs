use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::verify::VerificationReport;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_workspace(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_pcache_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_proof_cache_cold_then_warm_persists_across_store_reopen() {
    let ws = temp_workspace("cold_warm");
    let src = r#"
        fn clamp_pos(x: i64, hi: i64) -> i64
          req hi >= 0
          ens result >= 0 && result <= hi
        {
          if x < 0 { 0 } else if x > hi { hi } else { x }
        }

        fn bounded_step(x: i64) -> i64
          ens result >= 0 && result <= 100
        {
          clamp_pos(x + 1, 100)
        }
    "#;
    let module = parse_module(src).expect("parse ok");

    // 1. Cold run: must execute Z3 (`cached == false`)
    let store1 = GraphStore::open(&ws).expect("store open");
    let r1 = store1.verify_module(&module).expect("cold verify ok");
    assert_eq!(r1.len(), 2);
    assert!(r1.iter().all(|r| !r.cached), "first run must not be cached");
    assert!(ws.join(".infimum/proof_cache.json").is_file());

    // 2. Warm run after reopening GraphStore from disk: must hit cache (`cached == true`)
    let store2 = GraphStore::open(&ws).expect("store reopen");
    let r2 = store2.verify_module(&module).expect("warm verify ok");
    assert_eq!(r2.len(), 2);
    for (cold, warm) in r1.iter().zip(r2.iter()) {
        assert!(warm.cached, "second run of '{}' must be cached", warm.alias);
        assert_eq!(warm.elapsed_ms, 0);
        assert_eq!(warm.status, "PROVED (cached)");
        assert_eq!(warm.obligations_checked, cold.obligations_checked);
    }

    // 3. Explicit bypass (`use_cache = false`): must re-run Z3 (`cached == false`)
    let r3 = store2
        .verify_module_with_cache(&module, false)
        .expect("no-cache verify ok");
    assert!(r3.iter().all(|r| !r.cached), "--no-cache must bypass cache");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_proof_cache_invalidates_caller_when_callee_contract_changes() {
    let ws = temp_workspace("callee_invalidation");
    let src_v1 = r#"
        fn safe_inc(x: i64) -> i64
          req x >= 0
          ens result >= 1
        {
          x + 1
        }

        fn caller(n: i64) -> i64
          req n >= 0
          ens result >= 1
        {
          safe_inc(n)
        }
    "#;
    let mod_v1 = parse_module(src_v1).expect("parse v1 ok");
    let store = GraphStore::open(&ws).expect("store open");

    let r1 = store.verify_module(&mod_v1).expect("v1 cold verify ok");
    assert!(r1.iter().all(|r| !r.cached));

    let r1_warm = store.verify_module(&mod_v1).expect("v1 warm verify ok");
    assert!(r1_warm.iter().all(|r| r.cached));

    // Change callee contract (`ens result >= 2` with `x + 2` — still valid for caller,
    // but callee contract changed so BOTH callee and caller MUST miss cache and re-verify!)
    let src_v2 = r#"
        fn safe_inc(x: i64) -> i64
          req x >= 0
          ens result >= 2
        {
          x + 2
        }

        fn caller(n: i64) -> i64
          req n >= 0
          ens result >= 1
        {
          safe_inc(n)
        }
    "#;
    let mod_v2 = parse_module(src_v2).expect("parse v2 ok");
    let r2 = store.verify_module(&mod_v2).expect("v2 verify ok");
    assert_eq!(r2.len(), 2);
    assert!(!r2[0].cached, "modified callee must miss cache");
    assert!(!r2[1].cached, "caller must miss cache when callee contract changes");

    // Strengthen callee `req` so caller now violates it — Z3 must catch it!
    let src_v3 = r#"
        fn safe_inc(x: i64) -> i64
          req x >= 10
          ens result >= 11
        {
          x + 1
        }

        fn caller(n: i64) -> i64
          req n >= 0
          ens result >= 1
        {
          safe_inc(n)
        }
    "#;
    let mod_v3 = parse_module(src_v3).expect("parse v3 ok");
    let err = store
        .verify_module(&mod_v3)
        .expect_err("caller must fail when callee precondition is strengthened");
    assert_eq!(err.code, "E051_CALL_PRECONDITION_VIOLATED");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_proof_cache_never_caches_failures_and_cli_no_cache_flag() {
    let ws = temp_workspace("cli_nocache");
    let ifm_file = ws.join("sample.ifm");
    let src = r#"
        fn double_pos(x: i64) -> i64
          req x >= 0
          ens result >= x
        {
          x + x
        }
    "#;
    fs::write(&ifm_file, src).unwrap();

    // Failing module must never be cached
    let bad_src = r#"
        fn broken(x: i64) -> i64
          req x >= 0
          ens result > x
        {
          x
        }
    "#;
    let bad_mod = parse_module(bad_src).unwrap();
    let store = GraphStore::open(&ws).unwrap();
    assert_eq!(
        store.verify_module(&bad_mod).unwrap_err().code,
        "E050_CONTRACT_VIOLATION"
    );
    assert_eq!(
        store.verify_module(&bad_mod).unwrap_err().code,
        "E050_CONTRACT_VIOLATION"
    );

    // CLI test: 1) cold verify -> cached == false; 2) warm verify -> cached == true; 3) --no-cache -> cached == false
    let bin = env!("CARGO_BIN_EXE_infimum");

    let out1 = Command::new(bin)
        .args(["verify", ifm_file.to_str().unwrap(), "--json"])
        .current_dir(&ws)
        .output()
        .expect("run infimum verify #1");
    assert!(out1.status.success(), "stderr: {}", String::from_utf8_lossy(&out1.stderr));
    let env1: serde_json::Value =
        serde_json::from_slice(&out1.stdout).expect("parse json report #1");
    assert_eq!(env1["ok"], true);
    let rep1: Vec<VerificationReport> =
        serde_json::from_value(env1["reports"].clone()).expect("parse reports #1");
    assert_eq!(rep1.len(), 1);
    assert!(!rep1[0].cached);

    let out2 = Command::new(bin)
        .args(["verify", ifm_file.to_str().unwrap(), "--json"])
        .current_dir(&ws)
        .output()
        .expect("run infimum verify #2");
    assert!(out2.status.success());
    let env2: serde_json::Value =
        serde_json::from_slice(&out2.stdout).expect("parse json report #2");
    assert_eq!(env2["ok"], true);
    let rep2: Vec<VerificationReport> =
        serde_json::from_value(env2["reports"].clone()).expect("parse reports #2");
    assert_eq!(rep2.len(), 1);
    assert!(rep2[0].cached);
    assert_eq!(rep2[0].status, "PROVED (cached)");

    let out3 = Command::new(bin)
        .args(["verify", ifm_file.to_str().unwrap(), "--json", "--no-cache"])
        .current_dir(&ws)
        .output()
        .expect("run infimum verify --no-cache");
    assert!(out3.status.success());
    let env3: serde_json::Value =
        serde_json::from_slice(&out3.stdout).expect("parse json report #3");
    assert_eq!(env3["ok"], true);
    let rep3: Vec<VerificationReport> =
        serde_json::from_value(env3["reports"].clone()).expect("parse reports #3");
    assert_eq!(rep3.len(), 1);
    assert!(!rep3[0].cached);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn pure_helper_without_ens_body_change_invalidates_caller_proof_cache() {
    let ws = temp_workspace("pure_helper_inlined_body_soundness");
    let store = GraphStore::open(&ws).expect("open store");

    // 1. Sound version: pure helper without `ens` is inlined into `caller`
    let good_src = r#"
        fn helper(x: i64) -> i64 {
            x + 1
        }
        fn caller(x: i64) -> i64
          ens result > x
        {
            helper(x)
        }
    "#;
    let good_mod = parse_module(good_src).unwrap();
    let rep1 = store.verify_module(&good_mod).expect("good version should verify");
    let caller1 = rep1.iter().find(|r| r.alias == "caller").unwrap();
    assert!(!caller1.cached, "first run must invoke Z3");

    // 2. Unchanged re-verify must hit cache
    let rep2 = store.verify_module(&good_mod).expect("warm verify should succeed");
    let caller2 = rep2.iter().find(|r| r.alias == "caller").unwrap();
    assert!(caller2.cached, "unchanged pure helper + caller must hit cache");

    // 3. Mutate pure helper body from `x + 1` to `x - 1` (no `ens`, same signature!)
    // Soundness: caller's proof key MUST change so Z3 rejects the broken caller instead of returning a stale cache hit!
    let bad_src = r#"
        fn helper(x: i64) -> i64 {
            x - 1
        }
        fn caller(x: i64) -> i64
          ens result > x
        {
            helper(x)
        }
    "#;
    let bad_mod = parse_module(bad_src).unwrap();
    let err = store
        .verify_module(&bad_mod)
        .expect_err("mutating inlined pure helper body must invalidate caller cache and fail Z3");
    assert_eq!(err.code, "E050_CONTRACT_VIOLATION");

    let _ = fs::remove_dir_all(&ws);
}
