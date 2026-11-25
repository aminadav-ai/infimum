// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0
//
// Integration tests for Slices (`[T]`) and Variable-Length Arrays (`vec<T>[CAP]`):
// - Zero-heap, zero-GC, zero-runtime-bounds-check Wasm slices
// - Whole value-struct reads/writes (`let d = s[i]; s[i] = d;`) + field invariants (`E054`)
// - Z3 bounds proofs (`for i in 0..s.len`, `E056_SLICE_OUT_OF_BOUNDS`)
// - Cross-module slices (`[g::Drone]`), `E082_NON_SCALAR_EXPORT`, `E062_SECRET_IN_SLICE`
// - Sound constant-address `known_mem` forwarding in Z3

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use infimum_lang::eval::run_module_tests;
use infimum_lang::parser::parse_module_from_virtual_fs;
use infimum_lang::store::GraphStore;
use infimum_lang::wasm_api::compile_project_in_memory;

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_ws(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "infimum_slice_vec_test_{}_{}_{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn assert_all_ifm_tests_pass(module: &infimum_lang::ast::Module) {
    let results = run_module_tests(module);
    assert!(!results.is_empty(), "expected at least one built-in test block");
    for r in results {
        assert!(r.passed, "test '{}' failed: {:?}", r.name, r.error);
    }
}

