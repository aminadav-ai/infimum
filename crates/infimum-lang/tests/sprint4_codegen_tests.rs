use infimum_lang::codegen::{build_native_binary, run_native_binary};
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
    let dir = std::env::temp_dir().join(format!("infimum_s4_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_native_build_dead_node_pruning_and_immutable_hash_cache() {
    let ws = temp_workspace("build_cache");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        fn sq(x: i64) -> i64
          ens result == x * x
        {
          x * x
        }

        fn sum_of_squares(a: i64, b: i64) -> i64
          ens result >= 0
        {
          sq(a) + sq(b)
        }

        fn unused_bloat_node(z: i64) -[Audit]-> i64 {
          do Audit.log(z);
          z + 999
        }
    "#;

    store.ingest_module(&parse_module(src).unwrap()).unwrap();

    // 1. First build compiles ONLY the 2 reachable nodes (`sum_of_squares` + `sq`), pruning `unused_bloat_node`!
    let art1 = build_native_binary(&store, &ws, "sum_of_squares", None).unwrap();
    assert_eq!(art1.compiled_nodes, 2);
    assert!(!art1.cache_hit);
    assert!(art1.binary_size_bytes > 0);

    // 2. Second build hits content-addressed binary cache instantaneously!
    let art2 = build_native_binary(&store, &ws, "sum_of_squares", None).unwrap();
    assert!(art2.cache_hit);

    // 3. Execute native binary (`3^2 + 4^2 = 25`)
    let run = run_native_binary(&store, &ws, "sum_of_squares", &["3", "4"]).unwrap();
    assert_eq!(run.exit_code, 0);
    assert_eq!(run.stdout.trim(), "25");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_native_execution_trace_secret_redaction_and_boundary_req_guard() {
    let ws = temp_workspace("run_sec");
    let mut store = GraphStore::open(&ws).unwrap();

    let demo = fs::read_to_string("examples/payment_demo.ifm").unwrap();
    store.ingest_module(&parse_module(&demo).unwrap()).unwrap();

    // 1. Valid execution with `--trace`: secret token `sk_super_secret_777` must NEVER appear in stdout!
    let ok_run = run_native_binary(
        &store,
        &ws,
        "checkout",
        &["sk_super_secret_777", "10000", "--trace"],
    )
    .unwrap();
    assert_eq!(ok_run.exit_code, 0);
    assert!(ok_run.stdout.contains("[TRACE #000]"));
    assert!(ok_run.stdout.contains("[PINNED SECRET -> api.mybank.com]"));
    assert!(ok_run.stdout.trim_end().ends_with("true"));
    assert!(
        !ok_run.stdout.contains("sk_super_secret_777"),
        "Raw secret value must never leak to stdout or trace!"
    );

    // 2. Boundary CLI input violating `req total > 100` (passing `50`) is rejected before execution!
    let bad_run = run_native_binary(&store, &ws, "checkout", &["sk_super_secret_777", "50"]).unwrap();
    assert_eq!(bad_run.exit_code, 2);
    assert!(bad_run.stderr.contains("Boundary Precondition Violation"));

    let _ = fs::remove_dir_all(&ws);
}
