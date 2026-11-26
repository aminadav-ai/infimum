//! Hot-loader for Z3-verified `.ifw` agent cores (`agent_core`, `editor_core`, `outline_core`).
//!
//! Checks `cores/<file_name>` on disk first (so recompiling a `.ifm` core takes effect
//! immediately without recompiling `infimum-agentd`), falling back to embedded bytes.

use std::path::PathBuf;
use wasmi::{Config, Engine, Instance, Module, Store};

pub fn check_core_abi(
    inst: &Instance,
    store: &mut Store<()>,
    expected: i32,
    core_name: &str,
) -> Result<(), String> {
    let f = inst
        .get_typed_func::<(), i32>(&*store, "core_abi_version")
        .map_err(|e| format!("{core_name}: missing export core_abi_version: {e}"))?;
    let actual = f
        .call(store, ())
        .map_err(|e| format!("{core_name}: core_abi_version trap: {e}"))?;
    if actual != expected {
        return Err(format!(
            "{core_name}: core_abi_version mismatch (expected {expected}, got {actual})"
        ));
    }
    Ok(())
}

pub fn get_word_export(
    inst: &Instance,
    store: &mut Store<()>,
    export_name: &str,
) -> Result<usize, String> {
    let f = inst
        .get_typed_func::<(), i32>(&*store, export_name)
        .map_err(|e| format!("missing word getter '{export_name}': {e}"))?;
    let w = f
        .call(store, ())
        .map_err(|e| format!("word getter '{export_name}' trapped: {e}"))?;
    if w < 0 {
        return Err(format!("word getter '{export_name}' returned negative {w}"));
    }
    Ok(w as usize)
}

#[inline]
pub fn get_i32(mem: &[u8], word: usize) -> i32 {
    let off = word * 4;
    if off + 4 <= mem.len() {
        i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
    } else {
        0
    }
}

#[inline]
pub fn put_i32(mem: &mut [u8], word: usize, val: i32) {
    let off = word * 4;
    if off + 4 <= mem.len() {
        mem[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }
}

pub fn load_core_axw(file_name: &str, embedded: &[u8]) -> (Engine, Module) {
    let mut cfg = Config::default();
    cfg.consume_fuel(true);
    let engine = Engine::new(&cfg);

    let mut candidates = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        // Monorepo workspace target/{release,debug}/infimum-agentd -> apps/infimum-code/cores/<file>
        if let Some(root) = exe.parent().and_then(|p| p.parent()).and_then(|p| p.parent()) {
            candidates.push(root.join("apps/infimum-code/cores").join(file_name));
        }
        // Installed side-by-side layout: <prefix>/bin/infimum-agentd -> <prefix>/cores/<file>
        if let Some(root) = exe.parent().and_then(|p| p.parent()) {
            candidates.push(root.join("cores").join(file_name));
        }
    }
    if let Ok(dir) = std::env::var("INFIMUM_CODE_HOME") {
        candidates.push(PathBuf::from(dir).join("cores").join(file_name));
    }
    candidates.push(PathBuf::from("cores").join(file_name));

    for p in candidates {
        if let Ok(bytes) = std::fs::read(&p) {
            if let Ok(module) = Module::new(&engine, &bytes) {
                return (engine, module);
            }
        }
    }
    let module = Module::new(&engine, embedded)
        .unwrap_or_else(|e| panic!("Embedded core module {file_name} must be valid Wasm: {e}"));
    (engine, module)
}
