// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

mod loader;
mod raw_ast;
mod resolver;
mod scope;
mod syntax;
#[cfg(test)]
mod tests;

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::fs;
use std::path::Path;

use crate::ast::{BinOp, EffectCap, Expr, FnDef, Module, Type};
use crate::diag::{InfimumDiagnostic, Span};

pub(crate) use raw_ast::RawExpr;
pub use syntax::Parser;

use loader::{DiskSource, MemorySource, UnitLoader, VirtualFsSource, normalize_virtual_rel};
use raw_ast::{RawFnDef, RawType, UnitGraph, UnitId};
use resolver::{
    ModuleResolver, expand_and_lower_forma_node, resolve_fixed_ops_in_fns,
    rewrite_forma_slot_callbacks,
};
use scope::{build_unit_tables, unit_fn_canonical_name};

pub fn parse_module(src: &str) -> Result<Module, InfimumDiagnostic> {
    let source = MemorySource;
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(
        "<memory>".to_string(),
        "<memory>".to_string(),
        src,
        true,
        false,
        false,
        Span::zero(),
    )?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}

/// Load and parse a multi-file Infimum module starting at `entry_path`.
///
/// Resolves `[pub] use "..."` directives recursively relative to each file's directory,
/// isolates every file as a Pascal-style unit (`pub` interface / private implementation),
/// deduplicates diamond imports by canonical path, detects import cycles, and collects all
/// declarations in deterministic topological order (dependencies before dependents).
pub fn parse_module_from_path(entry_path: &Path) -> Result<Module, InfimumDiagnostic> {
    let canon = fs::canonicalize(entry_path).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to resolve '{}': {e}", entry_path.display()),
            Span::zero(),
            "Check that the entry .ifm file path exists",
        )
    })?;
    let src = fs::read_to_string(&canon).map_err(|e| {
        InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to read '{}': {e}", canon.display()),
            Span::zero(),
            "Ensure the .ifm file is readable UTF-8 text",
        )
    })?;
    let entry_dir = canon.parent().map(|d| d.to_path_buf());
    let source = DiskSource { entry_dir };
    let label = source.display_for(&canon);
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(
        canon.to_string_lossy().to_string(),
        label,
        &src,
        true,
        false,
        false,
        Span::zero(),
    )?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}

/// Load and parse a multi-file Infimum module from an in-memory virtual filesystem
/// (used by Online Infimum Code / `wasm32`) while preserving per-file names and spans.
pub fn parse_module_from_virtual_fs(
    entry_path: &str,
    files: &BTreeMap<String, String>,
) -> Result<Module, InfimumDiagnostic> {
    let norm = normalize_virtual_rel("", entry_path);
    let Some(src) = files.get(&norm) else {
        return Err(InfimumDiagnostic::new(
            "E040_FILE_READ",
            "parse",
            format!("Failed to resolve virtual entry file '{norm}'"),
            Span::zero(),
            "Check that the entry .ifm file exists in the project workspace",
        ));
    };
    let source = VirtualFsSource { files };
    let mut loader = UnitLoader::new(&source);
    let entry_uid = loader.load_unit(norm.clone(), norm, src, true, false, false, Span::zero())?;
    let graph = loader.finish(entry_uid)?;
    resolve_and_lower(graph)
}


