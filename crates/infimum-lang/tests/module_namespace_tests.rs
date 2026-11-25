// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0
//
// Comprehensive test suite for the strict Pascal-unit + C fail-loud module system:
// - `pub` interface / private-by-default implementation on all 6 entity kinds
// - 3 `use` forms (`use "f";`, `use "f" as m;`, `use "f" { a, b as c };`) + `pub use`
// - No transitive leaks, no shadowing, diamond deduplication
// - Diagnostics: E024_DUPLICATE_SYMBOL, E067_UNKNOWN_IMPORT, E068_PRIVATE_ITEM, E069_DUPLICATE_ALIAS
// - Qualified `m::...` access across const, enum, struct, layout, fn, match, forma, Slot
// - Virtual FS parity (`parse_module_from_virtual_fs` & `compile_project_in_memory`)

use std::collections::BTreeMap;
use std::fs;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};

use infimum_lang::eval::run_module_tests;
use infimum_lang::parser::{parse_module_from_path, parse_module_from_virtual_fs};
use infimum_lang::wasm_api::compile_project_in_memory;

fn assert_all_ifm_tests_pass(module: &infimum_lang::ast::Module) {
    let results = run_module_tests(module);
    assert!(!results.is_empty(), "expected at least one built-in test block");
    for r in results {
        assert!(r.passed, "test '{}' failed: {:?}", r.name, r.error);
    }
}

static COUNTER: AtomicU64 = AtomicU64::new(0);

fn temp_dir(tag: &str) -> PathBuf {
    let n = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "infimum_mod_ns_test_{}_{}_{}",
        std::process::id(),
        tag,
        n
    ));
    let _ = fs::remove_dir_all(&dir);
    fs::create_dir_all(&dir).unwrap();
    dir
}

#[test]
fn encapsulation_isolates_private_helpers_and_rejects_external_access() {
    let mut files = BTreeMap::new();
    files.insert(
        "a.ifm".to_string(),
        r#"
const SECRET: i64 = 10;
fn helper(x: i64) -> i64 { x + SECRET }
pub fn from_a(x: i64) -> i64 { helper(x) }
"#
        .to_string(),
    );
    files.insert(
        "b.ifm".to_string(),
        r#"
const SECRET: i64 = 100;
fn helper(x: i64) -> i64 { x + SECRET }
pub fn from_b(x: i64) -> i64 { helper(x) }
"#
        .to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        r#"
use "a.ifm";
use "b.ifm";
const SECRET: i64 = 1000;
fn helper(x: i64) -> i64 { x + SECRET }
export fn run(x: i64) -> i64 { from_a(x) + from_b(x) + helper(x) }
test "private helpers isolated" {
  assert(run(1) == 1113);
}
"#
        .to_string(),
    );

    let module = parse_module_from_virtual_fs("main.ifm", &files)
        .expect("identical private helpers across units must never collide");
    assert_all_ifm_tests_pass(&module);

    // Accessing private fn via namespace must fail with E068_PRIVATE_ITEM
    let mut bad_ns = files.clone();
    bad_ns.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" as a;\nexport fn run() -> i64 { a::helper(1) }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &bad_ns).unwrap_err();
    assert_eq!(err.code, "E068_PRIVATE_ITEM");

    // Accessing private const via namespace must fail with E068_PRIVATE_ITEM
    let mut bad_const = files.clone();
    bad_const.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" as a;\nexport fn run() -> i64 { a::SECRET }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &bad_const).unwrap_err();
    assert_eq!(err.code, "E068_PRIVATE_ITEM");

    // Selective import of private item must fail with E068_PRIVATE_ITEM
    let mut bad_sel = files.clone();
    bad_sel.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" { helper };\nexport fn run() -> i64 { helper(1) }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &bad_sel).unwrap_err();
    assert_eq!(err.code, "E068_PRIVATE_ITEM");

    // Selective import of nonexistent item must fail with E067_UNKNOWN_IMPORT
    let mut bad_unk = files.clone();
    bad_unk.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" { does_not_exist };\nexport fn run() -> i64 { 0 }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &bad_unk).unwrap_err();
    assert_eq!(err.code, "E067_UNKNOWN_IMPORT");
}

