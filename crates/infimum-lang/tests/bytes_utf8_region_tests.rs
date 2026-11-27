// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

use infimum_lang::abi::{generate_js_abi, generate_rust_abi};
use infimum_lang::eval::run_module_tests;
use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module;
use std::fs;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_ws(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_p2_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn u8_bytes_vec_u8_slices_eval_and_wasm_verify() {
    let ws = temp_ws("u8_vec");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        use "std/mem.ifm";

        layout ByteState {
          raw: bytes[16],
          msg: vec<u8>[64],
        }

        fn sum_bytes(s: [u8]) -[Mem]-> i64
          ens result >= 0
        {
          let mut acc = 0;
          for i in 0..s.len max 64 {
            acc += s[i];
          }
          if acc < 0 { 0 } else { acc }
        }

        export fn run_demo() -[Mem]-> i64 {
          ByteState.raw[0] = 10;
          ByteState.raw[1] = 200;
          ByteState.raw[2] = u8(300); // 300 & 255 = 44
          ByteState.msg.clear();
          let _ = ByteState.msg.try_push(65);
          let _ = ByteState.msg.try_push(66);
          let _ = ByteState.msg.try_push(255);
          let sub = ByteState.msg[1..3];
          let s_sum = sum_bytes(sub); // 66 + 255 = 321
          let popped = if ByteState.msg.len > 0 { ByteState.msg.pop() } else { 0 };
          let sc = scratch_calloc_u8(8);
          if sc.len == 8 {
            sc[0] = 7;
            sc[7] = 9;
          }
          ByteState.raw[0] + ByteState.raw[1] + ByteState.raw[2] + s_sum + popped + sum_bytes(sc)
        }

        test "u8 layout, vec<u8>, [u8] subslice, and scratch_calloc_u8 work" {
          // 10 + 200 + 44 + 321 + 255 + (7 + 9) = 846
          assert(run_demo() == 846);
          assert(ByteState.msg.len == 2);
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    let ifw_path = ws.join("u8_demo.ifw");
    let art = build_strata_wasm_from_module(&mut store, &module, &ifw_path).expect("verify+wasm ok");
    assert!(art.wasm_size_bytes > 0);

    // Check export-abi emits byte offsets for bytes[16] and vec<u8>[64]
    let rust_abi = generate_rust_abi(&module).expect("rust abi ok");
    assert!(rust_abi.contains("BYTE_STATE_RAW_BYTE_OFFSET"));
    assert!(rust_abi.contains("BYTE_STATE_RAW_CAP_BYTES: usize = 16;"));
    assert!(rust_abi.contains("BYTE_STATE_MSG_DATA_BYTE_OFFSET"));
    assert!(rust_abi.contains("BYTE_STATE_MSG_CAP_BYTES: usize = 64;"));
    let js_abi = generate_js_abi(&module).expect("js abi ok");
    assert!(js_abi.contains("export const BYTE_STATE_MSG_DATA_BYTE_OFFSET"));

    // Run evaluator tests
    let results = run_module_tests(&module);
    assert_eq!(results.len(), 1);
    assert!(results[0].passed, "Test failed: {:?}", results[0]);

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn u8_out_of_range_store_rejected_by_z3() {
    let ws = temp_ws("u8_oob");
    let mut store = GraphStore::open(&ws).unwrap();

    let bad_high = r#"
        layout Buf {
          b: bytes[8],
        }
        export fn write_256() -[Mem]-> unit {
          Buf.b[0] = 256;
        }
    "#;
    let m1 = parse_module(bad_high).expect("parse ok");
    let err1 = store.ingest_module(&m1).unwrap_err();
    assert_eq!(err1.code, "E054_FIELD_INVARIANT_VIOLATED");

    let bad_neg = r#"
        layout Buf {
          b: bytes[8],
        }
        export fn write_neg() -[Mem]-> unit {
          Buf.b[0] = -1;
        }
    "#;
    let m2 = parse_module(bad_neg).expect("parse ok");
    let err2 = store.ingest_module(&m2).unwrap_err();
    assert_eq!(err2.code, "E054_FIELD_INVARIANT_VIOLATED");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn region_isolation_preserves_vec_len_across_dynamic_data_writes() {
    let ws = temp_ws("region_iso");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        struct Item {
          hp: word inv 0..=100,
          dmg: word,
        }

        layout State {
          items: vec<Item>[16],
        }

        export fn mutate_then_push(idx: i64) -[Mem]-> unit {
          if State.items.len < State.items.cap {
            if idx >= 0 && idx < State.items.len {
              // Dynamic write into region(items_data) MUST NOT invalidate region(items_len)!
              State.items[idx].hp = 50;
            }
            // Z3 proves State.items.len < 16 WITHOUT repeating the `if` guard!
            State.items.push(Item { hp: 100, dmg: 10 });
          }
        }
    "#;

    let module = parse_module(src).expect("parse ok");
    store
        .ingest_module(&module)
        .expect("Region isolation must let Z3 prove push without re-checking len < cap");

    let _ = fs::remove_dir_all(&ws);
}
