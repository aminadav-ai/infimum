//! Local zero-JS HTML sanitizer (`Legacy HTML -> Clean Structured Text`)
//! powered by the Z3-verified Infimum Lang module `capsules/core/sanitizer_core.ifw`.
//!
//! Strips `<script>`, `<style>`, `<iframe>`, `<svg>`, `<noscript>`, HTML comments,
//! and all attributes/event handlers before a single byte reaches a capsule.

use std::sync::OnceLock;
use wasmi::{Engine, Linker, Module, Store};

const SANITIZER_AXW: &[u8] = include_bytes!("../capsules/core/sanitizer_core.ifw");
const MAX_IN_CHARS: usize = 65_536;
const MAX_OUT_CHARS: usize = 65_536;
const IN_BUF_WORD: usize = 64;
const OUT_BUF_WORD: usize = 65_600;
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

fn read_word(mem: &[u8], word: usize) -> i32 {
    let off = word * 4;
    if off + 4 <= mem.len() {
        i32::from_le_bytes(mem[off..off + 4].try_into().unwrap())
    } else {
        0
    }
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
    let Ok(func) = inst.get_typed_func::<i32, i32>(&store, "sanitize_html_ifm") else {
        return SanitizedDoc::default();
    };

    let chars: Vec<u32> = raw.chars().take(MAX_IN_CHARS).map(|c| c as u32).collect();
    {
        let mem = memory.data_mut(&mut store);
        for (idx, &cp) in chars.iter().enumerate() {
            let off = (IN_BUF_WORD + idx) * 4;
            if off + 4 <= mem.len() {
                mem[off..off + 4].copy_from_slice(&cp.to_le_bytes());
            }
        }
    }

    let Ok(out_len_raw) = func.call(&mut store, chars.len() as i32) else {
        return SanitizedDoc::default();
    };
    let out_len = (out_len_raw.max(0) as usize).min(MAX_OUT_CHARS);
    let mem = memory.data(&store);
    let mut clean_text = String::with_capacity(out_len);
    for idx in 0..out_len {
        let cp = read_word(mem, OUT_BUF_WORD + idx) as u32;
        if let Some(ch) = char::from_u32(cp) {
            clean_text.push(ch);
        }
    }

    SanitizedDoc {
        clean_text,
        stripped_scripts: read_word(mem, 1).max(0) as usize,
        stripped_iframes: read_word(mem, 2).max(0) as usize,
        stripped_tags: read_word(mem, 3).max(0) as usize,
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
