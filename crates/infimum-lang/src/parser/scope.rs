// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

use std::collections::BTreeMap;

use crate::diag::{InfimumDiagnostic, Span};

use super::raw_ast::{RawItem, RawUnit, RawUseKind, UnitGraph, UnitId};

#[derive(Clone, PartialEq, Eq)]
pub(super) struct SymRef {
    pub(super) owner: UnitId,
    pub(super) orig_name: String,
    pub(super) canonical_name: String,
    pub(super) decl_file: String,
    pub(super) decl_span: Span,
}

#[derive(Clone, PartialEq, Eq)]
pub(super) struct NamespaceRef {
    pub(super) target_uid: UnitId,
    pub(super) decl_file: String,
    pub(super) decl_span: Span,
}

#[derive(Clone, Default)]
pub(super) struct SymbolTable {
    pub(super) fns: BTreeMap<String, SymRef>,
    pub(super) consts: BTreeMap<String, SymRef>,
    pub(super) enums: BTreeMap<String, SymRef>,
    pub(super) structs: BTreeMap<String, SymRef>,
    pub(super) layouts: BTreeMap<String, SymRef>,
    pub(super) formas: BTreeMap<String, SymRef>,
    pub(super) namespaces: BTreeMap<String, NamespaceRef>,
    // Insertion order for deterministic ABI export
    pub(super) consts_order: Vec<String>,
    pub(super) enums_order: Vec<String>,
    pub(super) layouts_order: Vec<String>,
}

#[derive(Clone, Default)]
pub(super) struct UnitTables {
    pub(super) exports: SymbolTable,
    pub(super) scope: SymbolTable,
}

pub(super) fn unit_canonical_name(u: &RawUnit, name: &str, is_exported_fn: bool) -> String {
    if u.is_entry || is_exported_fn || (u.is_forma_rt && name.starts_with("__forma_")) {
        name.to_string()
    } else {
        format!("u{}__{name}", u.id)
    }
}

pub(super) fn unit_fn_canonical_name(
    u: &RawUnit,
    name: &str,
    is_exported_fn: bool,
    multi_unit: bool,
) -> String {
    if is_exported_fn
        || name.starts_with("__ifm_test_")
        || (u.is_forma_rt && name.starts_with("__forma_"))
        || (!multi_unit && u.is_entry)
    {
        name.to_string()
    } else {
        format!("u{}__{name}", u.id)
    }
}

pub(super) fn insert_sym(
    map: &mut BTreeMap<String, SymRef>,
    order: Option<&mut Vec<String>>,
    key: String,
    sym: SymRef,
    kind_label: &str,
    use_span: Span,
    is_explicit_alias: bool,
) -> Result<(), InfimumDiagnostic> {
    if let Some(existing) = map.get(&key) {
        if existing.owner == sym.owner
            && existing.orig_name == sym.orig_name
            && existing.decl_span == sym.decl_span
        {
            return Ok(());
        }
        let code = if is_explicit_alias {
            "E069_DUPLICATE_ALIAS"
        } else {
            "E024_DUPLICATE_SYMBOL"
        };
        let err_span = if use_span.line > 0 { use_span } else { sym.decl_span };
        return Err(InfimumDiagnostic::new(
            code,
            "resolve",
            format!(
                "Duplicate {kind_label} '{key}': defined at {}:{}:{} and {}:{}:{}",
                existing.decl_file,
                existing.decl_span.line,
                existing.decl_span.col,
                sym.decl_file,
                sym.decl_span.line,
                sym.decl_span.col
            ),
            err_span,
            format!(
                "Disambiguate '{key}' with a namespace import (use \"file.ifm\" as alias;) or selective alias (use \"file.ifm\" {{ {key} as other }};)"
            ),
        ));
    }
    if let Some(ord) = order {
        ord.push(key.clone());
    }
    map.insert(key, sym);
    Ok(())
}

