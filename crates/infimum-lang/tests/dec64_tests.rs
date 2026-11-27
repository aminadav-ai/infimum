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
    let dir = std::env::temp_dir().join(format!("infimum_p3_{tag}_{nanos}"));
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
fn dec64_exactness_and_lexer_precision_rejection() {
    let ws = temp_ws("exactness");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        export fn test_exact() -> i64 {
          let a: dec64 = 0.10d + 0.20d;
          let b: dec64 = 19.99d * 0.0825d;
          let c: dec64 = -10.50d / 2d;
          let d: dec64 = dec64(42) - 0.0001d;
          if a == 0.30d && b == 1.6491d && c == -5.25d && d == 41.9999d && dec64_to_int(d) == 41 {
            1
          } else {
            0
          }
        }

        test "exact decimal arithmetic has zero IEEE 754 drift" {
          assert(test_exact() == 1);
          assert(dec64_raw(0.10d + 0.20d) == 3000);
          assert(dec64_raw(19.99d * 0.0825d) == 16491);
          assert(dec64_raw(-10.50d / 2d) == -52500);
          assert(dec64_from_cents(1999) == 19.99d);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("exactness.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);
    assert_eq!(run_wasm_i64_export_via_node(&ifw_path, "test_exact_i64", &[]), 1);

    // Lexer must reject > 4 fractional digits with E003_DEC64_PRECISION
    let bad_src = r#"
        fn bad() -> dec64 {
          1.12345d
        }
    "#;
    let err = parse_module(bad_src).expect_err("must reject >4 decimal places");
    assert_eq!(err.code, "E003_DEC64_PRECISION", "unexpected diag: {err:?}");
}

#[test]
fn dec64_large_trillion_amounts_match_between_eval_and_wasm() {
    let ws = temp_ws("large_amounts");
    let mut store = GraphStore::open(&ws).unwrap();

    // $50,000,000,000.00 * 0.0725 = $3,625,000,000.0000 (raw = 36_250_000_000_0000)
    // Raw product 500_000_000_000_000 * 725 = 3.625e17; test even larger:
    // $50,000,000,000.00 * 1234.5678d -> direct raw*raw = 5e14 * 1.2345678e7 = 6.17e21 (overflows i64!)
    // but (a*b)/10000 = 617_283_900_000_000_000 fits easily in i64!
    let src = r#"
        export fn mul_billions() -> dec64 {
          let a: dec64 = 50000000000.00d;
          let rate: dec64 = 0.0725d;
          a * rate
        }

        export fn mul_overflow_free_128bit() -> dec64 {
          let a: dec64 = 50000000000.00d;
          let factor: dec64 = 1234.5678d;
          a * factor
        }

        export fn mul_neg_overflow_free() -> dec64 {
          let a: dec64 = -50000000000.25d;
          let factor: dec64 = 1234.5678d;
          a * factor
        }

        export fn div_large() -> dec64 {
          let a: dec64 = 50000000000.00d;
          a / 3d
        }

        export fn div_neg_large() -> dec64 {
          let a: dec64 = -50000000000.00d;
          a / 3d
        }

        test "large financial amounts compute without intermediate i64 overflow" {
          assert(dec64_raw(mul_billions()) == 36250000000000);
          // 50_000_000_000 * 1234.5678 = 61_728_390_000_000.0000 -> raw = 617_283_900_000_000_000
          assert(dec64_raw(mul_overflow_free_128bit()) == 617283900000000000);
          // -50_000_000_000.25 * 1234.5678 = -61_728_390_000_308.64195 -> trunc = -617_283_900_003_086_419
          assert(dec64_raw(mul_neg_overflow_free()) == -617283900003086419);
          // 50_000_000_000 / 3 = 16_666_666_666.6666 -> raw = 166_666_666_666_666
          assert(dec64_raw(div_large()) == 166666666666666);
          assert(dec64_raw(div_neg_large()) == -166666666666666);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("large_amounts.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    // Verify bit-for-bit agreement with Wasm 128-bit decomposition in Node.js!
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "mul_billions_i64", &[]),
        36_250_000_000_000
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "mul_overflow_free_128bit_i64", &[]),
        617_283_900_000_000_000
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "mul_neg_overflow_free_i64", &[]),
        -617_283_900_003_086_419
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "div_large_i64", &[]),
        166_666_666_666_666
    );
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "div_neg_large_i64", &[]),
        -166_666_666_666_666
    );
}

