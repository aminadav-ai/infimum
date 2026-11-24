//! In-memory multi-file Infimum compiler & Z3 SMT verifier API for `wasm32` (Online Infimum Code).
//!
//! Compiles `.ifm` projects (including `use "std/..."` and relative `use "..."` imports) directly
//! to `.ifw` WebAssembly bytes in memory with mandatory Z3 SMT verification (fail-closed: no
//! compilation or verification is permitted without Z3).

use crate::canon::{canonicalize_fn, CanonicalNode};
use crate::diag::{InfimumDiagnostic, Span};
use crate::parser::parse_module_from_virtual_fs;
use crate::store::topo_sort_functions;
use crate::strata_wasm::{compute_module_layout_end_word, emit_strata_wasm_bytes_with_layout_end};
use crate::typecheck::typecheck_node;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

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

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct InMemoryCompileResult {
    pub ok: bool,
    pub exported_functions: Vec<String>,
    pub compiled_nodes: usize,
    pub proven: usize,
    pub inconclusive: Vec<String>,
    pub verifier: String,
    pub error: Option<String>,
}

/// Parse, typecheck, verify with native `z3`, embed a real `SecurityPassport`, and compile
/// an in-memory `.ifm` project directly to `.ifw` bytes (native only; requires `z3` on PATH or `Z3_BIN`).
#[cfg(not(target_arch = "wasm32"))]
pub fn compile_project_in_memory(
    entry_path: &str,
    files: &BTreeMap<String, String>,
) -> Result<(Vec<u8>, InMemoryCompileResult), InfimumDiagnostic> {
    let z3 = std::path::PathBuf::from(
        std::env::var("Z3_BIN").unwrap_or_else(|_| "z3".to_string()),
    );
    let mut runner = |script: &str| crate::verify::run_z3(&z3, script);
    compile_project_in_memory_with_z3(entry_path, files, &mut runner)
}