pub(super) fn insert_namespace(
    map: &mut BTreeMap<String, NamespaceRef>,
    alias: String,
    ns: NamespaceRef,
    use_span: Span,
) -> Result<(), InfimumDiagnostic> {
    if let Some(existing) = map.get(&alias) {
        if existing.target_uid == ns.target_uid {
            return Ok(());
        }
        return Err(InfimumDiagnostic::new(
            "E069_DUPLICATE_ALIAS",
            "resolve",
            format!(
                "Duplicate module alias '{alias}' (previously bound at {}:{}:{})",
                existing.decl_file, existing.decl_span.line, existing.decl_span.col
            ),
            if use_span.line > 0 { use_span } else { ns.decl_span },
            format!("Choose a distinct namespace alias instead of '{alias}'"),
        ));
    }
    map.insert(alias, ns);
    Ok(())
}

pub(super) fn merge_table_all(
    dst: &mut SymbolTable,
    src: &SymbolTable,
    use_span: Span,
) -> Result<(), InfimumDiagnostic> {
    for (k, v) in &src.fns {
        insert_sym(&mut dst.fns, None, k.clone(), v.clone(), "function", use_span, false)?;
    }
    for k in &src.consts_order {
        if let Some(v) = src.consts.get(k) {
            insert_sym(
                &mut dst.consts,
                Some(&mut dst.consts_order),
                k.clone(),
                v.clone(),
                "const",
                use_span,
                false,
            )?;
        }
    }
    for k in &src.enums_order {
        if let Some(v) = src.enums.get(k) {
            insert_sym(
                &mut dst.enums,
                Some(&mut dst.enums_order),
                k.clone(),
                v.clone(),
                "enum",
                use_span,
                false,
            )?;
        }
    }
    for (k, v) in &src.structs {
        insert_sym(&mut dst.structs, None, k.clone(), v.clone(), "struct", use_span, false)?;
    }
    for k in &src.layouts_order {
        if let Some(v) = src.layouts.get(k) {
            insert_sym(
                &mut dst.layouts,
                Some(&mut dst.layouts_order),
                k.clone(),
                v.clone(),
                "layout",
                use_span,
                false,
            )?;
        }
    }
    for (k, v) in &src.formas {
        insert_sym(&mut dst.formas, None, k.clone(), v.clone(), "forma", use_span, false)?;
    }
    for (k, v) in &src.namespaces {
        insert_namespace(&mut dst.namespaces, k.clone(), v.clone(), use_span)?;
    }
    Ok(())
}

