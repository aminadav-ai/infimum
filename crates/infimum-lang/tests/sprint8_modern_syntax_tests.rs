use infimum_lang::abi::{generate_json_abi, generate_rust_abi};
use infimum_lang::parser::{parse_module, parse_module_from_path};
use infimum_lang::store::GraphStore;
use infimum_lang::strata_wasm::build_strata_wasm_from_module;
use std::env;
use std::fs;
use std::path::PathBuf;
use std::process::Command;
use std::time::{SystemTime, UNIX_EPOCH};

fn temp_workspace(tag: &str) -> PathBuf {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let dir = std::env::temp_dir().join(format!("infimum_s8_{tag}_{nanos}"));
    fs::create_dir_all(&dir).unwrap();
    dir
}

fn find_node_bin() -> Option<PathBuf> {
    if let Some(home) = env::var_os("HOME") {
        let local_node = PathBuf::from(home).join(".local/bin/node");
        if local_node.is_file() {
            return Some(local_node);
        }
    }
    for p in ["/usr/bin/node", "/usr/local/bin/node"] {
        let pb = PathBuf::from(p);
        if pb.is_file() {
            return Some(pb);
        }
    }
    None
}

#[test]
fn test_multifile_use_diamond_dedup_and_cycle_detection() {
    let ws = temp_workspace("multifile");
    let sub = ws.join("sub");
    fs::create_dir_all(&sub).unwrap();

    // Diamond dependency graph:
    //   main.ifm -> left.ifm -> common.ifm
    //   main.ifm -> sub/right.ifm -> ../common.ifm
    fs::write(
        ws.join("common.ifm"),
        r#"
        pub const BASE_OFFSET: i64 = 100;
        pub enum Mode {
          Idle = 0,
          Active = 1,
        }
        pub fn common_helper(x: i64) -> i64
          ens result == x + BASE_OFFSET
        {
          x + BASE_OFFSET
        }
        "#,
    )
    .unwrap();

    fs::write(
        ws.join("left.ifm"),
        r#"
        pub use "common.ifm";
        pub const LEFT_DELTA: i64 = BASE_OFFSET + 10;
        pub fn left_fn(v: i64) -> i64
          ens result == v + LEFT_DELTA
        {
          common_helper(v) + 10
        }
        "#,
    )
    .unwrap();

    fs::write(
        sub.join("right.ifm"),
        r#"
        pub use "../common.ifm";
        pub fn right_fn(m: i64) -> i64 {
          if m == Mode::Active { BASE_OFFSET } else { 0 }
        }
        "#,
    )
    .unwrap();

    let entry = ws.join("main.ifm");
    fs::write(
        &entry,
        r#"
        use "left.ifm";
        use "sub/right.ifm";
        export fn entry(x: i64) -> i64
          req x >= 0
          ens result == x + 210
        {
          left_fn(x) + right_fn(Mode::Active)
        }
        "#,
    )
    .unwrap();

    let module = parse_module_from_path(&entry).expect("diamond use graph should load cleanly");
    assert_eq!(module.consts.len(), 2);
    assert_eq!(module.enums.len(), 1);
    assert_eq!(module.functions.len(), 4);

    let store = GraphStore::open(&ws).unwrap();
    let reports = store
        .verify_module(&module)
        .expect("Z3 should verify multi-file module");
    assert_eq!(reports.len(), 4);

    // Cycle detection: a.ifm -> b.ifm -> a.ifm
    let cyc_a = ws.join("cyc_a.ifm");
    let cyc_b = ws.join("cyc_b.ifm");
    fs::write(&cyc_a, "use \"cyc_b.ifm\";\nfn fa() -> i64 { 1 }\n").unwrap();
    fs::write(&cyc_b, "use \"cyc_a.ifm\";\nfn fb() -> i64 { 2 }\n").unwrap();

    let err = parse_module_from_path(&cyc_a).expect_err("circular use must be rejected");
    assert_eq!(err.code, "E019_IMPORT_CYCLE");
    assert!(
        err.message.contains("cyc_a.ifm") && err.message.contains("cyc_b.ifm"),
        "Cycle diagnostic should name both files: {}",
        err.message
    );
}

