use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::strata_elf::{build_strata_elf_target, run_strata_elf_target};
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
    let dir = std::env::temp_dir().join(format!("infimum_s7_{tag}_{nanos}"));
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
fn test_direct_strata_wasm_zero_rustc_framebuffer_and_passport() {
    let ws = temp_workspace("strata_wasm");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        // Z3-proven pure helper clamping coordinate into [0, limit]
        fn clamp_coord(v: i64, limit: i64) -> i64
          req limit >= 0
          ens result >= 0 && result <= limit
        {
          if v < 0 { 0 } else if v > limit { limit } else { v }
        }

        // Exported getter placed BEFORE helper `pack_rgba` to test topological export resolution!
        export fn get_magic_offset() -> i64
          ens result == 4096
        {
          4096
        }

        // Regression test for negative JS i32 -> Wasm i64 sign-extension (`-1`, `-12`)!
        export fn step_signed(dx: i64, dy: i64) -> i64
          ens result == dx * 100 + dy
        {
          dx * 100 + dy
        }

        fn pack_rgba(r: i64, g: i64, b: i64) -> i64 {
          0xFF000000 | ((b & 255) << 16) | ((g & 255) << 8) | (r & 255)
        }

        // Direct Strata framebuffer kernel with bounded termination (`max 256`)
        export fn render_strip(count: i64, red: i64) -[Mem]-> i64
          req count >= 0 && count <= 100
        {
          let mut i = 0;
          let mut checksum = 0;
          let safe_r = clamp_coord(red, 255);
          while i < count max 256 {
            let color = pack_rgba(safe_r, i * 2, 128);
            mem_store(1024 + i, color);
            checksum = checksum + mem_load(1024 + i);
            i = i + 1;
          }
          checksum
        }
    "#;

    let module = parse_module(src).expect("parse strata module ok");
    let wasm_path = ws.join("strip.wasm");
    let art = build_strata_wasm_from_module(&mut store, &module, &wasm_path)
        .expect("direct strata wasm compile ok");

    // Verify ultra-tiny binary size (< 2 KB — zero Rust runtime bloat!)
    assert!(
        art.wasm_size_bytes < 2048,
        "Expected direct Strata Wasm < 2 KB, got {} bytes",
        art.wasm_size_bytes
    );
    assert_eq!(
        art.exported_functions,
        vec![
            "get_magic_offset".to_string(),
            "step_signed".to_string(),
            "render_strip".to_string()
        ]
    );

    if let Some(node_bin) = find_node_bin() {
        let js_test = ws.join("run_strata.mjs");
        let js = r#"
import fs from 'node:fs';
import assert from 'node:assert/strict';

const bytes = fs.readFileSync(process.argv[2]);
const mod = await WebAssembly.compile(bytes);

// 1. Check embedded custom section `infimum.passport`
const sections = WebAssembly.Module.customSections(mod, 'infimum.passport');
assert.equal(sections.length, 1);
const passport = JSON.parse(new TextDecoder().decode(sections[0]));
assert.equal(passport.is_pure, true);

// 2. Instantiate & invoke JS-friendly i32 exports directly, including negative i32 args!
const inst = await WebAssembly.instantiate(mod, {});
const exp = inst.exports;
assert.equal(exp.get_magic_offset(), 4096);
assert.equal(exp.step_signed(-1, -12), -112);
assert.equal(exp.step_signed(-1, 1), -99);
exp.render_strip(10, 300); // 300 clamps to 255!

// 3. Read linear memory framebuffer at word offset 1024 (`byte offset 4096`)
const u32 = new Uint32Array(exp.memory.buffer, 1024 * 4, 10);
assert.equal(u32[0], 0xFF8000FF); // R=255, G=0, B=128, A=255
assert.equal(u32[5], 0xFF800AFF); // R=255, G=10, B=128, A=255
console.log('STRATA_WASM_PIXEL_OK');
"#;
        fs::write(&js_test, js).unwrap();
        let out = Command::new(node_bin)
            .args([js_test.to_str().unwrap(), wasm_path.to_str().unwrap()])
            .output()
            .unwrap();
        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "Strata Wasm V8 failed: stdout={stdout} stderr={stderr}"
        );
        assert!(stdout.contains("STRATA_WASM_PIXEL_OK"));
    }

    let _ = fs::remove_dir_all(&ws);
}

#[test]
fn test_direct_strata_linux_x86_64_elf64_zero_rustc() {
    let ws = temp_workspace("strata_elf");
    let mut store = GraphStore::open(&ws).unwrap();

    let src = r#"
        fn fib(n: i64) -> i64
          req n >= 0
          ens result >= 0
        {
          if n <= 1 {
            n
          } else {
            fib(n - 1) + fib(n - 2)
          }
        }

        fn mem_sum_squares(n: i64) -[Mem]-> i64
          req n >= 1 && n <= 100
        {
          let mut i = 1;
          while i <= n max 128 {
            mem_store(i, i * i);
            i = i + 1;
          }
          let mut k = 1;
          let mut acc = 0;
          while k <= n max 128 {
            acc = acc + mem_load(k);
            k = k + 1;
          }
          acc + fib(10) - 100
        }

        fn neg_diff(a: i64, b: i64) -> i64
          ens result == a - b
        {
          a - b
        }
    "#;

    store.ingest_module(&parse_module(src).unwrap()).unwrap();

    // 1. Build tiny standalone x86_64 ELF64 executable (< 4 KB, zero rustc/libc!)
    let elf_out = ws.join("mem_sum");
    let art = build_strata_elf_target(&store, "mem_sum_squares", &elf_out).unwrap();
    assert!(
        art.elf_size_bytes < 4096,
        "Expected tiny ELF64 < 4 KB, got {} bytes",
        art.elf_size_bytes
    );

    // 2. Run `mem_sum_squares(5)`: (1 + 4 + 9 + 16 + 25) + fib(10)=55 - 100 = 55 + 55 - 100 = 10
    let run_ok = run_strata_elf_target(&store, &ws, "mem_sum_squares", &["5"]).unwrap();
    assert_eq!(run_ok.exit_code, 0);
    assert_eq!(run_ok.stdout.trim(), "10");

    // 3. Test negative result formatting (`10 - 35 = -25`)
    let run_neg = run_strata_elf_target(&store, &ws, "neg_diff", &["10", "35"]).unwrap();
    assert_eq!(run_neg.exit_code, 0);
    assert_eq!(run_neg.stdout.trim(), "-25");

    // 4. Test boundary `req n >= 1 && n <= 100` rejection (`0` -> exit code 2)
    let run_bad = run_strata_elf_target(&store, &ws, "mem_sum_squares", &["0"]).unwrap();
    assert_eq!(run_bad.exit_code, 2);

    let _ = fs::remove_dir_all(&ws);
}