/// Parse, typecheck, verify with mandatory Z3 SMT (`z3_runner`),
/// embed a real `SecurityPassport`, and compile an in-memory `.ifm` project directly to `.ifw` bytes.
pub fn compile_project_in_memory_with_z3<R>(
    entry_path: &str,
    files: &BTreeMap<String, String>,
    z3_runner: &mut R,
) -> Result<(Vec<u8>, InMemoryCompileResult), InfimumDiagnostic>
where
    R: FnMut(&str) -> Result<String, InfimumDiagnostic>,
{
    let module = parse_module_from_virtual_fs(entry_path, files)?;

    let order = topo_sort_functions(&module.functions)?;
    let mut aliases: BTreeMap<String, String> = BTreeMap::new();
    let mut nodes_by_hash: BTreeMap<String, CanonicalNode> = BTreeMap::new();
    let mut ordered_nodes: Vec<CanonicalNode> = Vec::with_capacity(order.len());
    let mut z3_proven: usize = 0;

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
        let vrep = crate::verify::verify_node_with_runner(raw_fn, &node, &loader, z3_runner)?;
        z3_proven += vrep.obligations_checked;
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

    let mut aliases_by_hash: BTreeMap<String, Vec<String>> = BTreeMap::new();
    for (alias, hash) in &aliases {
        aliases_by_hash
            .entry(hash.clone())
            .or_default()
            .push(alias.clone());
    }
    let passport_bytes = crate::store::build_merged_passport_bytes(&export_pairs, |alias| {
        let h = aliases.get(alias).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E032_NODE_NOT_FOUND",
                "passport",
                format!("Missing alias '{alias}' while computing passport"),
                Span::zero(),
                "Ensure exported functions exist in the module",
            )
        })?;
        crate::store::compute_passport_in_memory(h, &nodes_by_hash, &aliases_by_hash)
    })?;

    let layout_end_word = compute_module_layout_end_word(&module);
    let wasm_bytes = emit_strata_wasm_bytes_with_layout_end(
        &ordered_nodes,
        &export_pairs,
        &passport_bytes,
        layout_end_word,
    )?;
    let meta = InMemoryCompileResult {
        ok: true,
        exported_functions: export_pairs.into_iter().map(|(n, _)| n).collect(),
        compiled_nodes: ordered_nodes.len(),
        proven: z3_proven,
        inconclusive: Vec::new(),
        verifier: "z3".to_string(),
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

    fn make_js_z3_runner(
        z3_eval: &js_sys::Function,
    ) -> impl FnMut(&str) -> Result<String, InfimumDiagnostic> + '_ {
        move |script: &str| {
            let res = z3_eval
                .call1(&JsValue::NULL, &JsValue::from_str(script))
                .map_err(|e| {
                    InfimumDiagnostic::new(
                        "E058_Z3_EXEC",
                        "verify",
                        format!("Browser Z3.wasm evaluation error: {e:?}"),
                        Span::zero(),
                        "Check browser console for Z3.wasm initialization issues",
                    )
                })?;
            res.as_string().ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E058_Z3_EXEC",
                    "verify",
                    "Browser Z3.wasm callback returned non-string result".to_string(),
                    Span::zero(),
                    "Ensure evalSmtZ3 returns a string",
                )
            })
        }
    }

    #[wasm_bindgen]
    pub fn infimum_verify_project_z3_json(
        entry_path: &str,
        files_json: &str,
        z3_eval: &js_sys::Function,
    ) -> String {
        console_error_panic_hook::set_once();
        let files: BTreeMap<String, String> = match serde_json::from_str(files_json) {
            Ok(f) => f,
            Err(e) => {
                return serde_json::json!({ "ok": false, "error": format!("Invalid files JSON: {e}") }).to_string();
            }
        };
        let mut runner = make_js_z3_runner(z3_eval);
        match compile_project_in_memory_with_z3(entry_path, &files, &mut runner) {
            Ok((_, meta)) => {
                serde_json::to_string(&meta).unwrap_or_else(|_| r#"{"ok":true}"#.to_string())
            }
            Err(diag) => serde_json::json!({
                "ok": false,
                "error": format!("error {diag}"),
            })
            .to_string(),
        }
    }

    #[wasm_bindgen]
    pub fn infimum_compile_project_z3(
        entry_path: &str,
        files_json: &str,
        z3_eval: &js_sys::Function,
    ) -> Result<js_sys::Uint8Array, JsValue> {
        console_error_panic_hook::set_once();
        let files: BTreeMap<String, String> = serde_json::from_str(files_json)
            .map_err(|e| JsValue::from_str(&format!("Invalid files JSON: {e}")))?;
        let mut runner = make_js_z3_runner(z3_eval);
        match compile_project_in_memory_with_z3(entry_path, &files, &mut runner) {
            Ok((bytes, _)) => Ok(js_sys::Uint8Array::from(bytes.as_slice())),
            Err(diag) => Err(JsValue::from_str(&format!("error {diag}"))),
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
        let module = match parse_module_from_virtual_fs(entry_path, &files) {
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

#[cfg(all(test, not(target_arch = "wasm32")))]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn test_z3_runner(script: &str) -> Result<String, InfimumDiagnostic> {
        let z3 = PathBuf::from(std::env::var("Z3_BIN").unwrap_or_else(|_| "z3".to_string()));
        crate::verify::run_z3(&z3, script)
    }

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
        let mut runner = test_z3_runner;
        let (bytes, meta) =
            compile_project_in_memory_with_z3("main.ifm", &files, &mut runner).unwrap();
        assert!(bytes.starts_with(b"\0asm"), "Expected valid Wasm magic header");
        assert!(meta.ok);
        assert_eq!(meta.verifier, "z3");
        assert!(meta.proven > 0, "Expected Z3 obligations proven");
        assert!(meta.exported_functions.contains(&"add_clamped".to_string()));
        let text = String::from_utf8_lossy(&bytes);
        assert!(
            text.contains("infimum.passport") && text.contains("\"root_hash\"") && text.contains("\"add_clamped\""),
            "Expected real SecurityPassport embedded in .ifw custom section"
        );
    }

    #[test]
    fn duplicate_fn_reports_both_files_and_lines() {
        let mut files = BTreeMap::new();
        files.insert(
            "main.ifm".to_string(),
            "use \"std/math.ifm\";\nuse \"render.ifm\";\nexport fn entry() -> i64 { clamp_i64(5, 0, 10) }\n".to_string(),
        );
        files.insert(
            "render.ifm".to_string(),
            "\n\nfn clamp_i64(v: i64, lo: i64, hi: i64) -> i64 {\n  v\n}\n".to_string(),
        );
        let mut runner = test_z3_runner;
        let err = compile_project_in_memory_with_z3("main.ifm", &files, &mut runner).unwrap_err();
        assert_eq!(err.code, "E024_DUPLICATE_FN_NAME");
        assert_eq!(err.span.line, 3);
        assert!(
            err.message.contains("std/math.ifm:") && err.message.contains("render.ifm:3"),
            "Unexpected diagnostic message: {}",
            err.message
        );
    }
}
