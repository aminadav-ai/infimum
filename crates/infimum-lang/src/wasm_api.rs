//! In-memory multi-file Infimum compiler & lightweight verifier API for `wasm32` (Online Infimum Code).
//!
//! Compiles `.ifm` projects (including `use "std/..."` and relative `use "..."` imports) directly
//! to `.ifw` WebAssembly bytes in memory without Z3 or filesystem access.

use crate::canon::{canonicalize_fn, CanonicalNode};
use crate::diag::{InfimumDiagnostic, Span};
use crate::light_verify::{verify_module_light, LightVerifyReport};
use crate::parser::parse_module;
use crate::store::topo_sort_functions;
use crate::strata_wasm::{compute_module_layout_end_word, emit_strata_wasm_bytes_with_layout_end};
use crate::typecheck::typecheck_node;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};

/// Embedded `std/*.ifm` standard library so Online Infimum Code works out of the box with zero extra fetches.
pub fn embedded_std_file(name: &str) -> Option<&'static str> {
    let clean = name.trim_start_matches("std/").trim_start_matches("./");
    match clean {
        "audio.ifm" => Some(include_str!("../std/audio.ifm")),
        "buf.ifm" => Some(include_str!("../std/buf.ifm")),
        "color.ifm" => Some(include_str!("../std/color.ifm")),
        "fmt.ifm" => Some(include_str!("../std/fmt.ifm")),
        "gfx.ifm" => Some(include_str!("../std/gfx.ifm")),
        "icons.ifm" => Some(include_str!("../std/icons.ifm")),
        "input.ifm" => Some(include_str!("../std/input.ifm")),
        "json.ifm" => Some(include_str!("../std/json.ifm")),
        "markdown.ifm" => Some(include_str!("../std/markdown.ifm")),
        "math.ifm" => Some(include_str!("../std/math.ifm")),
        "mem.ifm" => Some(include_str!("../std/mem.ifm")),
        "mesh3d.ifm" => Some(include_str!("../std/mesh3d.ifm")),
        "prelude.ifm" => Some(include_str!("../std/prelude.ifm")),
        "sys_mem.ifm" => Some(include_str!("../std/sys_mem.ifm")),
        "text.ifm" => Some(include_str!("../std/text.ifm")),
        "text_edit.ifm" => Some(include_str!("../std/text_edit.ifm")),
        "theme.ifm" => Some(include_str!("../std/theme.ifm")),
        "ui.ifm" => Some(include_str!("../std/ui.ifm")),
        _ => None,
    }
}

pub fn list_embedded_std_files() -> &'static [&'static str] {
    &[
        "audio.ifm",
        "buf.ifm",
        "color.ifm",
        "fmt.ifm",
        "gfx.ifm",
        "icons.ifm",
        "input.ifm",
        "json.ifm",
        "markdown.ifm",
        "math.ifm",
        "mem.ifm",
        "mesh3d.ifm",
        "prelude.ifm",
        "sys_mem.ifm",
        "text.ifm",
        "text_edit.ifm",
        "theme.ifm",
        "ui.ifm",
    ]
}

fn normalize_rel(base_dir: &str, rel: &str) -> String {
    let raw = if base_dir.is_empty() || rel.starts_with("std/") {
        rel.to_string()
    } else {
        format!("{base_dir}/{rel}")
    };
    let mut parts: Vec<&str> = Vec::new();
    for seg in raw.split('/') {
        match seg {
            "" | "." => {}
            ".." => {
                parts.pop();
            }
            s => parts.push(s),
        }
    }
    parts.join("/")
}

fn parent_dir(path: &str) -> &str {
    match path.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    }
}

