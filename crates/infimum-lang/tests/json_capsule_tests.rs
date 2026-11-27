// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_lang::eval::run_module_tests;
use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_ws(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_p5_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn std_json_v2_reader_int_dec64_bool_str_unicode_nested_and_array() {
    let ws = temp_ws("json_reader");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        use "std/json.ifm";
        use "std/slice.ifm";

        layout DecBuf {
          out: bytes[128],
        }

        fn sum_array(arr: [u8]) -[Mem]-> i64 {
          let mut cur = 0;
          let mut total = 0;
          for _step in 0..16 {
            match json_array_iter_next(arr, cur) {
              Some(el) => {
                if el.start >= 0 && el.start < el.end && el.end <= arr.len {
                  let sub = arr[el.start..el.end];
                  match json_get_int(sub, "v") {
                    Ok(n) => { total += n; }
                    Err(_) => {}
                  }
                }
                cur = el.next;
              }
              None => { break; }
            }
          }
          total
        }

        test "std/json v2 reads int, dec64, bool, UTF-8/\\uXXXX str, nested obj, and array" {
          let doc = "{\"count\":-42,\"price\":19.99,\"neg\":-0.0725,\"whole\":100,\"ok\":true,\"no\":false,\"curr\":\"USD\",\"nested\":{\"rate\":0.15},\"items\":[{\"v\":10},{\"v\":25},{\"v\":7}]}".as_bytes();
          assert(json_get_int(doc, "count").unwrap_or(0) == -42);
          assert(json_get_dec64(doc, "price").unwrap_or(0.0d) == 19.99d);
          assert(json_get_dec64(doc, "neg").unwrap_or(0.0d) == -0.0725d);
          assert(json_get_dec64(doc, "whole").unwrap_or(0.0d) == 100.0d);
          assert(json_get_bool(doc, "ok").unwrap_or(false));
          assert(!json_get_bool(doc, "no").unwrap_or(true));
          let curr = json_get_str_raw(doc, "curr").unwrap_or("");
          assert(bytes_eq(curr.as_bytes(), "USD".as_bytes()));
          assert(json_get_int(doc, "missing").is_err());

          // Nested object
          let nested_ok = match json_get_object(doc, "nested") {
            Ok(obj) => json_get_dec64(obj, "rate").unwrap_or(0.0d) == 0.15d,
            Err(_) => false,
          };
          assert(nested_ok);

          // Array iteration
          let arr_sum = match json_get_array(doc, "items") {
            Ok(arr) => sum_array(arr),
            Err(_) => -1,
          };
          assert(arr_sum == 42);

          // Unicode \u041F ('П') + Cyrillic + \" escape decoding
          let esc_doc = "{\"msg\":\"\\u041Fривет, \\\"мир\\\"!\"}".as_bytes();
          let pos = json_find_key(esc_doc, 0, "msg").unwrap_or(-1);
          assert(pos >= 0);
          let dst = DecBuf.out[..];
          let n = json_decode_str(esc_doc, pos, dst).unwrap_or(0);
          let dec_ok = if n > 0 && n <= dst.len {
            bytes_eq(dst[0..n], "Привет, \"мир\"!".as_bytes())
          } else {
            false
          };
          assert(dec_ok);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    build_strata_wasm_from_module(&mut store, &module, &ws.join("json_reader.ifw"))
        .expect("verify+wasm ok");
    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);
}

#[test]
fn json_writer_escaping_and_buffer_full_verified_by_z3() {
    let ws = temp_ws("json_writer");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        use "std/json.ifm";
        use "std/slice.ifm";

        layout OutBuf {
          big: bytes[256],
          tiny: bytes[12],
        }

        test "JsonWriter escapes quotes/slashes/newlines/tabs and detects BufferFull" {
          let dst = OutBuf.big[..];
          let mut w = jw_begin_obj(dst);
          w = jw_kv_int(dst, w, "id", -7);
          w = jw_kv_dec64(dst, w, "tax", 1.6491d);
          w = jw_kv_bool(dst, w, "ok", true);
          w = jw_kv_str(dst, w, "msg", "a\"b\\c\nd\te");
          let len = jw_end_obj(dst, w).unwrap_or(0);
          let expected = "{\"id\":-7,\"tax\":1.6491,\"ok\":true,\"msg\":\"a\\\"b\\\\c\\nd\\te\"}".as_bytes();
          let eq_ok = if len > 0 && len <= dst.len {
            bytes_eq(dst[0..len], expected)
          } else {
            false
          };
          assert(eq_ok);

          // Overflow tiny buffer -> must return Err(JsonErr::BufferFull) safely!
          let small = OutBuf.tiny[..];
          let mut ws = jw_begin_obj(small);
          ws = jw_kv_str(small, ws, "long_key", "long_value_that_exceeds_12_bytes");
          let err_code = match jw_end_obj(small, ws) {
            Ok(_) => 0,
            Err(c) => c,
          };
          assert(err_code == JsonErr::BufferFull);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    build_strata_wasm_from_module(&mut store, &module, &ws.join("json_writer.ifw"))
        .expect("verify+wasm ok");
    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "eval test failed: {:?}", results[0]);
}

#[test]
fn headless_b2b_capsule_auto_exports_size_ram_and_wasm_roundtrip() {
    let ws = temp_ws("b2b_capsule");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        use "std/capsule.ifm";
        use "std/json.ifm";

        fn handle(input: [u8], out: [u8]) -[Mem]-> Result<i64, i64> {
          let amount = json_get_dec64(input, "amount")?;
          let rate = json_get_dec64(input, "tax_rate")?;
          let curr = json_get_str_raw(input, "currency")?;
          if amount < 0.0d || amount > 1000000000.0d || rate < 0.0d || rate > 1.0d {
            Err(400)
          } else {
            let tax = amount * rate;
            let total = amount + tax;
            let mut w = jw_begin_obj(out);
            w = jw_kv_dec64(out, w, "tax", tax);
            w = jw_kv_dec64(out, w, "total", total);
            w = jw_kv_str(out, w, "currency", curr);
            jw_end_obj(out, w)
          }
        }

        export fn cap_execute(in_len: i64) -[Mem]-> i64 {
          let input = cap_input(in_len);
          let out = cap_output_buf();
          cap_finish(handle(input, out))
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("pricing_capsule.ifw");
    let art = build_strata_wasm_from_module(&mut store, &module, &ifw_path)
        .expect("verify+wasm ok");

    // 1) All 7 auto-generated capsule ABI exports present
    for exp in [
        "cap_execute",
        "ifw_capsule_abi_version",
        "ifw_cap_input_byte_offset",
        "ifw_cap_input_cap_bytes",
        "ifw_cap_output_byte_offset",
        "ifw_cap_output_cap_bytes",
        "ifw_cap_secret_byte_offset",
        "ifw_cap_secret_cap_bytes",
    ] {
        assert!(
            art.exported_functions.iter().any(|e| e == exp),
            "Missing auto-export '{exp}' in {:?}",
            art.exported_functions
        );
    }

    // 2) Compact binary (< 25 KB)
    assert!(
        art.wasm_size_bytes < 25 * 1024,
        "Expected compact B2B capsule < 25 KB, got {} bytes",
        art.wasm_size_bytes
    );

    // 3) Execute end-to-end in Node.js WebAssembly: write JSON -> cap_execute -> read JSON + check 512 KB RAM!
    let script = format!(
        r#"
        const fs = require('fs');
        const bytes = fs.readFileSync({path:?});
        WebAssembly.instantiate(bytes, {{}}).then(({{ instance }}) => {{
            const ex = instance.exports;
            if (ex.ifw_capsule_abi_version() !== 1) throw new Error('bad abi version');
            const memBytes = ex.memory.buffer.byteLength;
            if (memBytes !== 524288) throw new Error('expected 512 KiB RAM, got ' + memBytes);
            const inOff = ex.ifw_cap_input_byte_offset();
            const outOff = ex.ifw_cap_output_byte_offset();
            const payload = Buffer.from(JSON.stringify({{ amount: 200.00, tax_rate: 0.0825, currency: "USD" }}), 'utf8');
            new Uint8Array(ex.memory.buffer, inOff, payload.length).set(payload);
            const outLen = ex.cap_execute(payload.length);
            if (outLen <= 0) throw new Error('cap_execute failed with ' + outLen);
            const outStr = Buffer.from(new Uint8Array(ex.memory.buffer, outOff, outLen)).toString('utf8');
            process.stdout.write(outStr);
        }}).catch(err => {{ console.error(err); process.exit(1); }});
        "#,
        path = ifw_path.to_str().unwrap()
    );
    let out = Command::new("node")
        .arg("-e")
        .arg(&script)
        .output()
        .expect("node must be available");
    assert!(
        out.status.success(),
        "node capsule run failed: {}",
        String::from_utf8_lossy(&out.stderr)
    );
    let out_json: serde_json::Value =
        serde_json::from_slice(&out.stdout).expect("output must be valid JSON");
    assert_eq!(out_json["tax"], 16.5);
    assert_eq!(out_json["total"], 216.5);
    assert_eq!(out_json["currency"], "USD");
}

#[test]
fn cap_read_secret_cannot_leak_into_json_output() {
    let ws = temp_ws("secret_leak");
    let mut store = GraphStore::open(&ws).unwrap();

    let bad_src = r#"
        use "std/capsule.ifm";
        use "std/json.ifm";

        export fn cap_execute(in_len: i64) -[Mem]-> i64 {
          let sec = cap_read_secret(16);
          let out = cap_output_buf();
          let mut w = jw_begin_obj(out);
          w = jw_kv_str(out, w, "leaked", sec);
          cap_finish(jw_end_obj(out, w))
        }
    "#;

    let module = parse_module(bad_src).expect("parse ok");
    let err = build_strata_wasm_from_module(&mut store, &module, &ws.join("leak.ifw"))
        .expect_err("Compiler must statically block Secret<str> leak into jw_kv_str");
    assert!(
        err.code.starts_with("E06"),
        "Expected E06x type/secret error, got: {err:?}"
    );
}

#[test]
fn cli_verify_json_v2_envelope_collects_multiple_diagnostics_with_dec64_cex() {
    let ws = temp_ws("verify_json_v2");
    let ifm_file = ws.join("pricing_bad.ifm");
    fs::write(
        &ifm_file,
        r#"pub fn bad_discount(amount: dec64, rate: dec64) -> dec64
  req amount >= 0.0d && amount <= 1000.0d
  req rate >= 0.0d && rate <= 2.0d
  ens result >= 0.0d
  ens result <= 500.0d
{
  amount - (amount * rate)
}
"#,
    )
    .unwrap();

    let bin = env!("CARGO_BIN_EXE_infimum");
    let out = Command::new(bin)
        .args(["verify", ifm_file.to_str().unwrap(), "--json", "--no-cache"])
        .current_dir(&ws)
        .output()
        .expect("run infimum verify --json");
    assert!(
        !out.status.success(),
        "verify must fail on violated contracts"
    );

    let env: serde_json::Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must be valid JSON envelope ({e}), got: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(env["ok"], false);
    assert_eq!(env["phase"], "verify");
    let diags = env["diagnostics"].as_array().expect("diagnostics array");
    assert!(
        diags.len() >= 2,
        "Expected >= 2 collected diagnostics in one pass, got {}: {env:#}",
        diags.len()
    );
    let d0 = &diags[0];
    assert_eq!(d0["code"], "E050_CONTRACT_VIOLATION");
    assert_eq!(d0["function"], "bad_discount");
    assert!(
        d0["file"].as_str().unwrap_or("").contains("pricing_bad.ifm"),
        "missing file field: {d0:#}"
    );
    assert!(
        d0["source_line"].as_str().unwrap_or("").contains("bad_discount"),
        "missing source_line field: {d0:#}"
    );
    let cex = d0["counterexample"].as_object().expect("counterexample obj");
    let rate_cex = cex.get("rate").and_then(|v| v.as_str()).unwrap_or("");
    assert!(
        rate_cex.contains("d (raw "),
        "Expected human-readable dec64 counterexample format, got '{rate_cex}'"
    );
}
