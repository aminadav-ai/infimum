use infimum_lang::parser::parse_module;
use infimum_lang::store::GraphStore;
use infimum_lang::wasm::build_wasm_module;
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
    let dir = std::env::temp_dir().join(format!("infimum_s5_{tag}_{nanos}"));
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
fn test_wasm_compilation_size_cache_and_v8_timetravel_replay() {
    let ws = temp_workspace("wasm_tt");
    let mut store = GraphStore::open(&ws).unwrap();

    let demo = fs::read_to_string("examples/payment_demo.ifm").unwrap();
    store.ingest_module(&parse_module(&demo).unwrap()).unwrap();

    // 1. Compile verified subgraph to WebAssembly (.wasm)
    let wasm_out = ws.join("checkout.wasm");
    let art1 = build_wasm_module(&store, &ws, "checkout", Some(&wasm_out)).unwrap();
    assert_eq!(art1.compiled_nodes, 3);
    assert!(!art1.cache_hit);

    // Check Wasm magic header `\0asm` and compact size (< 120 KB!)
    let wasm_bytes = fs::read(&wasm_out).unwrap();
    assert_eq!(&wasm_bytes[0..4], b"\0asm");
    assert!(
        wasm_bytes.len() < 120 * 1024,
        "Expected compact Wasm binary < 120 KB, got {} bytes",
        wasm_bytes.len()
    );

    // 2. Second build hits content-addressed Wasm cache!
    let art2 = build_wasm_module(&store, &ws, "checkout", None).unwrap();
    assert!(art2.cache_hit);

    // 3. Execute inside real V8 WebAssembly engine (Node.js) and verify Time-Travel scrubbing!
    if let Some(node_bin) = find_node_bin() {
        let harness_js = ws.join("verify_wasm.mjs");
        let js_code = r#"
import fs from 'node:fs';
import assert from 'node:assert/strict';

const wasmPath = process.argv[2];
const bytes = fs.readFileSync(wasmPath);
const { instance } = await WebAssembly.instantiate(bytes, {});
const exp = instance.exports;

const dec = new TextDecoder();
const enc = new TextEncoder();

function readOut() {
  const ptr = exp.infimum_out_ptr();
  const len = exp.infimum_out_len();
  return JSON.parse(dec.decode(new Uint8Array(exp.memory.buffer, ptr, len)));
}

function stepArgs(args) {
  const payload = enc.encode(args.join('\n'));
  const inPtr = exp.infimum_prepare_input(payload.length);
  new Uint8Array(exp.memory.buffer, inPtr, payload.length).set(payload);
  const code = exp.infimum_step();
  return { code, data: readOut() };
}

// A. Check embedded compile-time Security Passport inside .wasm
exp.infimum_load_passport();
const passport = readOut();
assert.equal(passport.total_reachable_nodes, 3);
assert.equal(passport.uses_secret_types, true);
assert.equal(passport.secret_pipes[0].destination_domain, 'api.mybank.com');

// B. Execute 3 valid steps into the FlightRecorder timeline
const s0 = stepArgs(['sk_secret_alpha', '2000']);
const s1 = stepArgs(['sk_secret_beta', '10000']);
const s2 = stepArgs(['sk_secret_gamma', '40000']);
assert.equal(s0.code, 0);
assert.equal(s1.code, 0);
assert.equal(s2.code, 0);
assert.equal(exp.infimum_frame_count(), 3);

// C. Boundary precondition violation (`total = 50 <= 100`) must be rejected & NOT corrupt timeline!
const bad = stepArgs(['sk_secret_bad', '50']);
assert.equal(bad.code, 2);
assert.equal(bad.data.status, 'error');
assert.equal(bad.data.kind, 'precondition');
assert.equal(exp.infimum_frame_count(), 3);

// D. Scrub Time-Travel slider backward to Frame #0 (`total = 2000`, fee = 50, net = 1950)
assert.equal(exp.infimum_scrub_frame(0), 0);
const f0 = readOut();
assert.equal(f0.frame_id, 0);
assert.equal(f0.inputs[0], '[REDACTED SECRET]');
assert.equal(f0.inputs[1], '2000');
assert.equal(f0.effects[0].args[0], '1950');
assert.ok(!JSON.stringify(f0).includes('sk_secret_alpha'), 'Secret must never appear in Wasm frame!');

// E. Scrub forward to Frame #2 (`total = 40000`, fee = 1000, net = 39000)
assert.equal(exp.infimum_scrub_frame(2), 0);
const f2 = readOut();
assert.equal(f2.frame_id, 2);
assert.equal(f2.effects[0].args[0], '39000');
assert.equal(f2.calls.length, 3);

console.log('V8_WASM_TIMETRAVEL_OK: frames=' + exp.infimum_frame_count());
"#;
        fs::write(&harness_js, js_code).unwrap();

        let out = Command::new(node_bin)
            .args([harness_js.to_str().unwrap(), wasm_out.to_str().unwrap()])
            .output()
            .expect("node execution ok");

        let stdout = String::from_utf8_lossy(&out.stdout);
        let stderr = String::from_utf8_lossy(&out.stderr);
        assert!(
            out.status.success(),
            "V8 Wasm harness failed: stdout={stdout} stderr={stderr}"
        );
        assert!(stdout.contains("V8_WASM_TIMETRAVEL_OK: frames=3"));
    }

    let _ = fs::remove_dir_all(&ws);
}
