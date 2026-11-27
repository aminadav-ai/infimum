// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_lang::eval::run_module_tests;
use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_ws(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_p4_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn run_wasm_i64_export_via_node(ifw_path: &Path, export_name: &str, args: &[i64]) -> i64 {
    let args_js = args
        .iter()
        .map(|a| format!("{a}n"))
        .collect::<Vec<_>>()
        .join(", ");
    let script = format!(
        r#"
        const fs = require('fs');
        const bytes = fs.readFileSync({path:?});
        WebAssembly.instantiate(bytes, {{}}).then(({{ instance }}) => {{
            const fn = instance.exports[{name:?}];
            if (typeof fn !== 'function') {{
                throw new Error('Missing export ' + {name:?} + '; exports=' + Object.keys(instance.exports).join(','));
            }}
            const res = fn({args_js});
            process.stdout.write(res.toString());
        }}).catch(err => {{
            console.error(err);
            process.exit(1);
        }});
        "#,
        path = ifw_path.to_str().unwrap(),
        name = export_name,
        args_js = args_js,
    );
    let out = Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
        .expect("node must be available to verify compiled Wasm");
    assert!(
        out.status.success(),
        "node Wasm execution failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    String::from_utf8_lossy(&out.stdout)
        .trim()
        .parse::<i64>()
        .expect("node stdout must be i64")
}

#[test]
fn option_across_scalars_dec64_str_struct_and_slice_eval_and_wasm() {
    let ws = temp_ws("option_all");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        struct Drone {
          id: word,
          hp: word,
          price: dec64,
        }

        layout Buf {
          raw: bytes[16],
        }

        fn find_int(flag: i64) -> Option<i64> {
          if flag != 0 { Some(42) } else { None }
        }

        fn find_price(flag: i64) -> Option<dec64> {
          if flag != 0 { Some(19.99d) } else { None }
        }

        fn find_label(flag: i64) -> Option<str> {
          if flag != 0 { Some("hello") } else { None }
        }

        fn find_drone(flag: i64) -> Option<Drone> {
          if flag != 0 {
            Some(Drone { id: 7, hp: 95, price: 250.50d })
          } else {
            None
          }
        }

        fn find_buf(flag: i64) -[Mem]-> Option<[u8]> {
          Buf.raw[0] = 65;
          Buf.raw[1] = 66;
          if flag != 0 { Some(Buf.raw[0..2]) } else { None }
        }

        export fn run_option_demo(flag: i64) -[Mem]-> i64 {
          let oi = find_int(flag);
          let vi = oi.unwrap_or(10);
          let has_i = if oi.is_some() && !oi.is_none() { 1 } else { 0 };

          let od = find_price(flag);
          let vd = match od {
            Some(p) => dec64_raw(p),
            None => 0,
          };

          let os = find_label(flag);
          let vs = match os {
            Some(s) => s.len,
            None => 0,
          };

          let odr = find_drone(flag);
          let vdr = match odr {
            Some(d) => d.id + d.hp + dec64_to_int(d.price),
            None => 0,
          };

          let ob = find_buf(flag);
          let vb = match ob {
            Some(b) => if b.len >= 2 { b[0] + b[1] } else { 0 },
            None => 0,
          };

          vi + has_i + vd + vs + vdr + vb
        }

        test "Option<T> works for i64, dec64, str, struct, and [u8]" {
          // flag = 1: 42 + 1 + 199900 + 5 + (7+95+250=352) + (65+66=131) = 200431
          assert(run_option_demo(1) == 200431);
          // flag = 0: 10 + 0 + 0 + 0 + 0 + 0 = 10
          assert(run_option_demo(0) == 10);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("option_all.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_option_demo_i64", &[1]),
        200431
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_option_demo_i64", &[0]),
        10
    );
}

#[test]
fn result_try_operator_chain_propagates_early_errors_eval_and_wasm() {
    let ws = temp_ws("result_try");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        const ERR_AMOUNT: i64 = 101;
        const ERR_RATE: i64 = 102;
        const ERR_FEE: i64 = 103;

        fn parse_amount(code: i64) -> Result<dec64, i64>
          ens result.is_err() || (match result { Ok(v) => v >= 0.0d && v <= 1000000.0d, Err(_) => false })
        {
          if code <= 0 { Err(ERR_AMOUNT) } else { Ok(100.00d) }
        }

        fn parse_rate(code: i64) -> Result<dec64, i64>
          ens result.is_err() || (match result { Ok(v) => v >= 0.0d && v <= 1.0d, Err(_) => false })
        {
          if code <= 0 { Err(ERR_RATE) } else { Ok(0.15d) }
        }

        fn parse_fee(code: i64) -> Result<dec64, i64>
          ens result.is_err() || (match result { Ok(v) => v >= 0.0d && v <= 100.0d, Err(_) => false })
        {
          if code <= 0 { Err(ERR_FEE) } else { Ok(2.50d) }
        }

        fn compute_total(a_ok: i64, r_ok: i64, f_ok: i64) -> Result<dec64, i64> {
          let amount = parse_amount(a_ok)?;
          let rate = parse_rate(r_ok)?;
          let fee = parse_fee(f_ok)?;
          if amount >= 0.0d && amount <= 1000000.0d
            && rate >= 0.0d && rate <= 1.0d
            && fee >= 0.0d && fee <= 100.0d
          {
            Ok(amount + (amount * rate) + fee)
          } else {
            Err(999)
          }
        }

        export fn run_try_chain(a_ok: i64, r_ok: i64, f_ok: i64) -> i64 {
          let r = compute_total(a_ok, r_ok, f_ok);
          match r {
            Ok(v) => dec64_raw(v),
            Err(code) => -code,
          }
        }

        test "chain of 3 ? operators stops at 1st, 2nd, 3rd error or succeeds" {
          // 1st error
          assert(run_try_chain(0, 1, 1) == -101);
          // 2nd error
          assert(run_try_chain(1, 0, 1) == -102);
          // 3rd error
          assert(run_try_chain(1, 1, 0) == -103);
          // Success: 100.00 + 15.00 + 2.50 = 117.50 -> raw 1_175_000
          assert(run_try_chain(1, 1, 1) == 1175000);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("result_try.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_try_chain_i64", &[0, 1, 1]),
        -101
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_try_chain_i64", &[1, 0, 1]),
        -102
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_try_chain_i64", &[1, 1, 0]),
        -103
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_try_chain_i64", &[1, 1, 1]),
        1_175_000
    );
}

#[test]
fn non_exhaustive_tagged_match_is_rejected_with_e079() {
    let bad_opt = r#"
        fn bad(x: Option<i64>) -> i64 {
          match x {
            Some(v) => v,
          }
        }
    "#;
    let err1 = parse_module(bad_opt).expect_err("must reject missing None arm");
    assert_eq!(err1.code, "E079_NON_EXHAUSTIVE_MATCH", "unexpected: {err1:?}");

    let bad_res = r#"
        fn bad(r: Result<i64, i64>) -> i64 {
          match r {
            Ok(v) => v,
          }
        }
    "#;
    let err2 = parse_module(bad_res).expect_err("must reject missing Err arm");
    assert_eq!(err2.code, "E079_NON_EXHAUSTIVE_MATCH", "unexpected: {err2:?}");
}

#[test]
fn try_type_mismatch_bad_result_err_and_uninferrable_none_rejected() {
    // 1) `?` in function returning `i64` -> E080_TRY_TYPE_MISMATCH
    let bad_try = r#"
        fn helper() -> Result<i64, i64> { Ok(1) }
        fn bad() -> i64 {
          let x = helper()?;
          x
        }
    "#;
    let err1 = parse_module(bad_try).expect_err("must reject ? in non-Result fn");
    assert_eq!(err1.code, "E080_TRY_TYPE_MISMATCH", "unexpected: {err1:?}");

    // 2) `Result<i64, bool>` -> E077_RESULT_ERR_MUST_BE_I64
    let bad_err_ty = r#"
        fn bad() -> Result<i64, bool> { Ok(1) }
    "#;
    let err2 = parse_module(bad_err_ty).expect_err("must reject non-i64 Result err");
    assert_eq!(err2.code, "E077_RESULT_ERR_MUST_BE_I64", "unexpected: {err2:?}");

    // 3) Uninferrable `None` -> E078_CANNOT_INFER_TAGGED_TYPE
    let bad_none = r#"
        fn bad() -> i64 {
          let x = None;
          0
        }
    "#;
    let err3 = parse_module(bad_none).expect_err("must reject uninferrable None");
    assert_eq!(
        err3.code, "E078_CANNOT_INFER_TAGGED_TYPE",
        "unexpected: {err3:?}"
    );
}

#[test]
fn z3_proves_tagged_correlation_for_option_slice_and_safe_div() {
    let ws = temp_ws("z3_corr");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        layout Store {
          data: bytes[8],
        }

        pub fn get_nonempty(flag: i64) -[Mem]-> Option<[u8]>
          ens match result { Some(s) => s.len > 0, None => true }
        {
          Store.data[0] = 99;
          if flag != 0 {
            Some(Store.data[0..4])
          } else {
            None
          }
        }

        // Z3 must prove `s[0]` is in-bounds purely from `get_nonempty`'s `ens`!
        pub fn first_byte_or_zero(flag: i64) -[Mem]-> i64 {
          match get_nonempty(flag) {
            Some(s) => s[0],
            None => 0,
          }
        }

        pub fn safe_div(a: dec64, b: dec64) -> Result<dec64, i64>
          req a >= -1000000.0d && a <= 1000000.0d
          req b >= -1000000.0d && b <= 1000000.0d
          ens result.is_ok() || b == 0.0d
        {
          if b == 0.0d {
            Err(1)
          } else if b >= 0.01d || b <= -0.01d {
            Ok(a / b)
          } else {
            Ok(0.0d)
          }
        }

        test "Z3-verified Option<[u8]> and safe_div execute cleanly" {
          assert(first_byte_or_zero(1) == 99);
          assert(first_byte_or_zero(0) == 0);
          assert(safe_div(10.0d, 2.0d).is_ok());
          assert(safe_div(10.0d, 0.0d).is_err());
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    build_strata_wasm_from_module(&mut store, &module, &ws.join("z3_corr.ifw"))
        .expect("Z3 must prove tagged correlation for Option<[u8]> and safe_div");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);
}

#[test]
fn export_fn_with_option_or_result_rejected_with_e082() {
    let ws = temp_ws("export_reject");
    let mut store = GraphStore::open(&ws).unwrap();

    let bad_ret = r#"
        export fn bad_opt() -> Option<i64> {
          Some(1)
        }
    "#;
    let m1 = parse_module(bad_ret).expect("parse ok");
    let e1 = build_strata_wasm_from_module(&mut store, &m1, &ws.join("bad1.ifw"))
        .expect_err("must reject export fn returning Option<i64>");
    assert_eq!(e1.code, "E082_NON_SCALAR_EXPORT", "unexpected: {e1:?}");

    let bad_param = r#"
        export fn bad_res(r: Result<i64, i64>) -> i64 {
          r.unwrap_or(0)
        }
    "#;
    let m2 = parse_module(bad_param).expect("parse ok");
    let e2 = build_strata_wasm_from_module(&mut store, &m2, &ws.join("bad2.ifw"))
        .expect_err("must reject export fn taking Result<i64, i64>");
    assert_eq!(e2.code, "E082_NON_SCALAR_EXPORT", "unexpected: {e2:?}");
}
