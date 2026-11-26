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

#[inline]
pub fn put_i32(mem: &mut [u8], word: usize, val: i32) {
    let off = word * 4;
    if off + 4 <= mem.len() {
        mem[off..off + 4].copy_from_slice(&val.to_le_bytes());
    }
}

pub fn write_raw_chars(mem: &mut [u8], base_word: usize, s: &str, max_chars: usize) -> usize {
    let mut n = 0usize;
    for ch in s.chars().take(max_chars) {
        put_i32(mem, base_word + n, ch as i32);
        n += 1;
    }
    n
}

pub fn read_raw_chars(mem: &[u8], base_word: usize, len: usize) -> String {
    let mut out = String::with_capacity(len);
    for idx in 0..len {
        let cp = get_i32(mem, base_word + idx) as u32;
        if let Some(ch) = char::from_u32(cp) {
            out.push(ch);
        }
    }
    out
}
