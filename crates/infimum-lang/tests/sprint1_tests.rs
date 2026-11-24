use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_workspace(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_test_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_semantic_deduplication_across_different_variable_names_and_formatting() {
    let ws = temp_workspace("dedup");
    let src = r#"
        fn add_by_human(left_val: i64, right_val: i64) -> i64
          req left_val >= 0
          ens result == left_val + right_val
        {
          let temp_sum = left_val + right_val;
          temp_sum
        }

        fn sum_by_agent(a: i64, b: i64) -> i64
          req a >= 0
          ens result == a + b
        {
          {
            let res = a + b;
            res
          }
        }
    "#;

    let module = parse_module(src).expect("valid module");
    let mut store = GraphStore::open(&ws).expect("store open");
    let entries = store.ingest_module(&module).expect("ingest ok");

    assert_eq!(entries.len(), 2);
    assert_eq!(
        entries[0].hash, entries[1].hash,
        "Identical logic with different parameter/local names must produce identical BLAKE3 hash"
    );
    assert!(!entries[0].deduplicated);
    assert!(entries[1].deduplicated);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_recursive_functions_share_identical_hash_regardless_of_function_name() {
    let ws = temp_workspace("rec");
    let src = r#"
        fn factorial(n: i64) -> i64
          req n >= 0
        {
          if n <= 1 { 1 } else { n * factorial(n - 1) }
        }

        fn fact_v2(x: i64) -> i64
          req x >= 0
        {
          if x <= 1 { 1 } else { x * fact_v2(x - 1) }
        }
    "#;

    let module = parse_module(src).expect("valid recursive module");
    let mut store = GraphStore::open(&ws).expect("store open");
    let entries = store.ingest_module(&module).expect("ingest ok");

    assert_eq!(entries[0].hash, entries[1].hash);
    assert!(entries[1].deduplicated);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_renaming_alias_never_breaks_callers_or_changes_hashes() {
    let ws = temp_workspace("rename");
    // Also tests out-of-order definitions (caller before callee!)
    let src = r#"
        fn caller(x: i64) -> i64 {
          callee(x) * 2
        }

        fn callee(v: i64) -> i64 {
          v + 10
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let mut store = GraphStore::open(&ws).expect("store open");
    let entries = store.ingest_module(&module).expect("ingest ok");

    let callee_hash_before = store.resolve_ref("callee").unwrap();
    let caller_hash_before = store.resolve_ref("caller").unwrap();
    assert_eq!(entries.len(), 2);

    // Rename `callee` -> `totally_new_name`
    let renamed_hash = store
        .rename_alias("callee", "totally_new_name")
        .expect("rename ok");
    assert_eq!(renamed_hash, callee_hash_before);

    // `caller` hash and its resolved view remain 100% intact!
    let caller_view = store.show_node("caller").expect("caller still valid");
    assert_eq!(caller_view.node.hash, caller_hash_before);
    assert_eq!(caller_view.neighbor_signatures.len(), 1);
    assert_eq!(
        caller_view.neighbor_signatures[0].aliases,
        vec!["totally_new_name".to_string()]
    );

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_transitive_security_passport_and_undeclared_effect_rejection() {
    let ws = temp_workspace("passport");

    // 1. Undeclared direct effect must be rejected immediately!
    let bad_src = r#"
        fn sneaky_backdoor(x: i64) -> i64 {
          do Net.exfiltrate(x);
          x
        }
    "#;
    let bad_mod = parse_module(bad_src).expect("parse ok");
    let mut store = GraphStore::open(&ws).expect("store open");
    let err = store
        .ingest_module(&bad_mod)
        .expect_err("undeclared Net effect must fail");
    assert_eq!(err.code, "E021_UNDECLARED_EFFECT");

    // 2. Pure caller invoking an effectful callee must be rejected transitively!
    let leaky_src = r#"
        fn pure_wrapper(s: Secret<str>, payload: i64) -> i64 {
          leaf_net(s, payload)
        }
        fn leaf_net(s: Secret<str>, payload: i64) -[Net("vault.local")]-> i64 {
          do Net.send(reveal("vault.local", s), payload);
          payload
        }
    "#;
    let leaky_err = store
        .ingest_module(&parse_module(leaky_src).expect("parse ok"))
        .expect_err("pure caller invoking Net callee must fail");
    assert_eq!(leaky_err.code, "E021_UNDECLARED_EFFECT");

    // 3. Transitive capability discovery through 3-hop chain A -> B -> C (with explicit Flux effects)
    let chain_src = r#"
        fn top_handler(tok: Secret<str>, v: i64) -[Net("vault.local"), Fs("/tmp/log")]-> i64 {
          middle_step(tok, v)
        }

        fn middle_step(t: Secret<str>, n: i64) -[Net("vault.local"), Fs("/tmp/log")]-> i64 {
          leaf_network_call(t, n + 1)
        }

        fn leaf_network_call(s: Secret<str>, payload: i64) -[Net("vault.local"), Fs("/tmp/log")]-> i64 {
          do Fs.append(payload);
          do Net.send(reveal("vault.local", s), payload);
          payload
        }

        fn pure_math(x: i64) -> i64 {
          x * x + 1
        }
    "#;
    let chain_mod = parse_module(chain_src).expect("parse ok");
    store.ingest_module(&chain_mod).expect("ingest chain ok");

    let pure_pass = store.compute_passport("pure_math").unwrap();
    assert!(pure_pass.is_pure);
    assert!(!pure_pass.uses_secret_types);
    assert!(pure_pass.capabilities.is_empty());

    let top_pass = store.compute_passport("top_handler").unwrap();
    assert!(!top_pass.is_pure);
    assert!(top_pass.uses_secret_types);
    assert_eq!(top_pass.total_reachable_nodes, 3);
    assert_eq!(top_pass.capabilities.len(), 2);
    assert!(!top_pass.capabilities[0].witness_chain.is_empty());
    assert_eq!(top_pass.secret_pipes.len(), 1);
    assert_eq!(top_pass.secret_pipes[0].witness_chain.len(), 3);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_machine_readable_json_diagnostics_on_syntax_error() {
    let broken = "fn broken(x: i64) { x + 1 }"; // missing `-> RetType`
    let err = parse_module(broken).expect_err("missing return arrow must error");
    assert_eq!(err.code, "E013_EXPECTED_RETURN_ARROW");

    let json_text = err.to_json_pretty();
    let parsed: serde_json::Value =
        serde_json::from_str(&json_text).expect("diagnostic must be valid JSON");
    assert_eq!(parsed["code"], "E013_EXPECTED_RETURN_ARROW");
    assert_eq!(parsed["phase"], "parse");
    assert!(parsed["repair_hint"].as_str().unwrap().contains("->"));
}
