//! Shared self-describing ABI helpers for Z3-verified `.ifw` core modules
//! (`passport_core`, `sanitizer_core`, `audio_core`).
//!
//! Every `.ifw` core uses auto-placed `layout` (never `at 0`) and exports
//! `core_abi_version() -> i32` plus `get_*_word() -> i32` getters.

use wasmi::{Instance, Store};

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

pub fn write_utf8_bytes(mem: &mut [u8], base_byte: usize, s: &str, max_bytes: usize) -> usize {
    let raw = s.as_bytes();
    let cap = max_bytes.min(mem.len().saturating_sub(base_byte));
    let n = raw.len().min(cap);
    mem[base_byte..base_byte + n].copy_from_slice(&raw[..n]);
    n
}

pub fn read_utf8_bytes(mem: &[u8], base_byte: usize, len: usize) -> String {
    let end = base_byte.saturating_add(len).min(mem.len());
    if base_byte >= end {
        return String::new();
    }
    String::from_utf8_lossy(&mem[base_byte..end]).into_owned()
}
