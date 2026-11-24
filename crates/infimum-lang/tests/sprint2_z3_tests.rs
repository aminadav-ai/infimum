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
    let dir = std::env::temp_dir().join(format!("infimum_s2_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_z3_rejects_ai_stub_with_exact_counterexample_and_blocks_store_commit() {
    let ws = temp_workspace("stub");
    let stub_src = r#"
        fn transfer_stub(balance: i64, amount: i64) -> i64
          req amount > 0 && balance >= amount
          ens result == balance - amount
        {
          // Classic lazy AI stub: returns unchanged balance or 0
          balance
        }
    "#;

    let module = parse_module(stub_src).expect("parse ok");
    let mut store = GraphStore::open(&ws).expect("store open");

    let err = store
        .ingest_module(&module)
        .expect_err("Z3 must reject stub implementation");
    assert_eq!(err.code, "E050_CONTRACT_VIOLATION");
    assert!(err.counterexample.contains_key("balance"));
    assert!(err.counterexample.contains_key("amount"));
    assert!(err.counterexample.contains_key("result"));

    // Verify the counterexample values actually satisfy `req` and violate `ens`!
    let bal: i64 = err.counterexample["balance"].parse().unwrap();
    let amt: i64 = err.counterexample["amount"].parse().unwrap();
    let res: i64 = err.counterexample["result"].parse().unwrap();
    assert!(amt > 0 && bal >= amt);
    assert_ne!(res, bal - amt);

    // Ensure rejected node was NEVER written to .infimum/nodes/
    assert!(store.list_graph().unwrap().is_empty());

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_z3_proves_honest_branching_and_inductive_recursion() {
    let ws = temp_workspace("honest");
    let src = r#"
        fn abs_val(x: i64) -> i64
          ens result >= 0
          ens (x >= 0 && result == x) || (x < 0 && result == -x)
        {
          if x < 0 { -x } else { x }
        }

        fn clamp(val: i64, lo: i64, hi: i64) -> i64
          req lo <= hi
          ens result >= lo && result <= hi
        {
          if val < lo {
            lo
          } else if val > hi {
            hi
          } else {
            val
          }
        }

        fn factorial(n: i64) -> i64
          req n >= 0
          ens result >= 1
        {
          if n <= 1 { 1 } else { n * factorial(n - 1) }
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let mut store = GraphStore::open(&ws).expect("store open");
    let ingested = store.ingest_module(&module).expect("Z3 must prove all 3");
    assert_eq!(ingested.len(), 3);
    assert_eq!(ingested[0].obligations_proved, 2);
    assert_eq!(ingested[1].obligations_proved, 1);
    // factorial checks: 1 recursive call-site precondition (`n - 1 >= 0`) + 1 postcondition (`result >= 1`)
    assert_eq!(ingested[2].obligations_proved, 2);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_z3_catches_vacuous_precondition_reward_hacking() {
    let ws = temp_workspace("vacuous");
    let cheat_src = r#"
        fnimpossible_req(x: i64) -> i64
          req x > 100 && x < 50
          ens result == 999999
        {
          0
        }
    "#;
    // Fix space after fn
    let cheat_src = cheat_src.replace("fnimpossible_req", "fn impossible_req");

    let module = parse_module(&cheat_src).expect("parse ok");
    let store = GraphStore::open(&ws).expect("store open");
    let err = store
        .verify_module(&module)
        .expect_err("contradictory req must be rejected");
    assert_eq!(err.code, "E052_VACUOUS_PRECONDITION");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_z3_catches_modular_call_precondition_violation_and_div_by_zero() {
    let ws = temp_workspace("modular");

    let div_src = r#"
        fn unsafe_ratio(a: i64, b: i64) -> i64 {
          a / b
        }
    "#;
    let div_mod = parse_module(div_src).unwrap();
    let store = GraphStore::open(&ws).unwrap();
    let div_err = store
        .verify_module(&div_mod)
        .expect_err("unguarded division must fail");
    assert_eq!(div_err.code, "E053_POSSIBLE_DIV_BY_ZERO");
    assert_eq!(div_err.counterexample.get("b").map(String::as_str), Some("0"));

    let call_src = r#"
        fn strict_withdraw(bal: i64, amt: i64) -> i64
          req amt > 0 && bal >= amt
          ens result == bal - amt
        {
          bal - amt
        }

        fn careless_caller(bal: i64) -> i64
          req bal >= 0
        {
          // Bug: when bal == 5, bal < 10 violates strict_withdraw's `bal >= amt`!
          strict_withdraw(bal, 10)
        }
    "#;
    let call_mod = parse_module(call_src).unwrap();
    let call_err = store
        .verify_module(&call_mod)
        .expect_err("must catch invalid call arguments");
    assert_eq!(call_err.code, "E051_CALL_PRECONDITION_VIOLATED");
    let cex_bal: i64 = call_err.counterexample["bal"].parse().unwrap();
    assert!((0..10).contains(&cex_bal));

    let _ = fs::remove_dir_all(&ws);
}