#[test]
fn test_const_in_exprs_z3_contracts_and_while_max() {
    let ws = temp_workspace("consts");
    let store = GraphStore::open(&ws).unwrap();

    let src = r#"
        const MAX_INPUT_CAP: i64 = 8192;
        const INPUT_BUF_END: i64 = 500 + MAX_INPUT_CAP + 4;
        const MASK_LOW: i64 = (1 << 8) - 1;
        const SHIFTED: i64 = (1024 >> 2) ^ (0xF0 & 0x3C) | 3;
        const ENABLE_GUARD: bool = true && (INPUT_BUF_END == 8696);
        const LOOP_LIMIT: i64 = 64;

        export fn bounded_sum(n: i64) -> i64
          req n >= 0 && n <= LOOP_LIMIT && ENABLE_GUARD
          ens result >= 0
        {
          let mut i = 0;
          let mut acc = 0;
          while i < n max LOOP_LIMIT {
            acc += i;
            i += 1;
          }
          if acc < 0 { 0 } else { acc }
        }

        export fn check_const_math() -> i64
          ens result == 8696 + 255
        {
          INPUT_BUF_END + MASK_LOW
        }
    "#;

    let module = parse_module(src).expect("const module parses cleanly");
    assert_eq!(module.consts.len(), 6);
    let reports = store
        .verify_module(&module)
        .expect("Z3 verifies const contracts and while max");
    assert_eq!(reports.len(), 2);
}

