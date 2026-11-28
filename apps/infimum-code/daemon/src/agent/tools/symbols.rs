//! Multi-language symbol extraction, outline, AST-backed symbol replacement, and reference search.
//! - Rust (`.rs`): real `syn` AST with `proc-macro2` span locations (top-level items + `Type::method`
//!   inside `impl` blocks, preserving leading `#[...]` attributes/doc-comments), with automatic
//!   heuristic fallback when a file has temporary syntax errors.
//! - JavaScript/TypeScript (`.js`, `.mjs`, `.cjs`, `.ts`, `.tsx`, `.jsx`): state-machine lexer
//!   handling comments, strings, nested template literals `` `...${...}...` ``, and regex literals,
//!   extracting functions, arrow functions, classes + `Class::method`, interfaces, types, and enums.
//! - Infimum Lang (`.ifm`): Z3-verified `outline_core.ifw` + declaration span parser.

use serde_json::{json, Value};
use std::fs;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;
use syn::spanned::Spanned;

use super::sandbox::{glob_matches, is_jailed_in, resolve_in_project};

pub(in crate::agent) const OUTLINE_MAX_ENTRIES: usize = 400;
pub(in crate::agent) const OUTLINE_MAX_LINE_CHARS: usize = 300;
pub(in crate::agent) const OUTLINE_CORE_AXW: &[u8] =
    include_bytes!("../../../../cores/outline_core.ifw");

fn outline_core_module() -> &'static (wasmi::Engine, wasmi::Module) {
    static CELL: OnceLock<(wasmi::Engine, wasmi::Module)> = OnceLock::new();
    CELL.get_or_init(|| crate::core_loader::load_core_axw("outline_core.ifw", OUTLINE_CORE_AXW))
}

fn clamp_outline_line(s: &str) -> String {
    if s.chars().count() <= OUTLINE_MAX_LINE_CHARS {
        s.to_string()
    } else {
        let mut out: String = s.chars().take(OUTLINE_MAX_LINE_CHARS - 1).collect();
        out.push('…');
        out
    }
}