fn expand_uses_in_memory(
    cur_path: &str,
    files: &BTreeMap<String, String>,
    visited: &mut BTreeSet<String>,
    out: &mut String,
) -> Result<(), InfimumDiagnostic> {
    let key = normalize_rel("", cur_path);
    if !visited.insert(key.clone()) {
        return Ok(());
    }
    let src = if let Some(s) = files.get(&key) {
        s.as_str()
    } else if let Some(s) = embedded_std_file(&key) {
        s
    } else if let Some(std_tail) = key.split("std/").nth(1).and_then(embedded_std_file) {
        std_tail
    } else {
        return Err(InfimumDiagnostic::new(
            "E010_IMPORT_NOT_FOUND",
            "parser",
            format!("Cannot resolve `use \"{cur_path}\"` in virtual project"),
            Span::zero(),
            "Check that the imported .ifm file exists in the project or std/",
        ));
    };

    let base_dir = parent_dir(&key);
    let mut body_lines = Vec::new();
    for line in src.lines() {
        let trimmed = line.trim();
        if let Some(rest) = trimmed.strip_prefix("use \"") {
            if let Some(end_q) = rest.find('"') {
                let imp = &rest[..end_q];
                let resolved = if imp.starts_with("std/") || imp.contains("/std/") {
                    if let Some(tail) = imp.split("std/").nth(1) {
                        format!("std/{tail}")
                    } else {
                        normalize_rel(base_dir, imp)
                    }
                } else {
                    normalize_rel(base_dir, imp)
                };
                expand_uses_in_memory(&resolved, files, visited, out)?;
                continue;
            }
        }
        if let Some(rest) = trimmed.strip_prefix("mod ") {
            if let Some((mname, after_from)) = rest.split_once(" from \"") {
                if let Some(end_q) = after_from.find('"') {
                    let mname = mname.trim();
                    let imp = &after_from[..end_q];
                    let resolved = normalize_rel(base_dir, imp);
                    let mut mod_body = String::new();
                    expand_uses_in_memory(&resolved, files, visited, &mut mod_body)?;
                    out.push_str(&format!("mod {mname} {{\n{mod_body}\n}}\n"));
                    continue;
                }
            }
        }
        body_lines.push(line);
    }
    out.push_str(&body_lines.join("\n"));
    out.push('\n');
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InMemoryCompileResult {
    pub ok: bool,
    pub exported_functions: Vec<String>,
    pub compiled_nodes: usize,
    pub proven: usize,
    pub inconclusive: Vec<String>,
    pub error: Option<String>,
}

/// Parse, typecheck, light-verify, and compile an in-memory `.ifm` project directly to `.ifw` bytes.
pub fn compile_project_in_memory(
    entry_path: &str,
    files: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, InMemoryCompileResult), InfimumDiagnostic> {
    let mut combined = String::new();
    let mut visited = BTreeSet::new();
    expand_uses_in_memory(entry_path, files, &mut visited, &mut combined)?;

    let module = parse_module(&combined)?;
    let light_report: LightVerifyReport = verify_module_light(&module)?;

    let order = topo_sort_functions(&module.functions)?;
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut nodes_by_hash: BTreeMap<String, CanonicalNode> = BTreeMap::new();
    let mut ordered_nodes: Vec<CanonicalNode> = Vec::with_capacity(order.len());

    for &idx in &order {
        let raw_fn = &module.functions[idx];
        let mut node = canonicalize_fn(
            raw_fn,
            &|callee| {
                aliases.get(callee).cloned().ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E022_UNKNOWN_FUNCTION",
                        "resolve",
                        format!("Function '{}' calls unknown function '{callee}'", raw_fn.name),
                        Span::zero(),
                        "Define the callee in the same project or import it via `use`",
                    )
                })
            },
            &|prefix| {
                let clean = prefix.trim_start_matches('#').to_ascii_lowercase();
                nodes_by_hash
                    .keys()
                    .find(|h| h.starts_with(&clean))
                    .cloned()
                    .ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E032_NODE_NOT_FOUND",
                            "resolve",
                            format!("Unknown node hash prefix '{prefix}'"),
                            Span::zero(),
                            "Check the hash reference",
                        )
                    })
            },
        )?;
        let loader = |dep_hash: &str| -> Result<CanonicalNode, InfimumDiagnostic> {
            nodes_by_hash.get(dep_hash).cloned().ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E032_NODE_NOT_FOUND",
                    "typecheck",
                    format!("Missing dependency node #{dep_hash}"),
                    Span::zero(),
                    "Ensure all callees are defined",
                )
            })
        };
        node.direct_secret_sinks = typecheck_node(&raw_fn.name, &node, &loader)?;
        aliases.insert(raw_fn.name.clone(), node.hash.clone());
        nodes_by_hash.insert(node.hash.clone(), node.clone());
        ordered_nodes.push(node);
    }

    let mut export_pairs: Vec<(String, String)> = Vec::new();
    for raw_fn in &module.functions {
        if raw_fn.exported {
            if let Some(h) = aliases.get(&raw_fn.name) {
                export_pairs.push((raw_fn.name.clone(), h.clone()));
            }
        }
    }
    if export_pairs.is_empty() {
        if let Some(last) = module.functions.last() {
            if let Some(h) = aliases.get(&last.name) {
                export_pairs.push((last.name.clone(), h.clone()));
            }
        }
    }

    let layout_end_word = compute_module_layout_end_word(&module);
    let wasm_bytes =
        emit_strata_wasm_bytes_with_layout_end(&ordered_nodes, &export_pairs, b"{}", layout_end_word)?;
    let meta = InMemoryCompileResult {
        ok: true,
        exported_functions: export_pairs.into_iter().map(|(n, _)| n).collect(),
        compiled_nodes: ordered_nodes.len(),
        proven: light_report.proven,
        inconclusive: light_report.inconclusive,
        error: None,
    };
    Ok((wasm_bytes, meta))
}