#[test]
fn slice_scalar_and_fixed_for_loop_z3_eval_and_wasm() {
    let mut files = BTreeMap::new();
    files.insert(
        "main.ifm".to_string(),
        r#"
use "std/math.ifm";

layout DataMem {
  nums: words[8],
  weights: fixed[4],
}

fn sum_slice(s: [i64]) -[Mem]-> i64 {
  let mut acc = 0;
  for i in 0..s.len max 16 {
    acc += s[i];
  }
  acc
}

fn bump_slice(s: [i64], delta: i64) -[Mem]-> unit {
  for i in 0..s.len max 16 {
    s[i] += delta;
  }
}

fn sum_fixed(s: [fixed]) -[Mem]-> fixed {
  let mut acc: fixed = 0.0;
  for i in 0..s.len max 8 {
    acc = acc + s[i];
  }
  acc
}

fn count_str_chars(txt: str) -[Mem]-> i64 {
  let chars: [i64] = str_as_slice(txt);
  let mut sum = 0;
  for i in 0..chars.len max 16 {
    sum += chars[i];
  }
  sum
}

export fn run_demo() -[Mem]-> i64 {
  for i in 0..8 {
    DataMem.nums[i] = i + 1;
  }
  let all: [i64] = DataMem.nums[..];
  let mid: [i64] = DataMem.nums[1..5]; // [2, 3, 4, 5]
  let tail: [i64] = mid[1..mid.len];   // [3, 4, 5]
  bump_slice(tail, 10);                // -> [13, 14, 15]
  let raw: [i64] = slice_i64(all.ptr, 2); // [1, 2]

  DataMem.weights[0] = 1.5;
  DataMem.weights[1] = 2.5;
  let wf: [fixed] = DataMem.weights[0..2];
  let wsum = fixed_to_int(sum_fixed(wf)); // 4

  let csum = count_str_chars("AB"); // 65 + 66 = 131
  sum_slice(tail) + sum_slice(raw) + wsum + csum
}

test "scalar and fixed slices" {
  // tail: 13+14+15=42, raw: 1+2=3, wsum: 4, csum: 131 => 180
  assert(run_demo() == 180);
}
"#
        .to_string(),
    );

    let module = parse_module_from_virtual_fs("main.ifm", &files).expect("parse ok");
    assert_all_ifm_tests_pass(&module);

    let ws = temp_ws("scalar_fixed");
    let store = GraphStore::open(&ws).unwrap();
    store
        .verify_module(&module)
        .expect("Z3 must prove all slice bounds automatically");

    let (wasm, meta) =
        compile_project_in_memory("main.ifm", &files).expect("Wasm compile must pass");
    assert!(wasm.starts_with(b"\0asm"));
    assert!(meta.exported_functions.contains(&"run_demo".to_string()));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn slice_whole_struct_read_write_field_invariants_and_cross_module() {
    let mut files = BTreeMap::new();
    files.insert(
        "units.ifm".to_string(),
        r#"
pub struct Drone {
  x: word,
  y: word,
  hp: word inv 0..=100,
}

pub layout Arena {
  drones: Drone[4],
}
"#
        .to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        r#"
use "std/math.ifm";
use "units.ifm" as u;

fn heal_drones(drones: [u::Drone], delta: i64) -[Mem]-> i64 {
  let mut alive = 0;
  for i in 0..drones.len max 16 {
    drones[i].hp = clamp_i64(drones[i].hp + delta, 0, 100);
    let d = drones[i];
    if d.hp > 0 {
      drones[i] = u::Drone { x: d.x + 1, y: d.y, hp: d.hp };
      alive += 1;
    }
  }
  alive
}

export fn simulate() -[Mem]-> i64 {
  u::Arena.drones[0] = u::Drone { x: 10, y: 20, hp: 50 };
  u::Arena.drones[1] = u::Drone { x: 30, y: 40, hp: 0 };
  u::Arena.drones[2] = u::Drone { x: 50, y: 60, hp: 95 };
  let active: [u::Drone] = u::Arena.drones[0..3];
  let alive = heal_drones(active, 10);
  // drones[0]: x=11, hp=60; drones[1]: x=31, hp=10; drones[2]: x=51, hp=100; alive=3
  alive + u::Arena.drones[0].x + u::Arena.drones[0].hp + u::Arena.drones[2].hp
}

test "cross-module struct slice read/write" {
  assert(simulate() == 3 + 11 + 60 + 100);
}
"#
        .to_string(),
    );

    let module = parse_module_from_virtual_fs("main.ifm", &files).expect("parse ok");
    assert_all_ifm_tests_pass(&module);

    let ws = temp_ws("struct_slice");
    let store = GraphStore::open(&ws).unwrap();
    store
        .verify_module(&module)
        .expect("Z3 must prove cross-module struct slice + field invariants");

    let (wasm, _) = compile_project_in_memory("main.ifm", &files).expect("Wasm compile ok");
    assert!(wasm.starts_with(b"\0asm"));
    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn slice_oob_bad_range_and_field_inv_rejected_by_z3() {
    let ws = temp_ws("oob");
    let store = GraphStore::open(&ws).unwrap();

    // 1. s[s.len] -> E056_SLICE_OUT_OF_BOUNDS
    let mut f1 = BTreeMap::new();
    f1.insert(
        "main.ifm".to_string(),
        "fn bad_hi(s: [i64]) -[Mem]-> i64 { s[s.len] }\n".to_string(),
    );
    let m1 = parse_module_from_virtual_fs("main.ifm", &f1).expect("parse ok");
    let e1 = store.verify_module(&m1).expect_err("s[s.len] must fail Z3");
    assert_eq!(e1.code, "E056_SLICE_OUT_OF_BOUNDS");

    // 2. s[-1] -> E056_SLICE_OUT_OF_BOUNDS
    let mut f2 = BTreeMap::new();
    f2.insert(
        "main.ifm".to_string(),
        "fn bad_neg(s: [i64]) -[Mem]-> i64 { s[-1] }\n".to_string(),
    );
    let m2 = parse_module_from_virtual_fs("main.ifm", &f2).expect("parse ok");
    let e2 = store.verify_module(&m2).expect_err("s[-1] must fail Z3");
    assert_eq!(e2.code, "E056_SLICE_OUT_OF_BOUNDS");

    // 3. s[3..1] (lo > hi) -> E056_SLICE_OUT_OF_BOUNDS
    let mut f3 = BTreeMap::new();
    f3.insert(
        "main.ifm".to_string(),
        "fn bad_range(s: [i64]) -[Mem]-> i64 req s.len >= 5 { let t = s[3..1]; t.len }\n"
            .to_string(),
    );
    let m3 = parse_module_from_virtual_fs("main.ifm", &f3).expect("parse ok");
    let e3 = store.verify_module(&m3).expect_err("s[3..1] must fail Z3");
    assert_eq!(e3.code, "E056_SLICE_OUT_OF_BOUNDS");

    // 4. Whole-struct write violating field invariant `inv 0..=100` -> E054_FIELD_INVARIANT_VIOLATED
    let mut f4 = BTreeMap::new();
    f4.insert(
        "main.ifm".to_string(),
        r#"
struct Drone { x: word, hp: word inv 0..=100 }
fn bad_inv(s: [Drone]) -[Mem]-> unit req s.len >= 1 {
  s[0] = Drone { x: 1, hp: 999 };
}
"#
        .to_string(),
    );
    let m4 = parse_module_from_virtual_fs("main.ifm", &f4).expect("parse ok");
    let e4 = store
        .verify_module(&m4)
        .expect_err("whole-struct write with hp=999 must fail Z3");
    assert_eq!(e4.code, "E054_FIELD_INVARIANT_VIOLATED");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn slice_export_fn_and_secret_element_rejected() {
    let ws = temp_ws("export_secret");
    let store = GraphStore::open(&ws).unwrap();

    // 1. export fn taking `[i64]` -> E082_NON_SCALAR_EXPORT
    let mut f1 = BTreeMap::new();
    f1.insert(
        "main.ifm".to_string(),
        "export fn bad_exp(s: [i64]) -> i64 { s.len }\n".to_string(),
    );
    let m1 = parse_module_from_virtual_fs("main.ifm", &f1).expect("parse ok");
    let e1 = store
        .verify_module(&m1)
        .expect_err("export fn with slice param must fail");
    assert_eq!(e1.code, "E082_NON_SCALAR_EXPORT");

    // 2. `[Secret<i64>]` -> E062_SECRET_IN_SLICE
    let mut f2 = BTreeMap::new();
    f2.insert(
        "main.ifm".to_string(),
        "fn bad_sec(s: [Secret<i64>]) -> i64 { s.len }\n".to_string(),
    );
    let e2 = parse_module_from_virtual_fs("main.ifm", &f2)
        .expect_err("Secret<T> inside slice must fail");
    assert_eq!(e2.code, "E062_SECRET_IN_SLICE");

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn z3_known_mem_forwarding_is_sound() {
    let ws = temp_ws("known_mem");
    let store = GraphStore::open(&ws).unwrap();

    // 1. Constant-address load forwarded across `if State.denom > 0 { 100 / State.denom }`
    let mut f_ok = BTreeMap::new();
    f_ok.insert(
        "main.ifm".to_string(),
        r#"
layout State {
  denom: word,
  arr: words[4],
}
export fn safe_div() -[Mem]-> i64 {
  if State.denom > 0 {
    100 / State.denom
  } else {
    0
  }
}
"#
        .to_string(),
    );
    let m_ok = parse_module_from_virtual_fs("main.ifm", &f_ok).expect("parse ok");
    store
        .verify_module(&m_ok)
        .expect("known_mem forwarding should prove State.denom > 0 across loads");

    // 2. Dynamic MemStore invalidates known_mem (soundness!)
    let mut f_bad = BTreeMap::new();
    f_bad.insert(
        "main.ifm".to_string(),
        r#"
layout State {
  denom: word,
  arr: words[4],
}
export fn clobbered_div(idx: i64) -[Mem]-> i64 {
  if State.denom > 0 {
    mem_store(idx, 0);
    100 / State.denom
  } else {
    0
  }
}
"#
        .to_string(),
    );
    let m_bad = parse_module_from_virtual_fs("main.ifm", &f_bad).expect("parse ok");
    let err = store
        .verify_module(&m_bad)
        .expect_err("dynamic mem_store must invalidate known_mem");
    assert_eq!(err.code, "E053_POSSIBLE_DIV_BY_ZERO");

    let _ = fs::remove_dir_all(&ws);
}