pub(super) fn build_unit_tables(graph: &UnitGraph) -> Result<Vec<UnitTables>, InfimumDiagnostic> {
    let mut tables = vec![UnitTables::default(); graph.units.len()];
    let mut global_exports: BTreeMap<String, (String, Span)> = BTreeMap::new();

    for &uid in &graph.topo_order {
        let u = &graph.units[uid];

        // Inline `mod` inherits parent unit's visible scope before adding its own uses/decls
        if let Some(parent_uid) = u.parent {
            tables[uid].scope = tables[parent_uid].scope.clone();
        }

        // 1. Register own declarations (zero shadowing — even inside inline `mod`)
        {
            let ut = &mut tables[uid];
            for item in &u.items {
                match item {
                    RawItem::Const(rc) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rc.name.clone(),
                            canonical_name: unit_canonical_name(u, &rc.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rc.span,
                        };
                        insert_sym(
                            &mut ut.scope.consts,
                            Some(&mut ut.scope.consts_order),
                            rc.name.clone(),
                            sym.clone(),
                            "const",
                            rc.span,
                            false,
                        )?;
                        if rc.is_pub {
                            insert_sym(
                                &mut ut.exports.consts,
                                Some(&mut ut.exports.consts_order),
                                rc.name.clone(),
                                sym,
                                "const",
                                rc.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Enum(re) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: re.name.clone(),
                            canonical_name: unit_canonical_name(u, &re.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: re.span,
                        };
                        insert_sym(
                            &mut ut.scope.enums,
                            Some(&mut ut.scope.enums_order),
                            re.name.clone(),
                            sym.clone(),
                            "enum",
                            re.span,
                            false,
                        )?;
                        if re.is_pub {
                            insert_sym(
                                &mut ut.exports.enums,
                                Some(&mut ut.exports.enums_order),
                                re.name.clone(),
                                sym,
                                "enum",
                                re.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Struct(rs) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rs.name.clone(),
                            canonical_name: unit_canonical_name(u, &rs.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rs.span,
                        };
                        insert_sym(
                            &mut ut.scope.structs,
                            None,
                            rs.name.clone(),
                            sym.clone(),
                            "struct",
                            rs.span,
                            false,
                        )?;
                        if rs.is_pub {
                            insert_sym(
                                &mut ut.exports.structs,
                                None,
                                rs.name.clone(),
                                sym,
                                "struct",
                                rs.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Layout(rl) => {
                        let sym = SymRef {
                            owner: uid,
                            orig_name: rl.name.clone(),
                            canonical_name: unit_canonical_name(u, &rl.name, false),
                            decl_file: u.display_label.clone(),
                            decl_span: rl.span,
                        };
                        insert_sym(
                            &mut ut.scope.layouts,
                            Some(&mut ut.scope.layouts_order),
                            rl.name.clone(),
                            sym.clone(),
                            "layout",
                            rl.span,
                            false,
                        )?;
                        if rl.is_pub {
                            insert_sym(
                                &mut ut.exports.layouts,
                                Some(&mut ut.exports.layouts_order),
                                rl.name.clone(),
                                sym,
                                "layout",
                                rl.span,
                                false,
                            )?;
                        }
                    }
                    RawItem::Mod(_) => {}
                }
            }

            let multi_unit = graph.units.len() > 1;
            for rf in &u.formas {
                let sym = SymRef {
                    owner: uid,
                    orig_name: rf.name.clone(),
                    canonical_name: unit_fn_canonical_name(u, &rf.name, rf.exported, multi_unit),
                    decl_file: u.display_label.clone(),
                    decl_span: rf.span,
                };
                insert_sym(&mut ut.scope.formas, None, rf.name.clone(), sym.clone(), "forma", rf.span, false)?;
                insert_sym(&mut ut.scope.fns, None, rf.name.clone(), sym.clone(), "function", rf.span, false)?;
                if rf.is_pub {
                    insert_sym(&mut ut.exports.formas, None, rf.name.clone(), sym.clone(), "forma", rf.span, false)?;
                    insert_sym(&mut ut.exports.fns, None, rf.name.clone(), sym, "function", rf.span, false)?;
                }
            }

            for rf in &u.functions {
                if rf.exported {
                    if let Some((prev_file, prev_span)) = global_exports.get(&rf.name) {
                        return Err(InfimumDiagnostic::new(
                            "E024_DUPLICATE_SYMBOL",
                            "resolve",
                            format!(
                                "Duplicate Wasm export function '{}': defined at {}:{}:{} and {}:{}:{}",
                                rf.name, prev_file, prev_span.line, prev_span.col, u.display_label, rf.span.line, rf.span.col
                            ),
                            rf.span,
                            "Wasm 'export fn' names must be globally unique across the capsule",
                        ));
                    }
                    global_exports.insert(rf.name.clone(), (u.display_label.clone(), rf.span));
                }
                let sym = SymRef {
                    owner: uid,
                    orig_name: rf.name.clone(),
                    canonical_name: unit_fn_canonical_name(u, &rf.name, rf.exported, multi_unit),
                    decl_file: u.display_label.clone(),
                    decl_span: rf.span,
                };
                insert_sym(&mut ut.scope.fns, None, rf.name.clone(), sym.clone(), "function", rf.span, false)?;
                if rf.is_pub {
                    insert_sym(&mut ut.exports.fns, None, rf.name.clone(), sym, "function", rf.span, false)?;
                }
            }
        }

        // 2. Process all `use` / `pub use` directives (targets already built in topo_order)
        for (raw_use, dep_uid) in &u.uses {
            let dep_exports = tables[*dep_uid].exports.clone();
            let dep_scope = tables[*dep_uid].scope.clone();
            let dep_label = graph.units[*dep_uid].display_label.clone();
            let ut = &mut tables[uid];
            match &raw_use.kind {
                RawUseKind::All => {
                    merge_table_all(&mut ut.scope, &dep_exports, raw_use.span)?;
                    if raw_use.is_pub {
                        merge_table_all(&mut ut.exports, &dep_exports, raw_use.span)?;
                    }
                }
                RawUseKind::Alias(alias, alias_span) => {
                    let ns = NamespaceRef {
                        target_uid: *dep_uid,
                        decl_file: u.display_label.clone(),
                        decl_span: *alias_span,
                    };
                    insert_namespace(&mut ut.scope.namespaces, alias.clone(), ns.clone(), *alias_span)?;
                    if raw_use.is_pub {
                        insert_namespace(&mut ut.exports.namespaces, alias.clone(), ns, *alias_span)?;
                    }
                }
                RawUseKind::Selective(items) => {
                    for item in items {
                        let local_name = item.alias.as_deref().unwrap_or(&item.name).to_string();
                        let is_al = item.alias.is_some();
                        let mut matched_any = false;

                        if let Some(s) = dep_exports.fns.get(&item.name) {
                            insert_sym(&mut ut.scope.fns, None, local_name.clone(), s.clone(), "function", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.fns, None, local_name.clone(), s.clone(), "function", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.consts.get(&item.name) {
                            insert_sym(&mut ut.scope.consts, Some(&mut ut.scope.consts_order), local_name.clone(), s.clone(), "const", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.consts, Some(&mut ut.exports.consts_order), local_name.clone(), s.clone(), "const", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.enums.get(&item.name) {
                            insert_sym(&mut ut.scope.enums, Some(&mut ut.scope.enums_order), local_name.clone(), s.clone(), "enum", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.enums, Some(&mut ut.exports.enums_order), local_name.clone(), s.clone(), "enum", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.structs.get(&item.name) {
                            insert_sym(&mut ut.scope.structs, None, local_name.clone(), s.clone(), "struct", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.structs, None, local_name.clone(), s.clone(), "struct", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.layouts.get(&item.name) {
                            insert_sym(&mut ut.scope.layouts, Some(&mut ut.scope.layouts_order), local_name.clone(), s.clone(), "layout", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.layouts, Some(&mut ut.exports.layouts_order), local_name.clone(), s.clone(), "layout", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(s) = dep_exports.formas.get(&item.name) {
                            insert_sym(&mut ut.scope.formas, None, local_name.clone(), s.clone(), "forma", item.span, is_al)?;
                            if raw_use.is_pub {
                                insert_sym(&mut ut.exports.formas, None, local_name.clone(), s.clone(), "forma", item.span, is_al)?;
                            }
                            matched_any = true;
                        }
                        if let Some(ns) = dep_exports.namespaces.get(&item.name) {
                            insert_namespace(&mut ut.scope.namespaces, local_name.clone(), ns.clone(), item.span)?;
                            if raw_use.is_pub {
                                insert_namespace(&mut ut.exports.namespaces, local_name.clone(), ns.clone(), item.span)?;
                            }
                            matched_any = true;
                        }

                        if !matched_any {
                            let is_private_in_dep = dep_scope.fns.contains_key(&item.name)
                                || dep_scope.consts.contains_key(&item.name)
                                || dep_scope.enums.contains_key(&item.name)
                                || dep_scope.structs.contains_key(&item.name)
                                || dep_scope.layouts.contains_key(&item.name)
                                || dep_scope.formas.contains_key(&item.name)
                                || dep_scope.namespaces.contains_key(&item.name);
                            if is_private_in_dep {
                                return Err(InfimumDiagnostic::new(
                                    "E068_PRIVATE_ITEM",
                                    "resolve",
                                    format!("Symbol '{}' in '{}' is private; mark it 'pub' to export", item.name, dep_label),
                                    item.span,
                                    format!("Add 'pub' to '{}' in '{}'", item.name, dep_label),
                                ));
                            } else {
                                return Err(InfimumDiagnostic::new(
                                    "E067_UNKNOWN_IMPORT",
                                    "resolve",
                                    format!("Symbol '{}' not found in '{}'", item.name, dep_label),
                                    item.span,
                                    format!("Check the exported 'pub' declarations in '{}'", dep_label),
                                ));
                            }
                        }
                    }
                }
            }
        }

        // 3. If this is an inline `mod`, bind its namespace into parent's scope (and exports if `pub mod`)
        if let (Some(parent_uid), Some((mname, is_pub, mspan))) = (u.parent, &u.inline_mod) {
            let ns = NamespaceRef {
                target_uid: uid,
                decl_file: u.display_label.clone(),
                decl_span: *mspan,
            };
            insert_namespace(&mut tables[parent_uid].scope.namespaces, mname.clone(), ns.clone(), *mspan)?;
            if *is_pub {
                insert_namespace(&mut tables[parent_uid].exports.namespaces, mname.clone(), ns, *mspan)?;
            }
        }
    }

    Ok(tables)
}