fn extract_outline_via_infimum(src: &str, is_rs: bool) -> (Vec<String>, bool) {
    let (engine, module) = outline_core_module();
    let mut store = wasmi::Store::new(engine, ());
    if store.set_fuel(120_000_000).is_err() {
        return (Vec::new(), false);
    }
    let linker = wasmi::Linker::new(engine);
    let Ok(inst) = linker
        .instantiate(&mut store, module)
        .and_then(|pre| pre.start(&mut store))
    else {
        return (Vec::new(), false);
    };
    let Some(memory) = inst.get_memory(&store, "memory") else {
        return (Vec::new(), false);
    };
    if crate::core_loader::check_core_abi(&inst, &mut store, 1, "outline_core").is_err() {
        return (Vec::new(), false);
    }
    let Ok(src_buf_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_src_buf_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(src_len_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_src_len_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(is_rust_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_is_rust_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(out_buf_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_out_buf_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(out_len_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_out_len_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(trunc_word) =
        crate::core_loader::get_word_export(&inst, &mut store, "get_truncated_word")
    else {
        return (Vec::new(), false);
    };
    let Ok(func) = inst.get_typed_func::<(), i32>(&store, "extract_outline_ifm") else {
        return (Vec::new(), false);
    };
    let max_in: usize = 240_000;
    let mut in_len = 0usize;
    {
        let mem = memory.data_mut(&mut store);
        for ch in src.chars() {
            if in_len >= max_in {
                break;
            }
            crate::core_loader::put_i32(mem, src_buf_word + in_len, ch as i32);
            in_len += 1;
        }
        crate::core_loader::put_i32(mem, src_len_word, in_len as i32);
        crate::core_loader::put_i32(mem, is_rust_word, if is_rs { 1 } else { 0 });
    }
    let Ok(entries_count) = func.call(&mut store, ()) else {
        return (Vec::new(), false);
    };
    let mem = memory.data(&store);
    let out_len = crate::core_loader::get_i32(mem, out_len_word).clamp(0, 131_072) as usize;
    let trunc = crate::core_loader::get_i32(mem, trunc_word) != 0;
    let mut text = String::with_capacity(out_len);
    for i in 0..out_len {
        let cp = crate::core_loader::get_i32(mem, out_buf_word + 1 + i) as u32;
        if let Some(ch) = char::from_u32(cp) {
            text.push(ch);
        }
    }
    let entries: Vec<String> = if entries_count <= 0 || text.is_empty() {
        Vec::new()
    } else {
        text.lines().map(|l| l.to_string()).collect()
    };
    (entries, trunc)
}

pub(in crate::agent) fn extract_ifm_outline(src: &str) -> (Vec<String>, bool) {
    extract_outline_via_infimum(src, false)
}

#[derive(Debug, Clone)]
pub(in crate::agent) struct IfmSymbolSpan {
    pub(in crate::agent) kind: String,
    pub(in crate::agent) name: String,
    pub(in crate::agent) start_line: usize, // 1-based inclusive
    pub(in crate::agent) end_line: usize,   // 1-based inclusive
}

fn take_ident(s: &str) -> Option<String> {
    let mut out = String::new();
    for ch in s.chars() {
        if ch.is_ascii_alphanumeric() || ch == '_' || ch == '$' {
            out.push(ch);
        } else {
            break;
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Extract clean primary type name from a `syn::Type` (e.g., `SseTurnAccumulator` from
/// `SseTurnAccumulator<'a, T>` or `Foo::Bar`).
fn syn_type_short_name(ty: &syn::Type) -> String {
    match ty {
        syn::Type::Path(tp) => tp
            .path
            .segments
            .last()
            .map(|seg| seg.ident.to_string())
            .unwrap_or_else(|| "impl".to_string()),
        syn::Type::Reference(tr) => syn_type_short_name(&tr.elem),
        syn::Type::Group(tg) => syn_type_short_name(&tg.elem),
        syn::Type::Paren(tp) => syn_type_short_name(&tp.elem),
        _ => "impl".to_string(),
    }
}

fn syn_item_start_line(attrs: &[syn::Attribute], fallback_start: usize) -> usize {
    let mut start = fallback_start;
    if let Some(first) = attrs.first() {
        let al = first.span().start().line;
        if al > 0 && al < start {
            start = al;
        }
    }
    start
}

fn compact_signature_from_lines(lines: &[&str], decl_line_1based: usize, max_scan: usize) -> String {
    let start_idx = decl_line_1based.saturating_sub(1).min(lines.len());
    let mut sig = String::new();
    for &l in lines.iter().skip(start_idx).take(max_scan) {
        let t = l.trim();
        if t.is_empty() || t.starts_with("//") || t.starts_with("#[") {
            continue;
        }
        if !sig.is_empty() {
            sig.push(' ');
        }
        if let Some(brace_pos) = t.find('{') {
            sig.push_str(t[..brace_pos].trim_end());
            break;
        }
        if let Some(semi_pos) = t.find(';') {
            sig.push_str(t[..semi_pos].trim_end());
            break;
        }
        sig.push_str(t);
    }
    sig.trim().to_string()
}

/// Parse Rust `.rs` file via real `syn` AST with `proc-macro2` span locations.
/// Returns `Ok((spans, outline_entries, truncated))` on valid Rust syntax, or `Err` if the file
/// currently has syntax errors (so callers can fall back to the heuristic scanner).
fn parse_rs_with_syn(src: &str) -> Result<(Vec<IfmSymbolSpan>, Vec<String>, bool), String> {
    let file = syn::parse_file(src).map_err(|e| e.to_string())?;
    let lines: Vec<&str> = src.lines().collect();
    let total_lines = lines.len().max(1);
    let mut spans: Vec<IfmSymbolSpan> = Vec::new();
    let mut outline: Vec<String> = Vec::new();
    let mut truncated = false;

    let mut push_outline = |entry: String| {
        if outline.len() < OUTLINE_MAX_ENTRIES {
            outline.push(clamp_outline_line(&entry));
        } else {
            truncated = true;
        }
    };

    for item in &file.items {
        match item {
            syn::Item::Fn(f) => {
                let raw_start = f.sig.span().start().line.max(1);
                let start = syn_item_start_line(&f.attrs, f.span().start().line.max(1));
                let end = f.span().end().line.clamp(start, total_lines);
                let name = f.sig.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 6);
                let label = if sig.is_empty() {
                    format!("fn {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "fn".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Struct(s) => {
                let raw_start = s.struct_token.span.start().line.max(1);
                let start = syn_item_start_line(&s.attrs, s.span().start().line.max(1));
                let end = s.span().end().line.clamp(start, total_lines);
                let name = s.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 3);
                let label = if sig.is_empty() {
                    format!("struct {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "struct".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Enum(e) => {
                let raw_start = e.enum_token.span.start().line.max(1);
                let start = syn_item_start_line(&e.attrs, e.span().start().line.max(1));
                let end = e.span().end().line.clamp(start, total_lines);
                let name = e.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 3);
                let label = if sig.is_empty() {
                    format!("enum {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "enum".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Trait(t) => {
                let raw_start = t.trait_token.span.start().line.max(1);
                let start = syn_item_start_line(&t.attrs, t.span().start().line.max(1));
                let end = t.span().end().line.clamp(start, total_lines);
                let name = t.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 3);
                let label = if sig.is_empty() {
                    format!("trait {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "trait".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Type(t) => {
                let raw_start = t.type_token.span.start().line.max(1);
                let start = syn_item_start_line(&t.attrs, t.span().start().line.max(1));
                let end = t.span().end().line.clamp(start, total_lines);
                let name = t.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 2);
                let label = if sig.is_empty() {
                    format!("type {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "type".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Const(c) => {
                let raw_start = c.const_token.span.start().line.max(1);
                let start = syn_item_start_line(&c.attrs, c.span().start().line.max(1));
                let end = c.span().end().line.clamp(start, total_lines);
                let name = c.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 2);
                let label = if sig.is_empty() {
                    format!("const {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "const".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Static(st) => {
                let raw_start = st.static_token.span.start().line.max(1);
                let start = syn_item_start_line(&st.attrs, st.span().start().line.max(1));
                let end = st.span().end().line.clamp(start, total_lines);
                let name = st.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 2);
                let label = if sig.is_empty() {
                    format!("static {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "static".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Mod(m) => {
                let raw_start = m.mod_token.span.start().line.max(1);
                let start = syn_item_start_line(&m.attrs, m.span().start().line.max(1));
                let end = m.span().end().line.clamp(start, total_lines);
                let name = m.ident.to_string();
                let sig = compact_signature_from_lines(&lines, raw_start, 2);
                let label = if sig.is_empty() {
                    format!("mod {name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "mod".to_string(),
                    name,
                    start_line: start,
                    end_line: end,
                });
            }
            syn::Item::Macro(mac) => {
                if let Some(id) = &mac.ident {
                    let raw_start = mac.span().start().line.max(1);
                    let start = syn_item_start_line(&mac.attrs, raw_start);
                    let end = mac.span().end().line.clamp(start, total_lines);
                    let name = id.to_string();
                    push_outline(format!("L{start}..L{end}: macro_rules! {name}"));
                    spans.push(IfmSymbolSpan {
                        kind: "macro".to_string(),
                        name,
                        start_line: start,
                        end_line: end,
                    });
                }
            }
            syn::Item::Impl(imp) => {
                let raw_start = imp.impl_token.span.start().line.max(1);
                let start = syn_item_start_line(&imp.attrs, imp.span().start().line.max(1));
                let end = imp.span().end().line.clamp(start, total_lines);
                let ty_name = syn_type_short_name(&imp.self_ty);
                let sig = compact_signature_from_lines(&lines, raw_start, 3);
                let label = if sig.is_empty() {
                    format!("impl {ty_name}")
                } else {
                    sig
                };
                push_outline(format!("L{start}..L{end}: {label}"));
                spans.push(IfmSymbolSpan {
                    kind: "impl".to_string(),
                    name: ty_name.clone(),
                    start_line: start,
                    end_line: end,
                });
                // Also extract every method inside `impl` as `Type::method`!
                for impl_item in &imp.items {
                    if let syn::ImplItem::Fn(m) = impl_item {
                        let m_raw_start = m.sig.span().start().line.max(1);
                        let m_start =
                            syn_item_start_line(&m.attrs, m.span().start().line.max(1));
                        let m_end = m.span().end().line.clamp(m_start, total_lines);
                        let m_name = m.sig.ident.to_string();
                        let m_sig = compact_signature_from_lines(&lines, m_raw_start, 5);
                        let m_label = if m_sig.is_empty() {
                            format!("fn {m_name}")
                        } else {
                            m_sig
                        };
                        push_outline(format!("  L{m_start}..L{m_end}: {m_label}"));
                        spans.push(IfmSymbolSpan {
                            kind: "method".to_string(),
                            name: format!("{ty_name}::{m_name}"),
                            start_line: m_start,
                            end_line: m_end,
                        });
                    }
                }
            }
            _ => {}
        }
    }
    Ok((spans, outline, truncated))
}

pub(in crate::agent) fn extract_rs_outline(src: &str) -> (Vec<String>, bool) {
    if let Ok((_, outline, truncated)) = parse_rs_with_syn(src) {
        return (outline, truncated);
    }
    extract_outline_via_infimum(src, true)
}

/// Parse top-level Infimum declarations (`fn`, `export fn`, `pub fn`, `struct`, `enum`,
/// `layout`, `forma`, `const`) with exact 1-based start..end line ranges.
pub(in crate::agent) fn parse_ifm_symbol_spans(src: &str) -> Vec<IfmSymbolSpan> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<IfmSymbolSpan> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    while i < lines.len() {
        let raw = lines[i];
        let trimmed = raw.trim();
        let code_only = trimmed.split("//").next().unwrap_or("");
        if depth == 0 && !code_only.is_empty() {
            let mut rest = code_only.trim_start();
            for prefix in ["export ", "pub(super) ", "pub(crate) ", "pub ", "async "] {
                if let Some(r) = rest.strip_prefix(prefix) {
                    rest = r.trim_start();
                }
            }
            let decl: Option<(&str, &str)> = if let Some(r) = rest.strip_prefix("fn ") {
                Some(("fn", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("struct ") {
                Some(("struct", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("enum ") {
                Some(("enum", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("layout ") {
                Some(("layout", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("forma ") {
                Some(("forma", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("const ") {
                Some(("const", r.trim_start()))
            } else {
                None
            };
            if let Some((kind, after_kw)) = decl {
                if let Some(name) = take_ident(after_kw) {
                    let start_line = i + 1;
                    let mut end_line = start_line;
                    if kind == "const" && code_only.contains(';') && !code_only.contains('{') {
                        out.push(IfmSymbolSpan {
                            kind: kind.to_string(),
                            name,
                            start_line,
                            end_line,
                        });
                    } else {
                        let mut cur_d: i32 = 0;
                        let mut saw_open = false;
                        let mut j = i;
                        while j < lines.len() {
                            let c = lines[j].split("//").next().unwrap_or("");
                            let mut in_str = false;
                            let mut prev_esc = false;
                            for ch in c.chars() {
                                if in_str {
                                    if ch == '"' && !prev_esc {
                                        in_str = false;
                                    }
                                    prev_esc = ch == '\\' && !prev_esc;
                                } else {
                                    match ch {
                                        '"' => {
                                            in_str = true;
                                            prev_esc = false;
                                        }
                                        '{' => {
                                            cur_d += 1;
                                            saw_open = true;
                                        }
                                        '}' => {
                                            cur_d -= 1;
                                        }
                                        _ => {}
                                    }
                                }
                            }
                            end_line = j + 1;
                            if (saw_open && cur_d <= 0) || (!saw_open && c.trim_end().ends_with(';'))
                            {
                                break;
                            }
                            j += 1;
                        }
                        out.push(IfmSymbolSpan {
                            kind: kind.to_string(),
                            name,
                            start_line,
                            end_line,
                        });
                        i = end_line;
                        depth = 0;
                        continue;
                    }
                }
            }
        }
        let mut in_str = false;
        let mut prev_esc = false;
        for ch in code_only.chars() {
            if in_str {
                if ch == '"' && !prev_esc {
                    in_str = false;
                }
                prev_esc = ch == '\\' && !prev_esc;
            } else {
                match ch {
                    '"' => {
                        in_str = true;
                        prev_esc = false;
                    }
                    '{' => depth += 1,
                    '}' => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
        }
        i += 1;
    }
    out
}

/// Heuristic fallback parser for Rust `.rs` when the file currently fails `syn::parse_file`.
fn parse_rs_symbol_spans_heuristic(src: &str) -> Vec<IfmSymbolSpan> {
    let lines: Vec<&str> = src.lines().collect();
    let mut out: Vec<IfmSymbolSpan> = Vec::new();
    let mut depth: i32 = 0;
    let mut i = 0usize;
    while i < lines.len() {
        let trimmed = lines[i].trim();
        let code_only = trimmed.split("//").next().unwrap_or("");
        if depth == 0 && !code_only.is_empty() && !code_only.starts_with('#') {
            let mut rest = code_only.trim_start();
            for _ in 0..4 {
                if rest.starts_with("pub(") {
                    if let Some(close) = rest.find(')') {
                        rest = rest[close + 1..].trim_start();
                        continue;
                    }
                }
                let mut stripped = false;
                for prefix in ["pub ", "async ", "unsafe ", "extern \"C\" ", "const "] {
                    if prefix == "const " && !rest.starts_with("const fn ") {
                        continue;
                    }
                    if let Some(r) = rest.strip_prefix(prefix) {
                        rest = r.trim_start();
                        stripped = true;
                        break;
                    }
                }
                if !stripped {
                    break;
                }
            }
            let decl: Option<(&str, &str)> = if let Some(r) = rest.strip_prefix("fn ") {
                Some(("fn", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("struct ") {
                Some(("struct", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("enum ") {
                Some(("enum", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("trait ") {
                Some(("trait", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("type ") {
                Some(("type", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("const ") {
                Some(("const", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("static ") {
                Some(("static", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("mod ") {
                Some(("mod", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("impl ") {
                Some(("impl", r.trim_start()))
            } else {
                None
            };
            if let Some((kind, after_kw)) = decl {
                let name_opt = if kind == "impl" {
                    let s = after_kw
                        .split('{')
                        .next()
                        .unwrap_or(after_kw)
                        .split(" where ")
                        .next()
                        .unwrap_or(after_kw)
                        .trim();
                    if s.is_empty() {
                        None
                    } else {
                        Some(s.to_string())
                    }
                } else {
                    take_ident(after_kw)
                };
                if let Some(name) = name_opt {
                    let start_line = i + 1;
                    let mut end_line = start_line;
                    let mut cur_d: i32 = 0;
                    let mut saw_open = false;
                    let mut j = i;
                    while j < lines.len() {
                        let c = lines[j].split("//").next().unwrap_or("");
                        let mut in_str = false;
                        let mut prev_esc = false;
                        for ch in c.chars() {
                            if in_str {
                                if ch == '"' && !prev_esc {
                                    in_str = false;
                                }
                                prev_esc = ch == '\\' && !prev_esc;
                            } else {
                                match ch {
                                    '"' => {
                                        in_str = true;
                                        prev_esc = false;
                                    }
                                    '{' => {
                                        cur_d += 1;
                                        saw_open = true;
                                    }
                                    '}' => cur_d -= 1,
                                    _ => {}
                                }
                            }
                        }
                        end_line = j + 1;
                        if (saw_open && cur_d <= 0) || (!saw_open && c.trim_end().ends_with(';')) {
                            break;
                        }
                        j += 1;
                    }
                    out.push(IfmSymbolSpan {
                        kind: kind.to_string(),
                        name,
                        start_line,
                        end_line,
                    });
                    i = end_line;
                    depth = 0;
                    continue;
                }
            }
        }
        let mut in_str = false;
        let mut prev_esc = false;
        for ch in code_only.chars() {
            if in_str {
                if ch == '"' && !prev_esc {
                    in_str = false;
                }
                prev_esc = ch == '\\' && !prev_esc;
            } else {
                match ch {
                    '"' => {
                        in_str = true;
                        prev_esc = false;
                    }
                    '{' => depth += 1,
                    '}' => depth = (depth - 1).max(0),
                    _ => {}
                }
            }
        }
        i += 1;
    }
    out
}

pub(in crate::agent) fn parse_rs_symbol_spans(src: &str) -> Vec<IfmSymbolSpan> {
    if let Ok((spans, _, _)) = parse_rs_with_syn(src) {
        return spans;
    }
    parse_rs_symbol_spans_heuristic(src)
}

/// State-machine lexer for JS/TS that computes per-line brace depth changes and masks out
/// strings (`'...'`, `"..."`), nested template literals (`` `...${...}...` ``), comments
/// (`//`, `/* ... */`), and regex literals so brace tracking is never fooled by literal braces.
struct JsLineInfo {
    depth_before: i32,
    depth_after: i32,
    code: String, // line content with string/comment/regex interiors blanked
}

fn lex_js_lines(src: &str) -> Vec<JsLineInfo> {
    #[derive(Clone, Copy, PartialEq)]
    enum Mode {
        Code,
        SingleQuote,
        DoubleQuote,
        BlockComment,
        TemplateText,
    }

    let mut mode = Mode::Code;
    let mut template_expr_brace_stack: Vec<i32> = Vec::new();
    let mut depth: i32 = 0;
    let mut result: Vec<JsLineInfo> = Vec::new();

    for line in src.lines() {
        let depth_before = depth;
        let chars: Vec<char> = line.chars().collect();
        let mut code = String::with_capacity(line.len());
        let mut i = 0usize;
        let mut prev_sig: char = ';'; // helps distinguish regex literal `/` from division

        while i < chars.len() {
            let ch = chars[i];
            match mode {
                Mode::Code => {
                    if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        break; // line comment
                    }
                    if ch == '/' && i + 1 < chars.len() && chars[i + 1] == '*' {
                        mode = Mode::BlockComment;
                        i += 2;
                        continue;
                    }
                    if ch == '\'' {
                        mode = Mode::SingleQuote;
                        code.push(' ');
                        prev_sig = '\'';
                        i += 1;
                        continue;
                    }
                    if ch == '"' {
                        mode = Mode::DoubleQuote;
                        code.push(' ');
                        prev_sig = '"';
                        i += 1;
                        continue;
                    }
                    if ch == '`' {
                        mode = Mode::TemplateText;
                        code.push(' ');
                        prev_sig = '`';
                        i += 1;
                        continue;
                    }
                    // Heuristic for regex literal `/pattern/flags`: allowed after operator/punct/start
                    if ch == '/'
                        && matches!(
                            prev_sig,
                            '(' | '[' | '{' | ',' | ';' | ':' | '=' | '!' | '?' | '&' | '|' | '~' | '^' | '+' | '-' | '*'
                        )
                    {
                        let mut j = i + 1;
                        let mut in_class = false;
                        let mut closed = false;
                        while j < chars.len() {
                            let rc = chars[j];
                            if rc == '\\' {
                                j += 2;
                                continue;
                            }
                            if rc == '[' {
                                in_class = true;
                            } else if rc == ']' {
                                in_class = false;
                            } else if rc == '/' && !in_class {
                                closed = true;
                                j += 1;
                                break;
                            }
                            j += 1;
                        }
                        if closed {
                            while j < chars.len() && chars[j].is_ascii_alphabetic() {
                                j += 1;
                            }
                            code.push(' ');
                            prev_sig = '/';
                            i = j;
                            continue;
                        }
                    }
                    if ch == '{' {
                        depth += 1;
                        code.push(ch);
                        prev_sig = '{';
                    } else if ch == '}' {
                        if let Some(&tpl_depth) = template_expr_brace_stack.last() {
                            if depth == tpl_depth {
                                template_expr_brace_stack.pop();
                                depth = (depth - 1).max(0);
                                mode = Mode::TemplateText;
                                code.push(' ');
                                i += 1;
                                continue;
                            }
                        }
                        depth = (depth - 1).max(0);
                        code.push(ch);
                        prev_sig = '}';
                    } else {
                        code.push(ch);
                        if !ch.is_whitespace() {
                            prev_sig = ch;
                        }
                    }
                    i += 1;
                }
                Mode::SingleQuote => {
                    if ch == '\\' {
                        i += 2;
                        continue;
                    }
                    if ch == '\'' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
                Mode::DoubleQuote => {
                    if ch == '\\' {
                        i += 2;
                        continue;
                    }
                    if ch == '"' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
                Mode::BlockComment => {
                    if ch == '*' && i + 1 < chars.len() && chars[i + 1] == '/' {
                        mode = Mode::Code;
                        i += 2;
                        continue;
                    }
                    i += 1;
                }
                Mode::TemplateText => {
                    if ch == '\\' {
                        i += 2;
                        continue;
                    }
                    if ch == '$' && i + 1 < chars.len() && chars[i + 1] == '{' {
                        depth += 1;
                        template_expr_brace_stack.push(depth);
                        mode = Mode::Code;
                        prev_sig = '{';
                        i += 2;
                        continue;
                    }
                    if ch == '`' {
                        mode = Mode::Code;
                    }
                    i += 1;
                }
            }
        }
        // Unclosed single/double quote cannot span a line in JS without trailing backslash
        if matches!(mode, Mode::SingleQuote | Mode::DoubleQuote) && !line.ends_with('\\') {
            mode = Mode::Code;
        }
        result.push(JsLineInfo {
            depth_before,
            depth_after: depth,
            code,
        });
    }
    result
}

/// Find the 1-based end line of a JS/TS block starting at 0-based `start_idx` using pre-lexed depths.
fn js_block_end_line(lexed: &[JsLineInfo], start_idx: usize, base_depth: i32) -> usize {
    let mut saw_open = false;
    for (j, info) in lexed.iter().enumerate().skip(start_idx) {
        if info.code.contains('{') || info.depth_after > base_depth {
            saw_open = true;
        }
        if saw_open && info.depth_after <= base_depth {
            return j + 1;
        }
        if !saw_open && info.code.trim_end().ends_with(';') {
            return j + 1;
        }
    }
    lexed.len().max(start_idx + 1)
}

/// Parse top-level JS/TS declarations (`function`, `class` + `Class::method`, `interface`, `type`,
/// `enum`, `const/let/var name = (...) =>` or `= function`) with accurate token-level brace depth.
pub(in crate::agent) fn parse_js_symbol_spans(src: &str) -> Vec<IfmSymbolSpan> {
    let lexed = lex_js_lines(src);
    let mut out: Vec<IfmSymbolSpan> = Vec::new();
    let mut i = 0usize;

    while i < lexed.len() {
        let info = &lexed[i];
        let trimmed = info.code.trim();
        if info.depth_before == 0 && !trimmed.is_empty() {
            let mut rest = trimmed;
            for prefix in ["export default ", "export ", "declare ", "abstract ", "async "] {
                if let Some(r) = rest.strip_prefix(prefix) {
                    rest = r.trim_start();
                }
            }
            let decl: Option<(&str, &str)> = if let Some(r) = rest.strip_prefix("function* ") {
                Some(("function", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("function ") {
                Some(("function", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("class ") {
                Some(("class", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("interface ") {
                Some(("interface", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("type ") {
                Some(("type", r.trim_start()))
            } else if let Some(r) = rest.strip_prefix("enum ") {
                Some(("enum", r.trim_start()))
            } else if rest.starts_with("const ")
                || rest.starts_with("let ")
                || rest.starts_with("var ")
            {
                let after = rest.split_once(' ').map(|(_, b)| b.trim_start()).unwrap_or("");
                if rest.contains("=>") || rest.contains("function") || rest.contains('{') {
                    Some(("const", after))
                } else {
                    None
                }
            } else {
                None
            };

            if let Some((kind, after_kw)) = decl {
                if let Some(name) = take_ident(after_kw) {
                    let start_line = i + 1;
                    let end_line = js_block_end_line(&lexed, i, 0);
                    out.push(IfmSymbolSpan {
                        kind: kind.to_string(),
                        name: name.clone(),
                        start_line,
                        end_line,
                    });
                    // For `class`, also extract class methods at depth 1 as `Class::method`
                    if kind == "class" && end_line > start_line {
                        let mut m = i + 1;
                        while m < end_line.min(lexed.len()) {
                            let minfo = &lexed[m];
                            let mt = minfo.code.trim();
                            if minfo.depth_before == 1 && !mt.is_empty() && !mt.starts_with('}') {
                                let mut mrest = mt;
                                for kw in [
                                    "public ",
                                    "private ",
                                    "protected ",
                                    "readonly ",
                                    "static ",
                                    "async ",
                                    "get ",
                                    "set ",
                                    "override ",
                                ] {
                                    if let Some(r) = mrest.strip_prefix(kw) {
                                        mrest = r.trim_start();
                                    }
                                }
                                if let Some(mname) = take_ident(mrest) {
                                    let after_name = mrest[mname.len()..].trim_start();
                                    let is_method = after_name.starts_with('(')
                                        || after_name.starts_with('<')
                                        || (after_name.starts_with('=') && after_name.contains("=>"));
                                    if is_method
                                        && !matches!(
                                            mname.as_str(),
                                            "if" | "for" | "while" | "switch" | "catch" | "return"
                                        )
                                    {
                                        let m_start = m + 1;
                                        let m_end = js_block_end_line(&lexed, m, 1);
                                        out.push(IfmSymbolSpan {
                                            kind: "method".to_string(),
                                            name: format!("{name}::{mname}"),
                                            start_line: m_start,
                                            end_line: m_end,
                                        });
                                        m = m_end;
                                        continue;
                                    }
                                }
                            }
                            m += 1;
                        }
                    }
                    i = end_line;
                    continue;
                }
            }
        }
        i += 1;
    }
    out
}

pub(in crate::agent) fn extract_js_outline(src: &str) -> (Vec<String>, bool) {
    let spans = parse_js_symbol_spans(src);
    let lines: Vec<&str> = src.lines().collect();
    let mut entries = Vec::new();
    let truncated = spans.len() > OUTLINE_MAX_ENTRIES;
    for s in spans.iter().take(OUTLINE_MAX_ENTRIES) {
        let header = lines
            .get(s.start_line.saturating_sub(1))
            .copied()
            .unwrap_or("")
            .trim()
            .trim_end_matches('{')
            .trim();
        let prefix = if s.kind == "method" { "  " } else { "" };
        entries.push(clamp_outline_line(&format!(
            "{prefix}L{}..L{}: {header}",
            s.start_line, s.end_line
        )));
    }
    (entries, truncated)
}

pub(in crate::agent) fn parse_code_symbol_spans(src: &str, ext: &str) -> Vec<IfmSymbolSpan> {
    match ext {
        "ifm" => parse_ifm_symbol_spans(src),
        "rs" => parse_rs_symbol_spans(src),
        "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx" => parse_js_symbol_spans(src),
        _ => parse_ifm_symbol_spans(src),
    }
}

pub(in crate::agent) fn line_contains_ident(line: &str, ident: &str) -> bool {
    let bytes = line.as_bytes();
    let ib = ident.as_bytes();
    if ib.is_empty() || bytes.len() < ib.len() {
        return false;
    }
    let is_id = |b: u8| b.is_ascii_alphanumeric() || b == b'_';
    let mut pos = 0usize;
    while pos + ib.len() <= bytes.len() {
        if let Some(found) = line[pos..].find(ident) {
            let abs = pos + found;
            let left_ok = abs == 0 || !is_id(bytes[abs - 1]);
            let right_idx = abs + ib.len();
            let right_ok = right_idx >= bytes.len() || !is_id(bytes[right_idx]);
            if left_ok && right_ok {
                return true;
            }
            pos = abs + 1;
        } else {
            break;
        }
    }
    false
}

/// Resolve a requested symbol name against parsed spans.
/// Supports exact match (`foo` or `Type::method`) and unambiguous short method suffix (`method` -> `Type::method`).
fn resolve_symbol_span<'a>(
    spans: &'a [IfmSymbolSpan],
    sym: &str,
) -> Result<&'a IfmSymbolSpan, Vec<String>> {
    if let Some(hit) = spans.iter().find(|s| s.name == sym) {
        return Ok(hit);
    }
    if !sym.contains("::") {
        let suffix = format!("::{sym}");
        let suffix_hits: Vec<&IfmSymbolSpan> = spans
            .iter()
            .filter(|s| s.name.ends_with(&suffix))
            .collect();
        if suffix_hits.len() == 1 {
            return Ok(suffix_hits[0]);
        }
        if suffix_hits.len() > 1 {
            return Err(suffix_hits
                .iter()
                .map(|s| format!("{} {} (L{}-L{})", s.kind, s.name, s.start_line, s.end_line))
                .collect());
        }
    }
    Err(spans
        .iter()
        .take(60)
        .map(|s| format!("{} {} (L{}-L{})", s.kind, s.name, s.start_line, s.end_line))
        .collect())
}

pub(in crate::agent) fn tool_get_file_outline(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Access denied: path is outside active project sandbox or sensitive" });
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    if !matches!(
        ext.as_str(),
        "ifm" | "rs" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx"
    ) {
        return json!({
            "ok": false,
            "error": format!("get_file_outline supports .ifm, .rs, .js, .mjs, .ts, .tsx files (got '.{ext}')")
        });
    }
    match fs::read_to_string(&p) {
        Ok(txt) => {
            let total_lines = txt.lines().count();
            let (mut entries, truncated) = match ext.as_str() {
                "ifm" => extract_ifm_outline(&txt),
                "rs" => extract_rs_outline(&txt),
                _ => extract_js_outline(&txt),
            };
            let entries_count = entries.len();
            if truncated {
                entries.push(format!(
                    "... [truncated at {OUTLINE_MAX_ENTRIES} entries; use search_project or read_project_file for remaining items]"
                ));
            }
            json!({
                "ok": true,
                "path": p.display().to_string(),
                "total_lines": total_lines,
                "entries_count": entries_count,
                "truncated": truncated,
                "outline": entries.join("\n"),
            })
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_get_symbol(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Access denied outside active project sandbox" });
    }
    if sym.is_empty() {
        return json!({ "error": "Missing required 'name' parameter" });
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match fs::read_to_string(&p) {
        Ok(src) => {
            let ast_fallback = ext == "rs" && parse_rs_with_syn(&src).is_err();
            let spans = parse_code_symbol_spans(&src, &ext);
            match resolve_symbol_span(&spans, sym) {
                Ok(hit) => {
                    let lines: Vec<&str> = src.lines().collect();
                    let s_idx = hit.start_line.saturating_sub(1);
                    let e_idx = hit.end_line.min(lines.len());
                    let snippet = lines[s_idx..e_idx].join("\n");
                    json!({
                        "ok": true,
                        "path": p.display().to_string(),
                        "kind": hit.kind,
                        "name": hit.name,
                        "start_line": hit.start_line,
                        "end_line": hit.end_line,
                        "ast_fallback": ast_fallback,
                        "source": snippet,
                    })
                }
                Err(avail) => json!({
                    "error": format!("Symbol '{sym}' not found (or ambiguous) in {}", p.display()),
                    "available_symbols": avail,
                }),
            }
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

pub(in crate::agent) fn tool_replace_symbol(default_proj: &Path, args: &Value) -> Value {
    let raw = args.get("path").and_then(|s| s.as_str()).unwrap_or("");
    let sym = args.get("name").and_then(|s| s.as_str()).unwrap_or("").trim();
    let new_src = args.get("new_source").and_then(|s| s.as_str()).unwrap_or("");
    let p = resolve_in_project(default_proj, raw);
    if !is_jailed_in(&p, default_proj) {
        return json!({ "error": "Write denied: path is outside active project sandbox" });
    }
    if sym.is_empty() || new_src.trim().is_empty() {
        return json!({ "error": "Both 'name' and non-empty 'new_source' are required" });
    }
    let ext = p
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("")
        .to_ascii_lowercase();
    match fs::read_to_string(&p) {
        Ok(src) => {
            let spans = parse_code_symbol_spans(&src, &ext);
            match resolve_symbol_span(&spans, sym) {
                Ok(hit) => {
                    let lines: Vec<&str> = src.lines().collect();
                    let s_idx = hit.start_line.saturating_sub(1);
                    let e_idx = hit.end_line.min(lines.len());
                    let mut out_lines: Vec<String> = Vec::with_capacity(lines.len() + 16);
                    for l in &lines[..s_idx] {
                        out_lines.push((*l).to_string());
                    }
                    let new_lines: Vec<&str> = new_src.trim_end_matches('\n').lines().collect();
                    let new_cnt = new_lines.len();
                    for l in new_lines {
                        out_lines.push(l.to_string());
                    }
                    for l in &lines[e_idx..] {
                        out_lines.push((*l).to_string());
                    }
                    let mut updated = out_lines.join("\n");
                    if src.ends_with('\n') {
                        updated.push('\n');
                    }
                    match fs::write(&p, &updated) {
                        Ok(_) => json!({
                            "ok": true,
                            "path": p.display().to_string(),
                            "name": hit.name,
                            "replaced_lines": [hit.start_line, hit.end_line],
                            "new_line_count": new_cnt,
                        }),
                        Err(e) => json!({ "error": e.to_string() }),
                    }
                }
                Err(avail) => json!({
                    "error": format!("Symbol '{sym}' not found (or ambiguous) in {}", p.display()),
                    "available_symbols": avail,
                }),
            }
        }
        Err(e) => json!({ "error": e.to_string() }),
    }
}

/// Multi-language AST-aware reference/caller search across `.rs`, `.ifm`, `.js`, `.mjs`, `.ts`, `.tsx`.
/// When `ifm_only` is true (legacy `find_ifm_callers` without `path`/`glob`), scans `.ifm` files.
pub(in crate::agent) fn tool_find_references(
    default_proj: &Path,
    args: &Value,
    ifm_only_default: bool,
) -> Value {
    let sym = args
        .get("symbol")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    let opt_path = args
        .get("path")
        .and_then(|s| s.as_str())
        .unwrap_or("")
        .trim();
    let glob_pat = args
        .get("glob")
        .and_then(|s| s.as_str())
        .map(|s| s.trim())
        .filter(|s| !s.is_empty());
    let max_matches = args
        .get("max_matches")
        .and_then(|v| v.as_u64())
        .unwrap_or(60)
        .clamp(1, 200) as usize;

    if sym.is_empty() {
        return json!({ "error": "Missing required 'symbol' parameter" });
    }

    let is_code_ext = |ext: &str| -> bool {
        if ifm_only_default && glob_pat.is_none() {
            ext == "ifm"
        } else {
            matches!(
                ext,
                "ifm" | "rs" | "js" | "mjs" | "cjs" | "ts" | "tsx" | "jsx"
            )
        }
    };

    let mut files: Vec<PathBuf> = Vec::new();
    let scan_root = if !opt_path.is_empty() {
        let p = resolve_in_project(default_proj, opt_path);
        if !is_jailed_in(&p, default_proj) {
            return json!({ "error": "Access denied outside active project sandbox" });
        }
        if p.is_file() {
            files.push(p);
            None
        } else if p.is_dir() {
            Some(p)
        } else {
            return json!({ "error": format!("Path not found: {}", p.display()) });
        }
    } else {
        Some(default_proj.to_path_buf())
    };

    if let Some(root_dir) = scan_root {
        let mut stack = vec![root_dir];
        while let Some(dir) = stack.pop() {
            if files.len() >= 400 {
                break;
            }
            let Ok(rd) = fs::read_dir(&dir) else {
                continue;
            };
            for ent in rd.flatten() {
                let ep = ent.path();
                let fname = ent.file_name().to_string_lossy().to_string();
                if fname.starts_with('.')
                    || matches!(
                        fname.as_str(),
                        "target" | "node_modules" | "dist" | "__pycache__" | ".wrangler"
                    )
                {
                    continue;
                }
                if ep.is_dir() {
                    stack.push(ep);
                } else {
                    let ext = ep
                        .extension()
                        .and_then(|e| e.to_str())
                        .unwrap_or("")
                        .to_ascii_lowercase();
                    if !is_code_ext(&ext) {
                        continue;
                    }
                    if let Some(g) = glob_pat {
                        let rel = ep
                            .strip_prefix(default_proj)
                            .map(|r| r.display().to_string())
                            .unwrap_or_else(|_| ep.display().to_string());
                        if !glob_matches(g, &rel) {
                            continue;
                        }
                    }
                    files.push(ep);
                }
            }
        }
        files.sort();
    }

    let mut hits: Vec<Value> = Vec::new();
    for fp in &files {
        if hits.len() >= max_matches {
            break;
        }
        let Ok(src) = fs::read_to_string(fp) else {
            continue;
        };
        // Fast pre-filter before running AST parser on the file
        if !src.contains(sym) {
            continue;
        }
        let rel = fp
            .strip_prefix(default_proj)
            .map(|r| r.display().to_string())
            .unwrap_or_else(|_| fp.display().to_string());
        let ext = fp
            .extension()
            .and_then(|e| e.to_str())
            .unwrap_or("")
            .to_ascii_lowercase();
        let spans = parse_code_symbol_spans(&src, &ext);
        for (idx, line) in src.lines().enumerate() {
            if hits.len() >= max_matches {
                break;
            }
            let ln = idx + 1;
            if !line_contains_ident(line, sym) {
                continue;
            }
            // Pick the innermost enclosing span (e.g. `method` inside `impl`)
            let enclosing = spans
                .iter()
                .filter(|s| ln >= s.start_line && ln <= s.end_line)
                .min_by_key(|s| s.end_line.saturating_sub(s.start_line));
            if let Some(enc) = enclosing {
                let short_enc_name = enc.name.rsplit("::").next().unwrap_or(&enc.name);
                if short_enc_name == sym && ln == enc.start_line {
                    continue; // skip own declaration header
                }
            }
            hits.push(json!({
                "file": rel,
                "line": ln,
                "enclosing": enclosing
                    .map(|e| format!("{} {}", e.kind, e.name))
                    .unwrap_or_else(|| "<top-level>".to_string()),
                "snippet": line.trim().chars().take(160).collect::<String>(),
            }));
        }
    }

    json!({
        "ok": true,
        "symbol": sym,
        "files_scanned": files.len(),
        "count": hits.len(),
        "callers": hits,
    })
}