#[test]
fn no_transitive_leaks_and_explicit_pub_use_reexports() {
    let mut files = BTreeMap::new();
    files.insert(
        "base.ifm".to_string(),
        "pub const K: i64 = 7;\npub fn base_fn(x: i64) -> i64 { x * K }\n".to_string(),
    );
    // Private `use "base.ifm";` in mid.ifm must NOT leak K or base_fn to main.ifm
    files.insert(
        "mid.ifm".to_string(),
        "use \"base.ifm\";\npub fn mid_fn(x: i64) -> i64 { base_fn(x) + 1 }\n".to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        "use \"mid.ifm\";\nexport fn run(x: i64) -> i64 { base_fn(x) }\n".to_string(),
    );

    let err = compile_project_in_memory("main.ifm", &files)
        .expect_err("private use in mid.ifm must not leak base_fn to main.ifm");
    assert_eq!(err.code, "E022_UNKNOWN_FUNCTION");

    // Explicit `pub use "base.ifm" { K, base_fn as re_fn };` and `pub use "base.ifm" as b;`
    files.insert(
        "mid.ifm".to_string(),
        r#"
pub use "base.ifm" { K, base_fn as re_fn };
pub use "base.ifm" as b;
pub fn mid_fn(x: i64) -> i64 { re_fn(x) + 1 }
"#
        .to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        r#"
use "mid.ifm";
export fn run(x: i64) -> i64 { mid_fn(x) + re_fn(x) + b::base_fn(x) + K }
test "pub use reexports" {
  assert(run(2) == 50);
}
"#
        .to_string(),
    );
    let module = parse_module_from_virtual_fs("main.ifm", &files)
        .expect("explicit pub use re-exports must work");
    assert_all_ifm_tests_pass(&module);
}

#[test]
fn selective_import_with_alias_hides_original_name() {
    let mut files = BTreeMap::new();
    files.insert(
        "math_util.ifm".to_string(),
        "pub const SCALE: i64 = 10;\npub fn add(a: i64, b: i64) -> i64 { a + b }\n".to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        r#"
use "math_util.ifm" { SCALE as K, add as plus };
export fn run(x: i64) -> i64 { plus(x, K) }
test "selective alias" {
  assert(run(5) == 15);
}
"#
        .to_string(),
    );

    let module = parse_module_from_virtual_fs("main.ifm", &files).expect("selective alias works");
    assert_all_ifm_tests_pass(&module);

    // Original name `add` must NOT be visible when aliased as `plus`
    files.insert(
        "main.ifm".to_string(),
        "use \"math_util.ifm\" { add as plus };\nexport fn run(x: i64) -> i64 { add(x, 1) }\n"
            .to_string(),
    );
    let err = compile_project_in_memory("main.ifm", &files).unwrap_err();
    assert_eq!(err.code, "E022_UNKNOWN_FUNCTION");
}

#[test]
fn qualified_namespace_access_across_all_six_entity_kinds_match_and_forma() {
    let dir = temp_dir("qualified_all");
    fs::write(
        dir.join("game.ifm"),
        r#"
use "std/prelude.ifm";

pub const MAX_N: i64 = 4;

pub enum Phase {
  Idle = 0,
  Active = 1,
}

pub struct Vec2 {
  x: i64,
  y: i64,
}

pub layout BoardMem {
  cells: words[MAX_N],
  pts: Vec2[2],
}

pub fn make_vec(x: i64, y: i64) -> Vec2 {
  Vec2 { x: x, y: y }
}

pub fn render_slot(rx: i64, ry: i64, rw: i64, rh: i64, clip: i64) -[Mem]-> unit {
  ui_rect(rx, ry, rw, rh, 4, 0, 0, 0xFF223344, 0, clip);
}

pub forma Badge(val: i64) {
  Row(w: 120, h: 32, bg: 0xFF112233) {
    Slot(w: 40, h: 24, render: render_slot)
    Text(text: "OK", size: 14, color: 0xFFFFFFFF)
  }
}
"#,
    )
    .unwrap();

    let main_path = dir.join("main.ifm");
    fs::write(
        &main_path,
        r#"
use "std/prelude.ifm";
use "game.ifm" as g;

fn double_vec(v: g::Vec2) -> g::Vec2 {
  g::Vec2 { x: v.x * 2, y: v.y * 2 }
}

forma Root(v: i64) {
  Col(w: 200, h: 100) {
    g::Badge(v)
    Slot(w: 60, h: 24, render: g::render_slot)
  }
}

export fn check_all(p: i64) -[Mem]-> i64 {
  let v0: g::Vec2 = g::make_vec(3, 4);
  let v1 = double_vec(v0);
  g::BoardMem.cells[0] = g::MAX_N;
  g::BoardMem.pts[0] = v1;
  let bonus = match p {
    g::Phase::Idle => 1,
    g::Phase::Active => 10,
    _ => 0,
  };
  g::BoardMem.cells[0] + g::BoardMem::cells::count + g::Vec2::SIZE + g::BoardMem.pts[0].x + g::BoardMem.pts[0].y + bonus
}

export fn step_ui(vw: i64, vh: i64, mx: i64, my: i64, _md: i64, mc: i64, wy: i64, _kc: i64, _ks: i64) -[Mem, Gpu]-> i64 {
  forma_begin_frame(vw, vh, mx, my, mc, wy);
  Root(1);
  gfx_end_frame()
}

test "qualified all six kinds" {
  assert(check_all(1) == 34);
}
"#,
    )
    .unwrap();

    let module = parse_module_from_path(&main_path)
        .expect("qualified access across all 6 kinds + match + forma + Slot must parse & lower");
    assert_all_ifm_tests_pass(&module);

    // Verify full Wasm compilation via virtual FS too
    let mut vfs = BTreeMap::new();
    vfs.insert(
        "game.ifm".to_string(),
        fs::read_to_string(dir.join("game.ifm")).unwrap(),
    );
    vfs.insert(
        "main.ifm".to_string(),
        fs::read_to_string(&main_path).unwrap(),
    );
    let (wasm, meta) = compile_project_in_memory("main.ifm", &vfs)
        .expect("qualified namespace + forma project must compile to valid Wasm");
    assert!(wasm.starts_with(b"\0asm"));
    assert!(meta.exported_functions.contains(&"check_all".to_string()));
    assert!(meta.exported_functions.contains(&"step_ui".to_string()));
}