fn resolve_and_lower(mut graph: UnitGraph) -> Result<Module, InfimumDiagnostic> {
    let tables = build_unit_tables(&graph)?;
    let unit_labels: Vec<String> = graph.units.iter().map(|u| u.display_label.clone()).collect();
    let unit_is_builtin: Vec<bool> = graph.units.iter().map(|u| u.is_builtin).collect();
    let entry_uid = graph.entry_uid;
    let has_std_gfx = graph.find_unit("__std__/gfx.ifm").is_some()
        || tables[entry_uid].scope.consts.contains_key("GPU_CMD_BYTE_OFFSET");
    let has_std_capsule = graph.find_unit("__std__/capsule.ifm").is_some();
    let std_text_uid = graph.find_unit("__std__/text.ifm");
    let std_mesh3d_uid = graph.find_unit("__std__/mesh3d.ifm");

    let mut resolver = ModuleResolver::new(
        tables,
        unit_labels,
        unit_is_builtin,
        entry_uid,
        has_std_gfx,
        std_text_uid,
    );

    // 1. Register all consts, enums, structs, and layouts in topological order
    let topo = graph.topo_order.clone();
    for &uid in &topo {
        resolver.cur_uid = uid;
        let items = std::mem::take(&mut graph.units[uid].items);
        let owner_u = &graph.units[uid];
        for item in items {
            resolver.register_item(item, owner_u)?;
        }
    }
    resolver.verify_no_layout_overlaps()?;

    // 2. Collect all forma definitions keyed by canonical name (paired with their owning UnitId)
    let multi_unit = graph.units.len() > 1;
    let mut all_raw_formas: Vec<(UnitId, String, crate::forma::FormaDef<RawExpr>)> = Vec::new();
    let mut forma_defs_by_canon: BTreeMap<String, (UnitId, crate::forma::FormaDef<RawExpr>)> =
        BTreeMap::new();
    for &uid in &topo {
        let u = &mut graph.units[uid];
        let formas_u = std::mem::take(&mut u.formas);
        resolver.cur_uid = uid;
        for mut rf in formas_u {
            let cname = unit_fn_canonical_name(u, &rf.name, rf.exported, multi_unit);
            rf.name = cname.clone();
            rewrite_forma_slot_callbacks(&mut rf.root, &resolver)?;
            forma_defs_by_canon.insert(cname.clone(), (uid, rf.clone()));
            all_raw_formas.push((uid, cname, rf));
        }
    }

    // 3. Expand and lower all forma definitions in a single lexically-hygienic pass
    let mut formas = Vec::with_capacity(all_raw_formas.len());
    for (uid, cname, rf) in all_raw_formas {
        resolver.cur_uid = uid;
        resolver.forma_subst.clear();
        let mut visiting = vec![cname];
        let param_set: BTreeSet<String> = rf.params.iter().map(|p| p.name.clone()).collect();
        let mut param_locals = vec![param_set];
        let expanded_roots = expand_and_lower_forma_node(
            rf.root,
            &forma_defs_by_canon,
            &mut resolver,
            &mut visiting,
            &mut param_locals,
        )?;
        let lowered_root = if expanded_roots.len() == 1 {
            expanded_roots.into_iter().next().unwrap()
        } else {
            crate::forma::FormaNode {
                kind: crate::forma::FormaNodeKind::Box,
                props: crate::forma::FormaProps::default(),
                children: expanded_roots,
                span: rf.span,
                component_call: None,
                forma_for: None,
                forma_if: None,
            }
        };
        let requires = rf
            .requires
            .into_iter()
            .map(|e| resolver.lower_expr(&e, &mut param_locals))
            .collect::<Result<Vec<_>, _>>()?;
        let ensures = rf
            .ensures
            .into_iter()
            .map(|e| resolver.lower_expr(&e, &mut param_locals))
            .collect::<Result<Vec<_>, _>>()?;
        formas.push(crate::forma::FormaDef {
            name: rf.name,
            exported: rf.exported,
            is_pub: rf.is_pub,
            params: rf.params,
            effects: rf.effects,
            ret_type: rf.ret_type,
            requires,
            ensures,
            root: lowered_root,
            span: rf.span,
        });
    }

    // 4. Pre-register function return structs by canonical name
    let mut all_raw_fns: Vec<(UnitId, RawFnDef)> = Vec::new();
    for &uid in &topo {
        let u = &mut graph.units[uid];
        let fns_u = std::mem::take(&mut u.functions);
        resolver.cur_uid = uid;
        for mut rf in fns_u {
            let cname = unit_fn_canonical_name(u, &rf.name, rf.exported, multi_unit);
            rf.name = cname.clone();
            if let RawType::Named { module, name, span } = &rf.ret_type {
                let scanon = resolver.resolve_struct_canon(module.as_deref(), name, *span)?;
                resolver.fn_ret_struct.insert(cname, scanon);
            } else if let RawType::Slice(elem) = &rf.ret_type {
                let smeta = resolver.resolve_slice_elem(elem)?;
                resolver.fn_ret_slice.insert(cname, smeta);
            } else if matches!(&rf.ret_type, RawType::Builtin(Type::Str)) {
                resolver.fn_ret_slice.insert(cname, resolver::SliceElemMeta::utf8_str());
            } else if let Some(tmeta) = resolver.resolve_tagged_meta(&rf.ret_type)? {
                resolver.fn_ret_tagged.insert(cname, tmeta);
            }
            all_raw_fns.push((uid, rf));
        }
    }

    let mut functions = Vec::with_capacity(
        all_raw_fns.len() + formas.len() + if formas.is_empty() { 0 } else { 5 },
    );
    if !formas.is_empty() {
        let cp_atlas_fn = std_text_uid
            .and_then(|tid| resolver.tables[tid].exports.fns.get("codepoint_to_atlas"))
            .map(|s| s.canonical_name.clone())
            .unwrap_or_else(|| "codepoint_to_atlas".to_string());
        let glyph_px_fn = std_text_uid
            .and_then(|tid| resolver.tables[tid].exports.fns.get("glyph_px"))
            .map(|s| s.canonical_name.clone())
            .unwrap_or_else(|| "glyph_px".to_string());
        let existing_names: HashSet<&str> =
            all_raw_fns.iter().map(|(_, rf)| rf.name.as_str()).collect();
        for h in crate::forma::forma_runtime_helpers_with_text(&cp_atlas_fn, &glyph_px_fn) {
            if !existing_names.contains(h.name.as_str()) {
                functions.push(h);
            }
        }
    }

    let mut tests = Vec::new();
    for (uid, rf) in all_raw_fns {
        resolver.cur_uid = uid;
        if let Some(label) = rf.test_label.clone() {
            tests.push((rf.name.clone(), label));
        }
        functions.push(resolver.lower_fn(rf)?);
    }
    for forma in &formas {
        functions.push(crate::forma::lower_forma_def(forma));
    }

    // 5b. Auto-generate Headless B2B Capsule ABI exports when std/capsule is loaded and cap_execute is exported
    if has_std_capsule && functions.iter().any(|f| f.exported && f.name == "cap_execute") {
        let existing: HashSet<String> = functions.iter().map(|f| f.name.clone()).collect();
        if let Some(cap_layout) = resolver.layouts.values().find(|l| l.name == "CapsuleIo") {
            let field_byte_off = |fname: &str| -> i64 {
                cap_layout
                    .fields
                    .iter()
                    .find(|f| f.name == fname)
                    .map(|f| (if f.is_vec { f.offset + 1 } else { f.offset }) * 4)
                    .unwrap_or(0)
            };
            let cap_exports: [(&str, i64); 7] = [
                ("ifw_capsule_abi_version", 1),
                ("ifw_cap_input_byte_offset", field_byte_off("input_buf")),
                ("ifw_cap_input_cap_bytes", 65_536),
                ("ifw_cap_output_byte_offset", field_byte_off("output_buf")),
                ("ifw_cap_output_cap_bytes", 65_536),
                ("ifw_cap_secret_byte_offset", field_byte_off("secret_buf")),
                ("ifw_cap_secret_cap_bytes", 4_096),
            ];
            for (ename, val) in cap_exports {
                if !existing.contains(ename) {
                    functions.push(FnDef {
                        name: ename.to_string(),
                        exported: true,
                        params: vec![],
                        effects: vec![],
                        ret_type: Type::I64,
                        requires: vec![],
                        ensures: vec![],
                        body: Expr::IntLit(val),
                        source_file: None,
                        span: None,
                    });
                }
            }
        }
    }

    // 5. Auto-generate standard .ifw GPU runtime exports when std/gfx is loaded and step_ui is exported
    if has_std_gfx && functions.iter().any(|f| f.exported && f.name == "step_ui") {
        let existing: HashSet<String> = functions.iter().map(|f| f.name.clone()).collect();
        let mem_eff = vec![EffectCap {
            name: "Mem".to_string(),
            scope: None,
        }];
        if !existing.contains("ifw_abi_version") {
            functions.push(FnDef {
                name: "ifw_abi_version".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::IntLit(5),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("ifw_unicode_atlas") {
            functions.push(FnDef {
                name: "ifw_unicode_atlas".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::IntLit(1),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_cmd_byte_offset") {
            functions.push(FnDef {
                name: "get_cmd_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(262144)),
                }],
                body: Expr::IntLit(262144),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_clips_byte_offset") {
            functions.push(FnDef {
                name: "get_clips_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(256)),
                }],
                body: Expr::IntLit(256),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_font_metrics_byte_offset") {
            functions.push(FnDef {
                name: "get_font_metrics_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(1024)),
                }],
                body: Expr::IntLit(1024),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_input_byte_offset") {
            functions.push(FnDef {
                name: "get_input_byte_offset".to_string(),
                exported: true,
                params: vec![],
                effects: vec![],
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var("result".to_string())),
                    rhs: Box::new(Expr::IntLit(768)),
                }],
                body: Expr::IntLit(768),
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_mesh3d_byte_offset") {
            let m3d_canon = std_mesh3d_uid
                .and_then(|uid| resolver.tables[uid].exports.layouts.get("Mesh3dMem"))
                .map(|s| s.canonical_name.as_str());
            if let Some(m3d) = m3d_canon.and_then(|c| resolver.layouts.get(c)) {
                let byte_off = m3d.base * 4;
                functions.push(FnDef {
                    name: "get_mesh3d_byte_offset".to_string(),
                    exported: true,
                    params: vec![],
                    effects: vec![],
                    ret_type: Type::I64,
                    requires: vec![],
                    ensures: vec![Expr::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(Expr::Var("result".to_string())),
                        rhs: Box::new(Expr::IntLit(byte_off)),
                    }],
                    body: Expr::IntLit(byte_off),
                    source_file: None,
                    span: None,
                });
            }
        }
        if !existing.contains("get_prim_count") {
            functions.push(FnDef {
                name: "get_prim_count".to_string(),
                exported: true,
                params: vec![],
                effects: mem_eff.clone(),
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::MemLoad {
                    index: Box::new(Expr::IntLit(12)),
                    inv: None,
                    fixed_ty: false,
                    region: 0,
                },
                source_file: None,
                span: None,
            });
        }
        if !existing.contains("get_cursor_kind") {
            functions.push(FnDef {
                name: "get_cursor_kind".to_string(),
                exported: true,
                params: vec![],
                effects: mem_eff,
                ret_type: Type::I64,
                requires: vec![],
                ensures: vec![],
                body: Expr::MemLoad {
                    index: Box::new(Expr::IntLit(11)),
                    inv: None,
                    fixed_ty: false,
                    region: 0,
                },
                source_file: None,
                span: None,
            });
        }
    }

    resolve_fixed_ops_in_fns(&mut functions);

    // 6. Export clean ABI surface from entry unit's public interface (or entry scope if no `pub` items)
    let entry_exports = &resolver.tables[entry_uid].exports;
    let entry_scope = &resolver.tables[entry_uid].scope;
    let use_exports = !entry_exports.consts_order.is_empty()
        || !entry_exports.enums_order.is_empty()
        || !entry_exports.layouts_order.is_empty();
    let abi_src = if use_exports { entry_exports } else { entry_scope };

    let mut consts = Vec::new();
    for k in &abi_src.consts_order {
        if let Some(sym) = abi_src.consts.get(k) {
            if let Some(mut cdef) = resolver.consts.get(&sym.canonical_name).cloned() {
                cdef.name = k.clone();
                consts.push(cdef);
            }
        }
    }
    let mut enums = Vec::new();
    for k in &abi_src.enums_order {
        if let Some(sym) = abi_src.enums.get(k) {
            if let Some(mut edef) = resolver.enums.get(&sym.canonical_name).cloned() {
                edef.name = k.clone();
                enums.push(edef);
            }
        }
    }
    let mut layouts = Vec::new();
    for k in &abi_src.layouts_order {
        if let Some(sym) = abi_src.layouts.get(k) {
            if let Some(mut ldef) = resolver.layouts.get(&sym.canonical_name).cloned() {
                ldef.name = k.clone();
                layouts.push(ldef);
            }
        }
    }

    let mut all_layouts_max_end: i64 = 0;
    for ldef in resolver.layouts.values() {
        for f in &ldef.fields {
            let end = f.offset + f.len;
            if end > all_layouts_max_end {
                all_layouts_max_end = end;
            }
        }
    }
    let all_layouts_end_word = if all_layouts_max_end > 0 {
        (((all_layouts_max_end + 15) / 16) * 16).min(16_777_216) as u32
    } else {
        0
    };

    Ok(Module {
        consts,
        enums,
        layouts,
        all_layouts_end_word,
        formas,
        functions,
        tests,
    })
}
