//! Local zero-JS HTML sanitizer (`Legacy HTML -> Clean Structured Text`)
//! powered by the Z3-verified Infimum Lang module `capsules/core/sanitizer_core.ifw`.
//!
//! Strips `<script>`, `<style>`, `<iframe>`, `<svg>`, `<noscript>`, HTML comments,
//! and all attributes/event handlers before a single byte reaches a capsule.

use std::sync::OnceLock;
use wasmi::{Engine, Linker, Module, Store};

const SANITIZER_AXW: &[u8] = include_bytes!("../capsules/core/sanitizer_core.ifw");
const SANITIZER_CORE_ABI: i32 = 2;
const MAX_IN_BYTES: usize = 65_536;
const MAX_OUT_BYTES: usize = 65_536;
const SANITIZER_FUEL: u64 = 80_000_000;

#[derive(Debug, Clone, Default)]
pub struct SanitizedDoc {
    pub clean_text: String,
    pub stripped_scripts: usize,
    pub stripped_iframes: usize,
    pub stripped_tags: usize,
}

fn compiled_module() -> &'static (Engine, Module) {
    static CELL: OnceLock<(Engine, Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::runtime::load_core_axw("sanitizer_core.ifw", SANITIZER_AXW))
}

/// Sanitize arbitrary untrusted HTML into clean, structured UTF-8 text with zero executable JS.
pub fn sanitize_html(raw: &str) -> SanitizedDoc {
    let (engine, module) = compiled_module();
    let mut store = Store::new(engine, ());
    if store.set_fuel(SANITIZER_FUEL).is_err() {
        return SanitizedDoc::default();
    }
    let linker = Linker::new(engine);
    let Ok(inst_pre) = linker.instantiate(&mut store, module) else {
        return SanitizedDoc::default();
    };
    let Ok(inst) = inst_pre.start(&mut store) else {
        return SanitizedDoc::default();
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return SanitizedDoc::default();
    };
    if crate::core_mem::check_core_abi(&inst, &mut store, SANITIZER_CORE_ABI, "sanitizer_core")
        .is_err()
    {
        return SanitizedDoc::default();
    }
    let Ok(in_buf_word) = crate::core_mem::get_word_export(&inst, &mut store, "get_in_buf_word")
    else {
        return SanitizedDoc::default();
    };
    let Ok(out_buf_word) = crate::core_mem::get_word_export(&inst, &mut store, "get_out_buf_word")
    else {
        return SanitizedDoc::default();
    };
    let Ok(scripts_word) =
        crate::core_mem::get_word_export(&inst, &mut store, "get_stripped_scripts_word")
    else {
        return SanitizedDoc::default();
    };
    let Ok(iframes_word) =
        crate::core_mem::get_word_export(&inst, &mut store, "get_stripped_iframes_word")
    else {
        return SanitizedDoc::default();
    };
    let Ok(tags_word) =
        crate::core_mem::get_word_export(&inst, &mut store, "get_stripped_tags_word")
    else {
        return SanitizedDoc::default();
    };
    let Ok(func) = inst.get_typed_func::<i32, i32>(&store, "sanitize_html_ifm") else {
        return SanitizedDoc::default();
    };

    let n_bytes = {
        let mem = memory.data_mut(&mut store);
        crate::core_mem::write_utf8_bytes(mem, in_buf_word * 4, raw, MAX_IN_BYTES)
    };

    let Ok(out_len_raw) = func.call(&mut store, n_bytes as i32) else {
        return SanitizedDoc::default();
    };
    let out_len = (out_len_raw.max(0) as usize).min(MAX_OUT_BYTES);
    let mem = memory.data(&store);
    let clean_text = crate::core_mem::read_utf8_bytes(mem, out_buf_word * 4, out_len);

    SanitizedDoc {
        clean_text,
        stripped_scripts: crate::core_mem::get_i32(mem, scripts_word).max(0) as usize,
        stripped_iframes: crate::core_mem::get_i32(mem, iframes_word).max(0) as usize,
        stripped_tags: crate::core_mem::get_i32(mem, tags_word).max(0) as usize,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strips_scripts_iframes_and_preserves_clean_content() {
        let html = r#"
            <html><head><style>body{color:red}</style>
            <script>alert("xss"); fetch("https://spy.evil")</script></head>
            <body>
              <h1>Infimum Clean Reader</h1>
              <!-- hidden comment -->
              <p onclick="steal()">Zero JS &amp; 100% safe &mdash; Привет!</p>
              <iframe src="https://ads.evil/track"></iframe>
              <ul><li>Item One</li><li>Item Two</li></ul>
            </body></html>
        "#;
        let doc = sanitize_html(html);
        assert_eq!(doc.stripped_scripts, 1);
        assert_eq!(doc.stripped_iframes, 1);
        assert!(!doc.clean_text.contains("alert"));
        assert!(!doc.clean_text.contains("steal"));
        assert!(!doc.clean_text.contains("ads.evil"));
        assert!(!doc.clean_text.contains("hidden comment"));
        assert!(doc.clean_text.contains("# Infimum Clean Reader"));
        assert!(doc.clean_text.contains("Zero JS & 100% safe — Привет!"));
        assert!(doc.clean_text.contains("• Item One"));
        assert!(doc.clean_text.contains("• Item Two"));
    }
}