#[test]
fn collisions_fail_loud_with_e024_and_e069() {
    let mut files = BTreeMap::new();
    files.insert("a.ifm".to_string(), "pub fn foo() -> i64 { 1 }\n".to_string());
    files.insert("b.ifm".to_string(), "pub fn foo() -> i64 { 2 }\n".to_string());

    // Two `use` bringing different `foo` -> E024_DUPLICATE_SYMBOL
    files.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\";\nuse \"b.ifm\";\nexport fn run() -> i64 { foo() }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &files).unwrap_err();
    assert_eq!(err.code, "E024_DUPLICATE_SYMBOL");
    assert!(
        err.message.contains("a.ifm:1") && err.message.contains("b.ifm:1"),
        "Expected both source files in E024 message: {}",
        err.message
    );

    // Local `fn foo` colliding with imported `foo` -> E024_DUPLICATE_SYMBOL (no shadowing!)
    files.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\";\nfn foo() -> i64 { 3 }\nexport fn run() -> i64 { foo() }\n".to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &files).unwrap_err();
    assert_eq!(err.code, "E024_DUPLICATE_SYMBOL");

    // Duplicate module namespace alias -> E069_DUPLICATE_ALIAS
    files.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" as m;\nuse \"b.ifm\" as m;\nexport fn run() -> i64 { m::foo() }\n"
            .to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &files).unwrap_err();
    assert_eq!(err.code, "E069_DUPLICATE_ALIAS");

    // Duplicate selective alias -> E069_DUPLICATE_ALIAS
    files.insert(
        "main.ifm".to_string(),
        "use \"a.ifm\" { foo as x };\nuse \"b.ifm\" { foo as x };\nexport fn run() -> i64 { x() }\n"
            .to_string(),
    );
    let err = parse_module_from_virtual_fs("main.ifm", &files).unwrap_err();
    assert_eq!(err.code, "E069_DUPLICATE_ALIAS");
}

#[test]
fn diamond_layout_reexport_deduplicates_without_overlap() {
    let mut files = BTreeMap::new();
    files.insert(
        "mem.ifm".to_string(),
        "pub layout SharedMem {\n  a: word,\n  b: word,\n}\n".to_string(),
    );
    files.insert(
        "left.ifm".to_string(),
        "pub use \"mem.ifm\";\npub fn set_a(v: i64) -[Mem]-> unit { SharedMem.a = v; }\n"
            .to_string(),
    );
    files.insert(
        "right.ifm".to_string(),
        "pub use \"mem.ifm\";\npub fn set_b(v: i64) -[Mem]-> unit { SharedMem.b = v; }\n"
            .to_string(),
    );
    files.insert(
        "main.ifm".to_string(),
        r#"
use "left.ifm";
use "right.ifm";
export fn run() -[Mem]-> i64 {
  set_a(19);
  set_b(23);
  SharedMem.a + SharedMem.b
}
test "diamond layout" {
  assert(run() == 42);
}
"#
        .to_string(),
    );

    let module = parse_module_from_virtual_fs("main.ifm", &files)
        .expect("diamond layout re-export must deduplicate cleanly");
    assert_eq!(module.layouts.len(), 1);
    assert_all_ifm_tests_pass(&module);
}