#[test]
fn dec64_full_64bit_storage_in_layout_struct_vec_and_slice() {
    let ws = temp_ws("storage64");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        struct InvoiceLine {
          id: word,
          amount: dec64,
          qty: word,
        }

        layout Ledger {
          total: dec64,
          prices: dec64[4],
          history: vec<dec64>[16],
          lines: InvoiceLine[4],
        }

        fn sum_dec_slice(s: [dec64]) -[Mem]-> dec64 {
          let mut acc: dec64 = 0.0d;
          for i in 0..s.len max 16 {
            let v = s[i];
            if dec64_raw(v) >= -1000000000000000 && dec64_raw(v) <= 1000000000000000
              && dec64_raw(acc) >= -10000000000000000 && dec64_raw(acc) <= 10000000000000000
            {
              acc = acc + v;
            }
          }
          acc
        }

        export fn run_storage64() -[Mem]-> dec64 {
          // 5_000_000_000.1234d has raw = 50_000_000_001_234 (far above i32::MAX = 2.14e9!)
          let big: dec64 = 5000000000.1234d;
          Ledger.total = big;
          Ledger.prices[0] = big;
          Ledger.prices[1] = 1000000000.5678d;
          Ledger.history.clear();
          let _ = Ledger.history.try_push(big);
          let _ = Ledger.history.try_push(-2500000000.0001d);
          let line = InvoiceLine { id: 7, amount: big, qty: 3 };
          Ledger.lines[0] = line;
          let read_line = Ledger.lines[0];
          let h_slice = Ledger.history.as_slice();
          let h_sum = sum_dec_slice(h_slice);
          if Ledger.total == big
            && Ledger.prices[0] == big
            && Ledger.prices[1] == 1000000000.5678d
            && read_line.id == 7
            && read_line.amount == big
            && read_line.qty == 3
            && h_sum == 2500000000.1233d
          {
            Ledger.total
          } else {
            0.0d
          }
        }

        test "64-bit dec64 survives layout, struct, vec, and slice without i32 truncation" {
          assert(dec64_raw(run_storage64()) == 50000000001234);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("storage64.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    // Verify in Node.js Wasm that i64.load / i64.store preserves all 64 bits!
    assert_eq!(
        run_wasm_i64_export_via_node(&ifw_path, "run_storage64_i64", &[]),
        50_000_000_001_234
    );
}

#[test]
fn z3_proves_discount_contract_and_rejects_dec64_div_by_zero() {
    let ws = temp_ws("z3_discount");
    let mut store = GraphStore::open(&ws).unwrap();

    let ok_src = r#"
        pub fn calc_discount(amount: dec64, rate: dec64) -> dec64
          req amount >= 0.0d && amount <= 1000000000.0d
          req rate >= 0.0d && rate <= 0.50d
          ens result >= 0.0d && result <= amount
        {
          amount - (amount * rate)
        }

        test "discount verified" {
          assert(calc_discount(100.00d, 0.20d) == 80.00d);
          assert(calc_discount(19.99d, 0.10d) == 17.9910d);
        }
    "#;
    let module = parse_module(ok_src).expect("parse ok");
    build_strata_wasm_from_module(&mut store, &module, &ws.join("discount.ifw"))
        .expect("Z3 must prove discount contract");
    let results = run_module_tests(&module);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    // Division by potentially zero dec64 must be rejected with E053_POSSIBLE_DIV_BY_ZERO
    let bad_div = r#"
        pub fn unsafe_unit_price(total: dec64, qty: dec64) -> dec64
          req total >= 0.0d && total <= 1000000.0d
          req qty >= 0.0d && qty <= 1000.0d
        {
          total / qty
        }
    "#;
    let bad_mod = parse_module(bad_div).expect("parse ok");
    let err = build_strata_wasm_from_module(&mut store, &bad_mod, &ws.join("bad_div.ifw"))
        .expect_err("Z3 must reject possible division by zero");
    assert_eq!(err.code, "E053_POSSIBLE_DIV_BY_ZERO", "unexpected diag: {err:?}");
}

#[test]
fn z3_rejects_dec64_field_invariant_violation_and_overflow() {
    let ws = temp_ws("z3_inv_overflow");
    let mut store = GraphStore::open(&ws).unwrap();

    // 1) Layout field invariant violation: inv 0.0d..=10000.0d
    let bad_inv = r#"
        layout Account {
          balance: dec64 inv 0.0d..=10000.0d,
        }

        pub fn deposit_unchecked(x: dec64) -[Mem]-> unit
          req x >= 0.0d && x <= 20000.0d
        {
          Account.balance = x;
        }
    "#;
    let bad_inv_mod = parse_module(bad_inv).expect("parse ok");
    let err1 = build_strata_wasm_from_module(&mut store, &bad_inv_mod, &ws.join("bad_inv.ifw"))
        .expect_err("Z3 must reject invariant violation");
    assert_eq!(
        err1.code, "E054_FIELD_INVARIANT_VIOLATED",
        "unexpected diag: {err1:?}"
    );

    // 2) Unbounded dec64 addition must be rejected with E057_DEC64_OVERFLOW
    let bad_overflow = r#"
        pub fn unbounded_add(a: dec64, b: dec64) -> dec64 {
          a + b
        }
    "#;
    let bad_ov_mod = parse_module(bad_overflow).expect("parse ok");
    let err2 = build_strata_wasm_from_module(&mut store, &bad_ov_mod, &ws.join("bad_ov.ifw"))
        .expect_err("Z3 must reject unbounded dec64 overflow");
    assert_eq!(
        err2.code, "E057_DEC64_OVERFLOW",
        "unexpected diag: {err2:?}"
    );
}

#[test]
fn std_dec_rounding_format_and_parse_verify_and_run() {
    let ws = temp_ws("std_dec");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        use "std/dec.ifm";
        use "std/slice.ifm";

        layout FmtBuf {
          out: bytes[64],
        }

        fn fmt_and_check(v: dec64, expected: str) -[Mem]-> bool {
          let dst = FmtBuf.out[..];
          let n = dec64_format(v, dst);
          if n <= 0 || n > dst.len {
            false
          } else {
            bytes_eq(dst[0..n], expected.as_bytes())
          }
        }

        test "std/dec rounding, banker's rounding, format, and parse" {
          // Basic helpers
          assert(dec64_abs(-12.34d) == 12.34d);
          assert(dec64_min(1.50d, 2.50d) == 1.50d);
          assert(dec64_max(1.50d, 2.50d) == 2.50d);
          assert(dec64_clamp(15.0d, 0.0d, 10.0d) == 10.0d);

          // Half-away-from-zero cent rounding
          assert(dec64_round_cents(1.0050d) == 1.01d);
          assert(dec64_round_cents(-1.0050d) == -1.01d);
          assert(dec64_round_cents(1.0049d) == 1.00d);
          assert(dec64_to_cents(19.9950d) == 2000);
          assert(dec64_to_cents(19.9949d) == 1999);

          // Banker's rounding (half-to-even)
          assert(dec64_round_half_even_cents(0.0050d) == 0.00d);
          assert(dec64_round_half_even_cents(0.0150d) == 0.02d);
          assert(dec64_round_half_even_cents(0.0250d) == 0.02d);
          assert(dec64_round_half_even_cents(-0.0150d) == -0.02d);
          assert(dec64_round_half_even_cents(-0.0250d) == -0.02d);

          // Formatting
          assert(fmt_and_check(0.0d, "0.0000"));
          assert(fmt_and_check(-12345.6789d, "-12345.6789"));
          assert(fmt_and_check(19.99d, "19.9900"));
          assert(fmt_and_check(-0.0001d, "-0.0001"));

          // Parsing (returns Result<dec64, i64>)
          let r0 = dec64_parse("0.0000".as_bytes());
          assert(r0.is_ok() && r0.unwrap_or(1.0d) == 0.0d);
          let r1 = dec64_parse("-12345.6789".as_bytes());
          assert(r1.is_ok() && r1.unwrap_or(0.0d) == -12345.6789d);
          let r2 = dec64_parse("19.99".as_bytes());
          assert(r2.is_ok() && r2.unwrap_or(0.0d) == 19.99d);
          let r3 = dec64_parse("100".as_bytes());
          assert(r3.is_ok() && r3.unwrap_or(0.0d) == 100d);
          let r_bad1 = dec64_parse("1.12345".as_bytes());
          assert(r_bad1.is_err());
          let r_bad2 = dec64_parse("abc".as_bytes());
          assert(r_bad2.is_err());
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("std_dec.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "std/dec test failed: {:?}", results[0]);
}

#[test]
fn dec64_div_large_operands_exact_no_i64_wraparound_in_wasm() {
    let ws = temp_ws("dec64_div_large");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        export fn div_safe(a: dec64, b: dec64) -> dec64
          req dec64_raw(a) >= -8999999999999990000 && dec64_raw(a) <= 8999999999999990000
          req dec64_raw(b) >= 10000 || dec64_raw(b) <= -10000
          req dec64_raw(b) >= -9000000000000000000 && dec64_raw(b) <= 9000000000000000000
        {
          a / b
        }

        test "dec64 division matches i128 on 10^17 / 2*10^17 and all sign combinations" {
          let a = dec64_from_raw(100000000000000000);
          let b = dec64_from_raw(200000000000000000);
          assert(dec64_raw(div_safe(a, b)) == 5000);
          assert(dec64_raw(div_safe(-a, b)) == -5000);
          assert(dec64_raw(div_safe(a, -b)) == -5000);
          assert(dec64_raw(div_safe(-a, -b)) == 5000);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("dec64_div_large.ifw");
    build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");

    // 1. Interpreter (i128 reference)
    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);

    // 2. Real Wasm execution via Node.js — must match i128 bit-for-bit on ALL edge cases!
    let cases: &[(i64, i64)] = &[
        (100_000_000_000_000_000, 200_000_000_000_000_000),   // 0.5000d (previously wrapped to -0.4223d!)
        (-100_000_000_000_000_000, 200_000_000_000_000_000),  // -0.5000d
        (100_000_000_000_000_000, -200_000_000_000_000_000),  // -0.5000d
        (-100_000_000_000_000_000, -200_000_000_000_000_000), // +0.5000d
        (8_999_999_999_999_990_000, 9_000_000_000_000_000_000), // near 9e18 limit -> 0.9999d
        (1_000_000_000_000_000_000, 3_000_000_000_000_000_000), // 1/3 -> 0.3333d
        (-7_777_777_777_777_777_777, 1_111_111_111_111_111_111), // -7.0000d
        (123_456_789_012_345_678, 987_654_321_098_765_432),
    ];
    for &(a, b) in cases {
        let expected = ((a as i128 * 10_000) / (b as i128)) as i64;
        let actual = run_wasm_i64_export_via_node(&ifw_path, "div_safe_i64", &[a, b]);
        assert_eq!(
            actual, expected,
            "Wasm DecDiv mismatch for a={a}, b={b}: got {actual}, expected {expected}"
        );
    }
}