#[cfg(target_arch = "wasm32")]
mod js_bindings {
    use super::*;
    use wasm_bindgen::prelude::*;

    #[wasm_bindgen]
    pub fn infimum_read_std(name: &str) -> Option<String> {
        embedded_std_file(name).map(|s| s.to_string())
    }

    #[wasm_bindgen]
    pub fn infimum_list_std_json() -> String {
        serde_json::to_string(list_embedded_std_files()).unwrap_or_else(|_| "[]".to_string())
    }

    #[wasm_bindgen]
    pub fn infimum_verify_project_json(entry_path: &str, files_json: &str) -> String {
        console_error_panic_hook::set_once();
        let files: BTreeMap<String, String> = match serde_json::from_str(files_json) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({ "ok": false, "error": format!("Invalid files JSON: {e}") }).to_string();
            }
        };
        match compile_project_in_memory(entry_path, &files) {
            Ok((_, meta)) => serde_json::to_string(&meta).unwrap_or_else(|_| r#"{"ok":true}"#.to_string()),
            Err(diag) => serde_json::json!({
                "ok": false,
                "error": format!("error [{}] ({}:{}): {}\n  hint: {}", diag.code, diag.span.line, diag.span.col, diag.message, diag.repair_hint),
            })
            .to_string(),
        }
    }

    #[wasm_bindgen]
    pub fn infimum_compile_project(entry_path: &str, files_json: &str) -> Result<js_sys::Uint8Array, JsValue> {
        console_error_panic_hook::set_once();
        let files: BTreeMap<String, String> = serde_json::from_str(files_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid files JSON: {e}")))?;
        match compile_project_in_memory(entry_path, &files) {
            Ok((bytes, _)) => Ok(js_sys::Uint8Array::from(bytes.as_slice())),
            Err(diag) => Err(JsValue::from_str(&format!(
                "error [{}] ({}:{}): {}\n  hint: {}",
                diag.code, diag.span.line, diag.span.col, diag.message, diag.repair_hint
            ))),
        }
    }

    #[wasm_bindgen]
    pub fn infimum_test_project_json(entry_path: &str, files_json: &str) -> String {
        console_error_panic_hook::set_once();
        let files: BTreeMap<String, String> = match serde_json::from_str(files_json) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({ "ok": false, "error": format!("Invalid files JSON: {e}") }).to_string();
            }
        };
        let mut combined = String::new();
        let mut visited = BTreeSet::new();
        if let Err(diag) = expand_uses_in_memory(entry_path, &files, &mut visited, &mut combined) {
            return serde_json::json!({
                "ok": false,
                "error": format!("error [{}] ({}:{}): {}\n  hint: {}", diag.code, diag.span.line, diag.span.col, diag.message, diag.repair_hint),
            })
            .to_string();
        }
        let module = match parse_module(&combined) {
            Ok(m) => m,
            Err(diag) => {
                return serde_json::json!({
                    "ok": false,
                    "error": format!("error [{}] ({}:{}): {}\n  hint: {}", diag.code, diag.span.line, diag.span.col, diag.message, diag.repair_hint),
                })
                .to_string();
            }
        };
        let results = crate::eval::run_module_tests(&module);
        let passed = results.iter().filter(|r| r.passed).count();
        let failed = results.len() - passed;
        serde_json::json!({
            "ok": failed == 0,
            "passed": passed,
            "failed": failed,
            "results": results,
        })
        .to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_in_memory_project_with_std_import_to_valid_wasm() {
        let mut files = BTreeMap::new();
        files.insert(
            "main.ifm".to_string(),
            r#"
use "std/math.ifm";

export fn add_clamped(a: i64, b: i64) -> i64
  req a >= 0
  req b >= 0
  ens result >= 0
{
  clamp_i64(a + b, 0, 100)
}
"#
            .to_string(),
        );
        let (bytes, meta) = compile_project_in_memory("main.ifm", &files).unwrap();
        assert!(bytes.starts_with(b"\0asm"), "Expected valid Wasm magic header");
        assert!(meta.ok);
        assert!(meta.exported_functions.contains(&"add_clamped".to_string()));
    }
}