#[test]
fn test_enum_and_match_z3_proof_exhaustiveness_and_wasm_exec() {
    let ws = temp_workspace("enum_match");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        enum HostAction {
          None = 0,
          SendChat = 1,
          PickImage = 11,
          AutoNext,
        }

        export fn classify_action(action: i64) -> i64
          ens result >= -1 && result <= 2
        {
          match action {
            HostAction::None => 0,
            HostAction::SendChat | HostAction::PickImage => 1,
            HostAction::AutoNext => 2,
            _ => -1,
          }
        }

        export fn exhaustive_enum_no_wildcard(action: i64) -> i64
          req action == HostAction::None || action == HostAction::SendChat || action == HostAction::PickImage || action == HostAction::AutoNext
          ens result >= 10 && result <= 40
        {
          match action {
            HostAction::None => 10,
            HostAction::SendChat => 20,
            HostAction::PickImage => 30,
            HostAction::AutoNext => 40,
          }
        }

        export fn match_bool_exhaustive(flag: bool) -> i64
          ens result == 1 || result == 0
        {
          match flag {
            true => 1,
            false => 0,
          }
        }
    "#;

    let module = parse_module(src).expect("enum & match parse cleanly");
    let wasm_path = ws.join("enum_match.wasm");
    let art = build_strata_wasm_from_module(&mut store, &module, &wasm_path)
        .expect("Z3 + Strata Wasm compile succeeds");
    assert_eq!(art.exported_functions.len(), 3);

    if let Some(node_bin) = find_node_bin() {
        let js_path = ws.join("run_match.mjs");
        fs::write(
            &js_path,
            r#"
import fs from 'node:fs';
import assert from 'node:assert/strict';

const bytes = fs.readFileSync(process.argv[2]);
const { instance } = await WebAssembly.instantiate(bytes, {});
const ex = instance.exports;

assert.equal(ex.classify_action(0), 0);
assert.equal(ex.classify_action(1), 1);
assert.equal(ex.classify_action(11), 1);
assert.equal(ex.classify_action(12), 2);
assert.equal(ex.classify_action(99), -1);

assert.equal(ex.exhaustive_enum_no_wildcard(0), 10);
assert.equal(ex.exhaustive_enum_no_wildcard(1), 20);
assert.equal(ex.exhaustive_enum_no_wildcard(11), 30);
assert.equal(ex.exhaustive_enum_no_wildcard(12), 40);

assert.equal(ex.match_bool_exhaustive(1), 1);
assert.equal(ex.match_bool_exhaustive(0), 0);
console.log("ENUM_MATCH_WASM_OK");
"#,
        )
        .unwrap();

        let out = Command::new(&node_bin)
            .arg(&js_path)
            .arg(&wasm_path)
            .output()
            .expect("node executes wasm");
        assert!(
            out.status.success(),
            "Node Wasm test failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("ENUM_MATCH_WASM_OK"));
    }

    // Non-exhaustive match must be rejected
    let bad_src = r#"
        enum HostAction { None = 0, SendChat = 1 }
        fn bad(a: i64) -> i64 {
          match a {
            HostAction::None => 0,
          }
        }
    "#;
    let err = parse_module(bad_src).expect_err("non-exhaustive match must fail");
    assert_eq!(err.code, "E063_NON_EXHAUSTIVE_MATCH");
}

#[test]
fn test_layout_auto_pinned_overlap_error_effect_enforcement_and_wasm_memory() {
    let ws = temp_workspace("layout");
    let mut store = GraphStore::open(&ws).unwrap();

    // 1. Intra-layout overlap must fail with E062_LAYOUT_OVERLAP
    let overlap_same = r#"
        layout BadMem at 0 {
          buf_a: words[100] at 500,
          buf_b: words[50] at 599,
        }
        fn noop() -> i64 { 0 }
    "#;
    let err1 = parse_module(overlap_same).expect_err("overlapping layout fields must fail");
    assert_eq!(err1.code, "E062_LAYOUT_OVERLAP");
    assert!(err1.message.contains("BadMem::buf_a") && err1.message.contains("BadMem::buf_b"));

    // 2. Inter-layout overlap must also fail with E062_LAYOUT_OVERLAP
    let overlap_cross = r#"
        layout MemA at 10 {
          slot: words[20],
        }
        layout MemB at 25 {
          other: word,
        }
        fn noop() -> i64 { 0 }
    "#;
    let err2 = parse_module(overlap_cross).expect_err("cross-layout overlap must fail");
    assert_eq!(err2.code, "E062_LAYOUT_OVERLAP");
    assert!(err2.message.contains("MemA::slot") && err2.message.contains("MemB::other"));

    // 3. Layout.field read/write without `-[Mem]->` must be rejected by effect checker
    let missing_mem = r#"
        layout StudioMem at 0 {
          tick: word,
        }
        fn illegal_pure_read() -> i64 {
          StudioMem.tick
        }
    "#;
    let m_bad = parse_module(missing_mem).expect("parses");
    let eff_err = store
        .verify_module(&m_bad)
        .expect_err("Layout.field read must require Mem effect");
    assert_eq!(eff_err.code, "E021_UNDECLARED_EFFECT");

    // 4. Valid layout with auto offsets, pinned `at`, ::len/::end, and scalar .field read/write
    let valid_src = r#"
        const BUF_CAP: i64 = 16;

        layout StudioMem at 0 {
          tick: word,
          theme: word,
          in_scroll_y: word at 47,
          in_follow: word,
          input_buf: words[BUF_CAP] at 500,
        }

        export fn layout_meta() -> i64
          ens result == 0 + 1 + 47 + 48 + 500 + 16 + 516
        {
          StudioMem::tick + StudioMem::theme + StudioMem::in_scroll_y + StudioMem::in_follow
            + StudioMem::input_buf + StudioMem::input_buf::len + StudioMem::input_buf::end
        }

        export fn layout_rw(delta: i64) -[Mem]-> i64 {
          StudioMem.tick = 10;
          StudioMem.tick += delta;
          StudioMem.in_scroll_y = 42;
          StudioMem.in_scroll_y -= 2;
          StudioMem.input_buf[0] = 'A';
          StudioMem.input_buf[1] = '\n';
          StudioMem.tick + StudioMem.in_scroll_y + StudioMem.input_buf[0] + StudioMem.input_buf[1]
        }
    "#;

    let module = parse_module(valid_src).expect("valid layout parses");
    let wasm_path = ws.join("layout.wasm");
    build_strata_wasm_from_module(&mut store, &module, &wasm_path)
        .expect("layout module compiles to Strata Wasm");

    if let Some(node_bin) = find_node_bin() {
        let js_path = ws.join("run_layout.mjs");
        fs::write(
            &js_path,
            r#"
import fs from 'node:fs';
import assert from 'node:assert/strict';

const bytes = fs.readFileSync(process.argv[2]);
const { instance } = await WebAssembly.instantiate(bytes, {});
const ex = instance.exports;
const mem = new Int32Array(ex.memory.buffer);

assert.equal(ex.layout_meta(), 1128);
// tick = 10 + 5 = 15, in_scroll_y = 40, 'A' = 65, '\n' = 10 -> 130
assert.equal(ex.layout_rw(5), 130);
assert.equal(mem[0], 15);
assert.equal(mem[47], 40);
assert.equal(mem[500], 65);
assert.equal(mem[501], 10);
console.log("LAYOUT_WASM_OK");
"#,
        )
        .unwrap();

        let out = Command::new(&node_bin)
            .arg(&js_path)
            .arg(&wasm_path)
            .output()
            .expect("node executes wasm");
        assert!(
            out.status.success(),
            "Node layout test failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("LAYOUT_WASM_OK"));
    }
}

#[test]
fn test_syntax_sugar_chars_if_no_else_else_if_and_compound_assigns() {
    let ws = temp_workspace("sugar");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        export fn char_codes() -> i64
          ens result == 97 + 35 + 32 + 10 + 9 + 13 + 92 + 39
        {
          'a' + '#' + ' ' + '\n' + '\t' + '\r' + '\\' + '\''
        }

        export fn classify_and_mutate(x: i64) -> i64 {
          let mut acc = 10;
          if x < 0 {
            acc -= 5;
          } else if x == 0 {
            acc *= 3;
          } else {
            acc += x;
          }
          if acc > 20 {
            acc |= 1;
          }
          if acc > 100 {
            acc &= 127;
          }
          acc
        }
    "#;

    let module = parse_module(src).expect("sugar parses cleanly");
    let wasm_path = ws.join("sugar.wasm");
    build_strata_wasm_from_module(&mut store, &module, &wasm_path)
        .expect("sugar verifies and compiles to Strata Wasm");

    if let Some(node_bin) = find_node_bin() {
        let js_path = ws.join("run_sugar.mjs");
        fs::write(
            &js_path,
            r#"
import fs from 'node:fs';
import assert from 'node:assert/strict';

const bytes = fs.readFileSync(process.argv[2]);
const { instance } = await WebAssembly.instantiate(bytes, {});
const ex = instance.exports;

assert.equal(ex.char_codes(), 327);
assert.equal(ex.classify_and_mutate(-3), 5);
assert.equal(ex.classify_and_mutate(0), 31);
assert.equal(ex.classify_and_mutate(5), 15);
assert.equal(ex.classify_and_mutate(12), 23);
assert.equal(ex.classify_and_mutate(200), 83);
console.log("SUGAR_WASM_OK");
"#,
        )
        .unwrap();

        let out = Command::new(&node_bin)
            .arg(&js_path)
            .arg(&wasm_path)
            .output()
            .expect("node executes wasm");
        assert!(
            out.status.success(),
            "Node sugar test failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert!(String::from_utf8_lossy(&out.stdout).contains("SUGAR_WASM_OK"));
    }

    // Compound assignment on immutable `let` must still be rejected
    let immut_src = r#"
        fn bad_immut() -> i64 {
          let x = 1;
          x += 2;
          x
        }
    "#;
    let m_immut = parse_module(immut_src).expect("parses");
    let err = store
        .verify_module(&m_immut)
        .expect_err("compound assign to immutable let must fail");
    assert_eq!(err.code, "E067_ASSIGN_IMMUTABLE");
}

#[test]
fn test_export_abi_rust_and_json_compiles_with_rustc() {
    let ws = temp_workspace("abi");

    fs::write(
        ws.join("shared.ifm"),
        r#"
        pub const MAX_INPUT_CAP: i64 = 8192;
        pub const INPUT_BUF_END: i64 = 500 + MAX_INPUT_CAP + 4;
        pub const DEBUG_FLAG: bool = false;

        pub enum HostAction {
          None = 0,
          SendChat = 1,
          PickImage = 11,
          AutoNext,
        }
        "#,
    )
    .unwrap();

    let entry = ws.join("studio.ifm");
    fs::write(
        &entry,
        r#"
        pub use "shared.ifm";

        pub layout StudioMem at 0 {
          tick: word,
          theme: word,
          in_scroll_y: word at 47,
          input_buf: words[8200] at 500,
          projects_buf: words[4000] at 12000,
        }

        export fn ping() -> i64 { HostAction::SendChat }
        "#,
    )
    .unwrap();

    let module = parse_module_from_path(&entry).expect("multi-file module loads");

    // Verify JSON ABI
    let json_abi = generate_json_abi(&module).expect("json abi ok");
    let parsed_json: serde_json::Value = serde_json::from_str(&json_abi).expect("valid json");
    assert_eq!(parsed_json["consts"].as_array().unwrap().len(), 3);
    assert_eq!(parsed_json["enums"].as_array().unwrap().len(), 1);
    assert_eq!(parsed_json["layouts"].as_array().unwrap().len(), 1);

    // Verify Rust ABI content
    let rust_abi = generate_rust_abi(&module).expect("rust abi ok");
    assert!(rust_abi.starts_with("// @generated by infimum export-abi — DO NOT EDIT MANUALLY"));
    assert!(rust_abi.contains("pub const MAX_INPUT_CAP: i64 = 8192;"));
    assert!(rust_abi.contains("pub const INPUT_BUF_END: i64 = 8696;"));
    assert!(rust_abi.contains("pub const STUDIO_MEM_INPUT_BUF_WORD: usize = 500;"));
    assert!(rust_abi.contains("pub const STUDIO_MEM_INPUT_BUF_LEN: usize = 8200;"));
    assert!(rust_abi.contains("#[repr(i32)]"));
    assert!(rust_abi.contains("pub enum HostAction"));
    assert!(rust_abi.contains("TryFrom<i32> for HostAction"));
    assert!(rust_abi.contains("const _: () = assert!("));

    let rs_path = ws.join("studio_abi.rs");
    let rlib_path = ws.join("libstudio_abi.rlib");
    fs::write(&rs_path, &rust_abi).unwrap();

    // Compile generated Rust ABI with `rustc --crate-type lib`
    let status = Command::new("rustc")
        .arg("--edition=2021")
        .arg("--crate-type=lib")
        .arg(&rs_path)
        .arg("-o")
        .arg(&rlib_path)
        .output()
        .expect("rustc invocation");
    assert!(
        status.status.success(),
        "Generated Rust ABI failed to compile:\n{}\n--- SOURCE ---\n{}",
        String::from_utf8_lossy(&status.stderr),
        rust_abi
    );
    assert!(rlib_path.is_file());

    // Also verify the CLI `infimum export-abi <entry.ifm> --rust -o <out.rs>` command end-to-end
    let cli_rs_path = ws.join("cli_abi.rs");
    let cli_out = Command::new(env!("CARGO_BIN_EXE_infimum"))
        .arg("export-abi")
        .arg(&entry)
        .arg("--rust")
        .arg("-o")
        .arg(&cli_rs_path)
        .output()
        .expect("infimum CLI export-abi");
    assert!(
        cli_out.status.success(),
        "CLI export-abi failed: {}",
        String::from_utf8_lossy(&cli_out.stderr)
    );
    let cli_rs = fs::read_to_string(&cli_rs_path).expect("cli generated rs exists");
    assert_eq!(cli_rs, rust_abi);
}
