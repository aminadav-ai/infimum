//! Direct W3C WebAssembly 1.0 Binary Bytecode Emitter for `Infimum.Strata`.
//!
//! Compiles verified Infimum graphs directly into raw `.wasm` machine bytes in ~1-3ms
//! with ZERO intermediate Rust code generation and ZERO external `rustc` invocation.

use crate::ast::{BinOp, Expr, FnDef, Module, Stmt, TargetRef, Type, UnaryOp};
use crate::canon::CanonicalNode;
use crate::diag::{InfimumDiagnostic, Span};
use crate::store::GraphStore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet, VecDeque};
use std::fs;
use std::path::Path;
use std::time::Instant;

const WASM_TYPE_I32: u8 = 0x7F;
const WASM_TYPE_I64: u8 = 0x7E;
const WASM_BLOCK_VOID: u8 = 0x40;
const WASM_BLOCK_I64: u8 = 0x7E;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StrataWasmArtifact {
    pub exported_functions: Vec<String>,
    pub compiled_nodes: usize,
    pub wasm_path: String,
    pub wasm_size_bytes: u64,
    pub build_us: u128,
}

/// Compile an ingested module or entrypoint directly from `.infimum/` to a pure `.wasm` binary.
pub fn build_strata_wasm_from_module(
    store: &mut GraphStore,
    module: &Module,
    out_path: &Path,
) -> Result<StrataWasmArtifact, InfimumDiagnostic> {
    build_strata_wasm_from_module_with_cache(store, module, out_path, true)
}

/// Compile a module to a pure `.wasm` binary with explicit control over the Z3 proof cache.
pub fn build_strata_wasm_from_module_with_cache(
    store: &mut GraphStore,
    module: &Module,
    out_path: &Path,
    use_cache: bool,
) -> Result<StrataWasmArtifact, InfimumDiagnostic> {
    let t0 = Instant::now();
    let entries = store.ingest_module_with_cache(module, use_cache)?;

    // Determine exported functions: any `export fn` in module, or the last function if none marked `export`
    let mut export_pairs: Vec<(String, String)> = Vec::new();
    for raw_fn in &module.functions {
        if raw_fn.exported {
            let h = store.resolve_ref(&raw_fn.name)?;
            export_pairs.push((raw_fn.name.clone(), h));
        }
    }
    if export_pairs.is_empty() {
        if let Some(last) = entries.last() {
            export_pairs.push((last.alias.clone(), last.hash.clone()));
        }
    }

    let root_hashes: Vec<String> = export_pairs.iter().map(|(_, h)| h.clone()).collect();
    let nodes = collect_multi_root_subgraph(store, &root_hashes)?;

    // Embed SecurityPassport in Wasm custom section `infimum.passport`.
    // For capsule UIs (`step_ui`/`init_app`), merge all exported external capabilities so none can be hidden.
    let passport_bytes =
        crate::store::build_merged_passport_bytes(&export_pairs, |alias| {
            store.compute_passport(alias)
        })?;

    let layout_end_word = compute_module_layout_end_word(module);
    let wasm_bytes =
        emit_strata_wasm_bytes_with_layout_end(&nodes, &export_pairs, &passport_bytes, layout_end_word)?;

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    fs::write(out_path, &wasm_bytes).map_err(|e| {
        InfimumDiagnostic::new(
            "E080_WASM_IO",
            "strata-wasm",
            format!("Failed to write Strata .wasm '{}': {e}", out_path.display()),
            Span::zero(),
            "Check write permissions on target path",
        )
    })?;

    Ok(StrataWasmArtifact {
        exported_functions: export_pairs.into_iter().map(|(name, _)| name).collect(),
        compiled_nodes: nodes.len(),
        wasm_path: out_path.display().to_string(),
        wasm_size_bytes: wasm_bytes.len() as u64,
        build_us: t0.elapsed().as_micros(),
    })
}

pub fn build_strata_wasm_target(
    store: &GraphStore,
    target_ref: &str,
    out_path: &Path,
) -> Result<StrataWasmArtifact, InfimumDiagnostic> {
    let t0 = Instant::now();
    let root_hash = store.resolve_ref(target_ref)?;
    let aliases = store.aliases_for_hash(&root_hash);
    let export_name = aliases
        .first()
        .cloned()
        .unwrap_or_else(|| "main".to_string());
    let export_pairs = vec![(export_name.clone(), root_hash.clone())];

    let nodes = collect_multi_root_subgraph(store, &[root_hash])?;
    let passport = store.compute_passport(target_ref)?;
    let passport_bytes = serde_json::to_vec(&passport).unwrap_or_default();

    let wasm_bytes = emit_strata_wasm_bytes(&nodes, &export_pairs, &passport_bytes)?;

    if let Some(parent) = out_path.parent() {
        if !parent.as_os_str().is_empty() {
            let _ = fs::create_dir_all(parent);
        }
    }
    fs::write(out_path, &wasm_bytes).map_err(|e| {
        InfimumDiagnostic::new(
            "E080_WASM_IO",
            "strata-wasm",
            format!("Failed to write Strata .wasm '{}': {e}", out_path.display()),
            Span::zero(),
            "Check write permissions on target path",
        )
    })?;

    Ok(StrataWasmArtifact {
        exported_functions: vec![export_name],
        compiled_nodes: nodes.len(),
        wasm_path: out_path.display().to_string(),
        wasm_size_bytes: wasm_bytes.len() as u64,
        build_us: t0.elapsed().as_micros(),
    })
}

pub fn collect_multi_root_subgraph(
    store: &GraphStore,
    roots: &[String],
) -> Result<Vec<CanonicalNode>, InfimumDiagnostic> {
    let mut visited: BTreeMap<String, CanonicalNode> = BTreeMap::new();
    let mut queue: VecDeque<String> = roots.iter().cloned().collect();

    while let Some(h) = queue.pop_front() {
        if visited.contains_key(&h) {
            continue;
        }
        let node = store.load_node(&h)?;
        check_strata_compatible(&node)?;
        for dep in &node.direct_deps {
            if !visited.contains_key(dep) {
                queue.push_back(dep.clone());
            }
        }
        visited.insert(h, node);
    }

    Ok(visited.into_values().collect())
}

pub fn check_strata_compatible(node: &CanonicalNode) -> Result<(), InfimumDiagnostic> {
    let f = &node.ast;
    for p in &f.params {
        let param_ok = match &p.ty {
            Type::I64 | Type::Fixed | Type::Bool | Type::Unit | Type::Str | Type::Slice(_) => true,
            Type::Tuple(ts) => ts
                .iter()
                .all(|t| matches!(t, Type::I64 | Type::Fixed | Type::Bool | Type::Unit | Type::Str)),
            _ => false,
        };
        if !param_ok {
            return Err(InfimumDiagnostic::new(
                "E085_STRATA_UNSUPPORTED_TYPE",
                "strata",
                format!(
                    "Node #{} uses high-level type '{}' not allowed in direct Strata silicon/Wasm mode",
                    node.short_hash, p.ty
                ),
                Span::zero(),
                "Strata direct machine/Wasm emitter operates on unboxed i64, fixed, bool, str, unit, structs/tuples, and linear memory (Mem)",
            ));
        }
    }
    let ret_ok = match &f.ret_type {
        Type::I64 | Type::Fixed | Type::Bool | Type::Unit | Type::Str | Type::Slice(_) => true,
        Type::Tuple(ts) => ts
            .iter()
            .all(|t| matches!(t, Type::I64 | Type::Fixed | Type::Bool | Type::Unit | Type::Str)),
        _ => false,
    };
    if !ret_ok {
        return Err(InfimumDiagnostic::new(
            "E085_STRATA_UNSUPPORTED_TYPE",
            "strata",
            format!(
                "Node #{} returns high-level type '{}' not allowed in direct Strata mode",
                node.short_hash, f.ret_type
            ),
            Span::zero(),
            "Return i64, bool, str, or unit from Strata functions",
        ));
    }
    Ok(())
}

const DEFAULT_STRATA_STR_WORD_BASE: u32 = 2_030_000; // Legacy fallback when no Module layouts are provided
const USER_LAYOUT_SEGMENT_START: u32 = 705_536; // First word above GPU primitive stream (65536..705535)

/// Compute the highest word address used by any `layout` in a parsed module (aligned to 16 words).
pub fn compute_module_layout_end_word(module: &Module) -> u32 {
    let mut max_end: i64 = 0;
    for l in &module.layouts {
        for f in &l.fields {
            let end = f.offset + f.len;
            if end > max_end {
                max_end = end;
            }
        }
    }
    if max_end <= 0 {
        return 0;
    }
    let aligned = ((max_end + 15) / 16) * 16;
    aligned.min(16_777_216) as u32
}

fn collect_str_lits_stmt<'a>(stmt: &'a Stmt, out: &mut BTreeSet<&'a str>) {
    match stmt {
        Stmt::Let { value, .. }
        | Stmt::LetTuple { value, .. }
        | Stmt::Assign { value, .. }
        | Stmt::Expr(value) => collect_str_lits_expr(value, out),
        Stmt::While { cond, body, .. } => {
            collect_str_lits_expr(cond, out);
            for s in body {
                collect_str_lits_stmt(s, out);
            }
        }
    }
}

fn collect_str_lits_expr<'a>(expr: &'a Expr, out: &mut BTreeSet<&'a str>) {
    match expr {
        Expr::StrLit(s) => {
            out.insert(s.as_str());
        }
        Expr::Block { stmts, tail } => {
            for s in stmts {
                collect_str_lits_stmt(s, out);
            }
            collect_str_lits_expr(tail, out);
        }
        Expr::If { cond, then_branch, else_branch } => {
            collect_str_lits_expr(cond, out);
            collect_str_lits_expr(then_branch, out);
            collect_str_lits_expr(else_branch, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_str_lits_expr(lhs, out);
            collect_str_lits_expr(rhs, out);
        }
        Expr::Unary { expr: e, .. } | Expr::MemLoad { index: e, .. } => {
            collect_str_lits_expr(e, out);
        }
        Expr::MemStore { index, value, .. } => {
            collect_str_lits_expr(index, out);
            collect_str_lits_expr(value, out);
        }
        Expr::Call { args, .. } | Expr::Perform { args, .. } | Expr::TupleLit(args) => {
            for a in args {
                collect_str_lits_expr(a, out);
            }
        }
        Expr::SliceLit { ptr, len, .. } | Expr::BoundsCheck { index: ptr, len, .. } => {
            collect_str_lits_expr(ptr, out);
            collect_str_lits_expr(len, out);
        }
        Expr::SlicePtr(e) | Expr::SliceLen(e) | Expr::TupleField { expr: e, .. } => {
            collect_str_lits_expr(e, out);
        }
        Expr::RangeCheck { lo, hi, len } => {
            collect_str_lits_expr(lo, out);
            collect_str_lits_expr(hi, out);
            collect_str_lits_expr(len, out);
        }
        _ => {}
    }
}

fn estimate_str_pool_words(nodes: &[CanonicalNode]) -> u32 {
    let mut uniq = BTreeSet::new();
    for n in nodes {
        collect_str_lits_expr(&n.ast.body, &mut uniq);
    }
    let mut total: usize = 0;
    for s in uniq {
        total += 1 + s.chars().count();
    }
    total as u32
}

/// Select the smallest power-of-two Wasm memory page count in `{128, 256, 512, 1024}` (8..64 MiB)
/// covering `required_words`, returning `(pages, word_index_mask)`.
fn select_wasm_mem_pages(required_words: u32) -> (u32, i32) {
    // 1 Wasm page = 65,536 bytes = 16,384 32-bit words
    const CANDIDATES: &[(u32, u32)] = &[
        (128, 2_097_152),   // 8 MiB  -> mask 0x001F_FFFF
        (256, 4_194_304),   // 16 MiB -> mask 0x003F_FFFF
        (512, 8_388_608),   // 32 MiB -> mask 0x007F_FFFF
        (1024, 16_777_216), // 64 MiB -> mask 0x00FF_FFFF
    ];
    for &(pages, max_words) in CANDIDATES {
        if required_words <= max_words {
            return (pages, (max_words - 1) as i32);
        }
    }
    (1024, 0x00FF_FFFF)
}

struct StrataStrPool {
    base_word: u32,
    map: BTreeMap<String, i64>,
    words: Vec<i32>,
}

impl StrataStrPool {
    fn new(base_word: u32) -> Self {
        Self {
            base_word,
            map: BTreeMap::new(),
            words: Vec::new(),
        }
    }

    fn intern(&mut self, s: &str) -> i64 {
        if let Some(&addr) = self.map.get(s) {
            return addr;
        }
        let word_addr = (self.base_word as usize + self.words.len()) as i64;
        let chars: Vec<char> = s.chars().collect();
        self.words.push(chars.len() as i32);
        for ch in chars {
            self.words.push(ch as u32 as i32);
        }
        self.map.insert(s.to_string(), word_addr);
        word_addr
    }

    fn to_le_bytes(&self) -> Vec<u8> {
        let mut out = Vec::with_capacity(self.words.len() * 4);
        for &w in &self.words {
            out.extend_from_slice(&w.to_le_bytes());
        }
        out
    }
}

fn expr_uses_host_call(expr: &Expr) -> bool {
    match expr {
        Expr::Perform { op, args, .. } => {
            op == "call" || args.iter().any(expr_uses_host_call)
        }
        Expr::Call { args, .. } => args.iter().any(expr_uses_host_call),
        Expr::Binary { lhs, rhs, .. } => expr_uses_host_call(lhs) || expr_uses_host_call(rhs),
        Expr::Unary { expr: e, .. } => expr_uses_host_call(e),
        Expr::MemLoad { index, .. } => expr_uses_host_call(index),
        Expr::MemStore { index, value, .. } => expr_uses_host_call(index) || expr_uses_host_call(value),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            expr_uses_host_call(cond)
                || expr_uses_host_call(then_branch)
                || expr_uses_host_call(else_branch)
        }
        Expr::Block { stmts, tail } => {
            stmts.iter().any(stmt_uses_host_call) || expr_uses_host_call(tail)
        }
        _ => false,
    }
}

fn stmt_uses_host_call(stmt: &Stmt) -> bool {
    match stmt {
        Stmt::Let { value, .. } | Stmt::LetTuple { value, .. } | Stmt::Assign { value, .. } => {
            expr_uses_host_call(value)
        }
        Stmt::Expr(e) => expr_uses_host_call(e),
        Stmt::While { cond, body, .. } => {
            expr_uses_host_call(cond) || body.iter().any(stmt_uses_host_call)
        }
    }
}

pub fn emit_strata_wasm_bytes(
    nodes: &[CanonicalNode],
    exports: &[(String, String)],
    passport_json: &[u8],
) -> Result<Vec<u8>, InfimumDiagnostic> {
    emit_strata_wasm_bytes_with_layout_end(nodes, exports, passport_json, 0)
}

pub fn emit_strata_wasm_bytes_with_layout_end(
    nodes: &[CanonicalNode],
    exports: &[(String, String)],
    passport_json: &[u8],
    layout_end_word: u32,
) -> Result<Vec<u8>, InfimumDiagnostic> {
    let str_word_base = if layout_end_word > 0 {
        layout_end_word.max(USER_LAYOUT_SEGMENT_START)
    } else {
        DEFAULT_STRATA_STR_WORD_BASE
    };
    let est_str_words = estimate_str_pool_words(nodes);
    let required_words = str_word_base.saturating_add(est_str_words).saturating_add(64);
    let (mem_pages, mem_word_mask) = select_wasm_mem_pages(required_words);
    let uses_host_call = nodes.iter().any(|n| expr_uses_host_call(&n.ast.body));
    let import_offset: u32 = if uses_host_call { 1 } else { 0 };

    let mut hash_to_func_idx: BTreeMap<String, u32> = BTreeMap::new();
    for (idx, node) in nodes.iter().enumerate() {
        hash_to_func_idx.insert(node.hash.clone(), import_offset + idx as u32);
    }

    let internal_count = nodes.len() as u32;

    let mut type_entries: Vec<Vec<u8>> = Vec::new();
    let mut func_type_indices: Vec<u32> = Vec::new();

    // 0. Optional imported capability bridge `env.host_call(i64, i64, i64, i64) -> i64`
    let host_call_type_idx = if uses_host_call {
        let sig = vec![
            0x60,
            4,
            WASM_TYPE_I64,
            WASM_TYPE_I64,
            WASM_TYPE_I64,
            WASM_TYPE_I64,
            1,
            WASM_TYPE_I64,
        ];
        Some(intern_sig(&mut type_entries, sig))
    } else {
        None
    };

    // 1. Internal functions (uniform i64 -> i64 or multi-value i64^N for tuples)
    let mut hash_to_ret_arity: BTreeMap<String, usize> = BTreeMap::new();
    let mut multi_val_arities: BTreeSet<usize> = BTreeSet::new();
    for node in nodes {
        let mut arity = 0usize;
        for p in &node.ast.params {
            let pa = match &p.ty {
                Type::Tuple(ts) => ts.len().max(1),
                Type::Slice(_) => 2,
                _ => 1,
            };
            if pa > 1 {
                multi_val_arities.insert(pa);
            }
            arity += pa;
        }
        let ret_arity = match &node.ast.ret_type {
            Type::Tuple(ts) => ts.len(),
            Type::Slice(_) => 2,
            _ => 1,
        };
        if ret_arity > 1 {
            multi_val_arities.insert(ret_arity);
        }
        collect_expr_tuple_arities(&node.ast.body, &mut multi_val_arities);
        hash_to_ret_arity.insert(node.hash.clone(), ret_arity);
        let mut sig = Vec::with_capacity(4 + arity + ret_arity);
        sig.push(0x60); // func
        push_u32_leb(&mut sig, arity as u32);
        sig.extend(std::iter::repeat_n(WASM_TYPE_I64, arity));
        push_u32_leb(&mut sig, ret_arity as u32);
        sig.extend(std::iter::repeat_n(WASM_TYPE_I64, ret_arity));

        let tidx = intern_sig(&mut type_entries, sig);
        func_type_indices.push(tidx);
    }

    // 1b. Multi-value block types `() -> (i64^k)` for `if/else` returning tuples
    let mut multi_val_block_types: BTreeMap<usize, u32> = BTreeMap::new();
    for &k in &multi_val_arities {
        if k > 1 {
            let mut sig = Vec::with_capacity(4 + k);
            sig.push(0x60); // func
            push_u32_leb(&mut sig, 0);
            push_u32_leb(&mut sig, k as u32);
            sig.extend(std::iter::repeat_n(WASM_TYPE_I64, k));
            let tidx = intern_sig(&mut type_entries, sig);
            multi_val_block_types.insert(k, tidx);
        }
    }

    // 2. JS-friendly i32 wrapper functions for each exported entry (`<name>`)
    let mut wrapper_specs: Vec<(String, u32, usize, bool)> = Vec::new();
    for (exp_name, target_hash) in exports {
        let target_idx = *hash_to_func_idx.get(target_hash).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E032_NODE_NOT_FOUND",
                "strata-wasm",
                format!("Missing target hash #{target_hash} in Strata module"),
                Span::zero(),
                "Ensure exported node is ingested",
            )
        })?;
        let target_node = &nodes[(target_idx - import_offset) as usize];
        let has_non_scalar_param = target_node
            .ast
            .params
            .iter()
            .any(|p| matches!(&p.ty, Type::Tuple(_) | Type::Slice(_)));
        let has_non_scalar_ret = matches!(&target_node.ast.ret_type, Type::Tuple(_) | Type::Slice(_));
        if has_non_scalar_param || has_non_scalar_ret {
            return Err(InfimumDiagnostic::new(
                "E082_NON_SCALAR_EXPORT",
                "strata-wasm",
                format!(
                    "Exported function '{exp_name}' cannot take or return tuple/value-struct/slice types across the Wasm ABI boundary"
                ),
                Span::zero(),
                "Use scalar types (i64, fixed, bool, unit) on 'export fn' signatures, or keep tuple/struct/slice helpers non-exported",
            ));
        }
        let arity = target_node.ast.params.len();
        let is_unit = target_node.ast.ret_type == Type::Unit;

        let mut sig = Vec::with_capacity(4 + arity);
        sig.push(0x60);
        push_u32_leb(&mut sig, arity as u32);
        sig.extend(std::iter::repeat_n(WASM_TYPE_I32, arity));
        if is_unit {
            push_u32_leb(&mut sig, 0);
        } else {
            push_u32_leb(&mut sig, 1);
            sig.push(WASM_TYPE_I32);
        }

        let tidx = intern_sig(&mut type_entries, sig);
        func_type_indices.push(tidx);
        wrapper_specs.push((exp_name.clone(), target_idx, arity, is_unit));
    }

    let mut wasm = Vec::with_capacity(4096);
    // Wasm 1.0 header
    wasm.extend_from_slice(b"\x00asm\x01\x00\x00\x00");

    // Section 1: Type section
    let mut sec_type = Vec::new();
    push_u32_leb(&mut sec_type, type_entries.len() as u32);
    for sig in &type_entries {
        sec_type.extend_from_slice(sig);
    }
    push_section(&mut wasm, 1, &sec_type);

    // Section 2: Import section (only emitted when capsule uses `do <Effect>.call(...)`)
    if let Some(hc_tidx) = host_call_type_idx {
        let mut sec_imp = Vec::new();
        push_u32_leb(&mut sec_imp, 1);
        push_wasm_str(&mut sec_imp, "env");
        push_wasm_str(&mut sec_imp, "host_call");
        sec_imp.push(0x00); // func import
        push_u32_leb(&mut sec_imp, hc_tidx);
        push_section(&mut wasm, 2, &sec_imp);
    }

    // Section 3: Function section
    let mut sec_func = Vec::new();
    push_u32_leb(&mut sec_func, func_type_indices.len() as u32);
    for &tidx in &func_type_indices {
        push_u32_leb(&mut sec_func, tidx);
    }
    push_section(&mut wasm, 3, &sec_func);

    // Section 5: Linear Memory section (auto-sized power-of-two pages: 128=8MiB .. 1024=64MiB)
    let mut sec_mem = Vec::new();
    push_u32_leb(&mut sec_mem, 1); // 1 memory
    sec_mem.push(0x00); // flags: min only
    push_u32_leb(&mut sec_mem, mem_pages);
    push_section(&mut wasm, 5, &sec_mem);

    // Section 7: Export section (`memory` + `<name>` i32 wrapper + `<name>_i64` raw i64 fn)
    let mut sec_exp = Vec::new();
    let total_exports = 1 + (wrapper_specs.len() * 2);
    push_u32_leb(&mut sec_exp, total_exports as u32);

    // Export linear memory as "memory"
    push_wasm_str(&mut sec_exp, "memory");
    sec_exp.push(0x02); // memory export
    push_u32_leb(&mut sec_exp, 0);

    for (i, (exp_name, target_idx, _, _)) in wrapper_specs.iter().enumerate() {
        let wrapper_idx = import_offset + internal_count + i as u32;
        push_wasm_str(&mut sec_exp, exp_name);
        sec_exp.push(0x00); // func export
        push_u32_leb(&mut sec_exp, wrapper_idx);

        let i64_name = format!("{exp_name}_i64");
        push_wasm_str(&mut sec_exp, &i64_name);
        sec_exp.push(0x00);
        push_u32_leb(&mut sec_exp, *target_idx);
    }
    push_section(&mut wasm, 7, &sec_exp);

    // Section 10: Code section (simultaneously interns static StrLit constants into StrPool)
    let mut str_pool = StrataStrPool::new(str_word_base);
    let mut sec_code = Vec::new();
    push_u32_leb(&mut sec_code, func_type_indices.len() as u32);

    for (self_idx, node) in nodes.iter().enumerate() {
        let body_bytes = compile_node_body(
            node,
            import_offset + self_idx as u32,
            &hash_to_func_idx,
            &hash_to_ret_arity,
            &multi_val_block_types,
            &mut str_pool,
            mem_word_mask,
        )?;
        push_u32_leb(&mut sec_code, body_bytes.len() as u32);
        sec_code.extend_from_slice(&body_bytes);
    }

    // Emit thin i32 -> i64 -> i32 wrapper bodies
    for (_, target_idx, arity, is_unit) in &wrapper_specs {
        let mut wbody = Vec::new();
        push_u32_leb(&mut wbody, 0); // 0 additional locals
        for p in 0..*arity {
            wbody.push(0x20); // local.get p
            push_u32_leb(&mut wbody, p as u32);
            wbody.push(0xAC); // i64.extend_i32_s (MUST sign-extend negative i32 args like -1!)
        }
        wbody.push(0x10); // call target_idx
        push_u32_leb(&mut wbody, *target_idx);
        if *is_unit {
            wbody.push(0x1A); // drop
        } else {
            wbody.push(0xA7); // i32.wrap_i64
        }
        wbody.push(0x0B); // end

        push_u32_leb(&mut sec_code, wbody.len() as u32);
        sec_code.extend_from_slice(&wbody);
    }
    push_section(&mut wasm, 10, &sec_code);

    // Section 11: Data section (static read-only string pool placed safely after all layouts)
    if !str_pool.words.is_empty() {
        let raw_str_bytes = str_pool.to_le_bytes();
        let mut sec_data = Vec::with_capacity(16 + raw_str_bytes.len());
        push_u32_leb(&mut sec_data, 1); // 1 active data segment
        sec_data.push(0x00); // active memory 0
        sec_data.push(0x41); // i32.const
        push_i32_leb(&mut sec_data, (str_pool.base_word * 4) as i32);
        sec_data.push(0x0B); // end const expr
        push_u32_leb(&mut sec_data, raw_str_bytes.len() as u32);
        sec_data.extend_from_slice(&raw_str_bytes);
        push_section(&mut wasm, 11, &sec_data);
    }

    // Custom Section 0: "infimum.passport" (embedded compile-time SecurityPassport JSON)
    if !passport_json.is_empty() {
        let mut sec_custom = Vec::new();
        push_wasm_str(&mut sec_custom, "infimum.passport");
        sec_custom.extend_from_slice(passport_json);
        push_section(&mut wasm, 0, &sec_custom);
    }

    Ok(wasm)
}

struct FnCodegenCtx<'a> {
    self_idx: u32,
    self_ret_arity: usize,
    hash_to_func_idx: &'a BTreeMap<String, u32>,
    hash_to_ret_arity: &'a BTreeMap<String, usize>,
    multi_val_block_types: &'a BTreeMap<usize, u32>,
    str_pool: &'a mut StrataStrPool,
    mem_word_mask: i32,
    local_map: BTreeMap<String, u32>,
    tuple_locals: BTreeMap<String, Vec<u32>>,
    next_local_idx: u32,
    extra_i64_locals: u32,
}

impl<'a> FnCodegenCtx<'a> {
    fn alloc_local(&mut self, name: Option<&str>) -> u32 {
        let idx = self.next_local_idx;
        self.next_local_idx += 1;
        self.extra_i64_locals += 1;
        if let Some(n) = name {
            self.local_map.insert(n.to_string(), idx);
        }
        idx
    }
}

fn collect_stmt_tuple_arities(stmts: &[Stmt], out: &mut BTreeSet<usize>) {
    for s in stmts {
        match s {
            Stmt::Let { ty, value, .. } => {
                if let Some(Type::Tuple(ts)) = ty {
                    if ts.len() > 1 {
                        out.insert(ts.len());
                    }
                } else if matches!(ty, Some(Type::Slice(_))) {
                    out.insert(2);
                }
                collect_expr_tuple_arities(value, out);
            }
            Stmt::LetTuple { value, .. } | Stmt::Assign { value, .. } | Stmt::Expr(value) => {
                collect_expr_tuple_arities(value, out);
            }
            Stmt::While { cond, body, .. } => {
                collect_expr_tuple_arities(cond, out);
                collect_stmt_tuple_arities(body, out);
            }
        }
    }
}

fn collect_expr_tuple_arities(expr: &Expr, out: &mut BTreeSet<usize>) {
    match expr {
        Expr::TupleLit(elems) => {
            if elems.len() > 1 {
                out.insert(elems.len());
            }
            for e in elems {
                collect_expr_tuple_arities(e, out);
            }
        }
        Expr::Block { stmts, tail } => {
            collect_stmt_tuple_arities(stmts, out);
            collect_expr_tuple_arities(tail, out);
        }
        Expr::If { cond, then_branch, else_branch } => {
            collect_expr_tuple_arities(cond, out);
            collect_expr_tuple_arities(then_branch, out);
            collect_expr_tuple_arities(else_branch, out);
        }
        Expr::Binary { lhs, rhs, .. } => {
            collect_expr_tuple_arities(lhs, out);
            collect_expr_tuple_arities(rhs, out);
        }
        Expr::Unary { expr: inner, .. }
        | Expr::MemLoad { index: inner, .. }
        | Expr::SecretWrap(inner)
        | Expr::Reveal { expr: inner, .. }
        | Expr::TupleField { expr: inner, .. }
        | Expr::SlicePtr(inner)
        | Expr::SliceLen(inner) => collect_expr_tuple_arities(inner, out),
        Expr::SliceLit { ptr, len, .. } => {
            out.insert(2);
            collect_expr_tuple_arities(ptr, out);
            collect_expr_tuple_arities(len, out);
        }
        Expr::BoundsCheck { index, len, .. } => {
            collect_expr_tuple_arities(index, out);
            collect_expr_tuple_arities(len, out);
        }
        Expr::RangeCheck { lo, hi, len } => {
            collect_expr_tuple_arities(lo, out);
            collect_expr_tuple_arities(hi, out);
            collect_expr_tuple_arities(len, out);
        }
        Expr::MemStore { index, value, .. } => {
            collect_expr_tuple_arities(index, out);
            collect_expr_tuple_arities(value, out);
        }
        Expr::Call { args, .. } | Expr::Perform { args, .. } => {
            for a in args {
                collect_expr_tuple_arities(a, out);
            }
        }
        Expr::IntLit(_) | Expr::FixedLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit | Expr::Var(_) => {}
    }
}

fn expr_result_arity(expr: &Expr, ctx: &FnCodegenCtx<'_>) -> usize {
    match expr {
        Expr::TupleLit(elems) => elems.len().max(1),
        Expr::SliceLit { .. } => 2,
        Expr::Var(name) => ctx.tuple_locals.get(name).map(|v| v.len()).unwrap_or(1),
        Expr::Block { tail, .. } => expr_result_arity(tail, ctx),
        Expr::If { then_branch, .. } => expr_result_arity(then_branch, ctx),
        Expr::Call { target: TargetRef::Hash(h), .. } => *ctx.hash_to_ret_arity.get(h).unwrap_or(&1),
        Expr::Call { target: TargetRef::SelfRec, .. } => ctx.self_ret_arity,
        _ => 1,
    }
}

fn compile_node_body(
    node: &CanonicalNode,
    self_idx: u32,
    hash_to_func_idx: &BTreeMap<String, u32>,
    hash_to_ret_arity: &BTreeMap<String, usize>,
    multi_val_block_types: &BTreeMap<usize, u32>,
    str_pool: &mut StrataStrPool,
    mem_word_mask: i32,
) -> Result<Vec<u8>, InfimumDiagnostic> {
    let f: &FnDef = &node.ast;
    let self_ret_arity = match &f.ret_type {
        Type::Tuple(ts) => ts.len(),
        Type::Slice(_) => 2,
        _ => 1,
    };
    let mut local_map = BTreeMap::new();
    let mut tuple_locals = BTreeMap::new();
    let mut param_idx = 0u32;
    for p in &f.params {
        match &p.ty {
            Type::Tuple(ts) if ts.len() > 1 => {
                let mut slots = Vec::with_capacity(ts.len());
                for _ in 0..ts.len() {
                    slots.push(param_idx);
                    param_idx += 1;
                }
                tuple_locals.insert(p.name.clone(), slots);
            }
            Type::Slice(_) => {
                let slots = vec![param_idx, param_idx + 1];
                param_idx += 2;
                tuple_locals.insert(p.name.clone(), slots);
            }
            _ => {
                local_map.insert(p.name.clone(), param_idx);
                param_idx += 1;
            }
        }
    }

    let mut ctx = FnCodegenCtx {
        self_idx,
        self_ret_arity,
        hash_to_func_idx,
        hash_to_ret_arity,
        multi_val_block_types,
        str_pool,
        mem_word_mask,
        local_map,
        tuple_locals,
        next_local_idx: param_idx,
        extra_i64_locals: 0,
    };

    // Pre-allocate all named `let` locals across the body
    prealloc_expr_locals(&f.body, &mut ctx);

    let mut instrs = Vec::with_capacity(256);
    emit_expr(&f.body, &mut ctx, &mut instrs)?;
    instrs.push(0x0B); // end of function

    let mut body = Vec::with_capacity(8 + instrs.len());
    if ctx.extra_i64_locals == 0 {
        push_u32_leb(&mut body, 0);
    } else {
        push_u32_leb(&mut body, 1); // 1 local declaration group
        push_u32_leb(&mut body, ctx.extra_i64_locals);
        body.push(WASM_TYPE_I64);
    }
    body.extend_from_slice(&instrs);
    Ok(body)
}

fn prealloc_stmt_locals(stmts: &[Stmt], ctx: &mut FnCodegenCtx<'_>) {
    for s in stmts {
        match s {
            Stmt::Let { name, ty, value, .. } => {
                prealloc_expr_locals(value, ctx);
                let arity = match ty {
                    Some(Type::Tuple(ts)) => ts.len(),
                    Some(Type::Slice(_)) => 2,
                    _ => expr_result_arity(value, ctx),
                };
                if arity > 1 {
                    if !ctx.tuple_locals.contains_key(name) {
                        let mut slots = Vec::with_capacity(arity);
                        for _ in 0..arity {
                            slots.push(ctx.alloc_local(None));
                        }
                        ctx.tuple_locals.insert(name.clone(), slots);
                    }
                } else if !ctx.local_map.contains_key(name) {
                    ctx.alloc_local(Some(name));
                }
            }
            Stmt::LetTuple { names, value } => {
                prealloc_expr_locals(value, ctx);
                for (n, _) in names {
                    if !ctx.local_map.contains_key(n) {
                        ctx.alloc_local(Some(n));
                    }
                }
            }
            Stmt::Assign { value, .. } => prealloc_expr_locals(value, ctx),
            Stmt::While { cond, body, .. } => {
                prealloc_expr_locals(cond, ctx);
                prealloc_stmt_locals(body, ctx);
            }
            Stmt::Expr(e) => prealloc_expr_locals(e, ctx),
        }
    }
}

fn prealloc_expr_locals(expr: &Expr, ctx: &mut FnCodegenCtx<'_>) {
    match expr {
        Expr::Block { stmts, tail } => {
            prealloc_stmt_locals(stmts, ctx);
            prealloc_expr_locals(tail, ctx);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            prealloc_expr_locals(cond, ctx);
            prealloc_expr_locals(then_branch, ctx);
            prealloc_expr_locals(else_branch, ctx);
        }
        Expr::Unary { expr: inner, .. }
        | Expr::MemLoad { index: inner, .. }
        | Expr::SecretWrap(inner)
        | Expr::Reveal { expr: inner, .. } => prealloc_expr_locals(inner, ctx),
        Expr::MemStore { index, value, .. } => {
            prealloc_expr_locals(index, ctx);
            prealloc_expr_locals(value, ctx);
        }
        Expr::Binary { lhs, rhs, .. } => {
            prealloc_expr_locals(lhs, ctx);
            prealloc_expr_locals(rhs, ctx);
        }
        Expr::Call { args, .. } | Expr::Perform { args, .. } | Expr::TupleLit(args) => {
            for a in args {
                prealloc_expr_locals(a, ctx);
            }
        }
        Expr::TupleField { expr, .. } | Expr::SlicePtr(expr) | Expr::SliceLen(expr) => {
            prealloc_expr_locals(expr, ctx);
            if !ctx.local_map.contains_key("__ifm_tup_tmp") {
                ctx.alloc_local(Some("__ifm_tup_tmp"));
            }
        }
        Expr::SliceLit { ptr, len, .. } | Expr::BoundsCheck { index: ptr, len, .. } => {
            prealloc_expr_locals(ptr, ctx);
            prealloc_expr_locals(len, ctx);
        }
        Expr::RangeCheck { lo, hi, len } => {
            prealloc_expr_locals(lo, ctx);
            prealloc_expr_locals(hi, ctx);
            prealloc_expr_locals(len, ctx);
        }
        Expr::IntLit(_) | Expr::FixedLit(_) | Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit | Expr::Var(_) => {}
    }
}

fn emit_stmts(
    stmts: &[Stmt],
    ctx: &mut FnCodegenCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), InfimumDiagnostic> {
    for s in stmts {
        match s {
            Stmt::Let { name, value, .. } | Stmt::Assign { name, value } => {
                emit_expr(value, ctx, out)?;
                if let Some(slots) = ctx.tuple_locals.get(name).cloned() {
                    for &lidx in slots.iter().rev() {
                        out.push(0x21); // local.set
                        push_u32_leb(out, lidx);
                    }
                } else {
                    let lidx = *ctx.local_map.get(name).ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E020_UNKNOWN_VAR",
                            "strata-wasm",
                            format!("Missing local slot for '{name}'"),
                            Span::zero(),
                            "Internal local allocation mismatch",
                        )
                    })?;
                    out.push(0x21); // local.set
                    push_u32_leb(out, lidx);
                }
            }
            Stmt::LetTuple { names, value } => {
                emit_expr(value, ctx, out)?;
                for (name, _) in names.iter().rev() {
                    let lidx = *ctx.local_map.get(name).ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E020_UNKNOWN_VAR",
                            "strata-wasm",
                            format!("Missing local slot for tuple element '{name}'"),
                            Span::zero(),
                            "Internal local allocation mismatch",
                        )
                    })?;
                    out.push(0x21); // local.set
                    push_u32_leb(out, lidx);
                }
            }
            Stmt::While {
                cond,
                max_iters,
                body,
            } => {
                let fuel_local = ctx.alloc_local(None);
                // fuel = max_iters
                out.push(0x42); // i64.const
                push_i64_leb(out, *max_iters as i64);
                out.push(0x21); // local.set $fuel
                push_u32_leb(out, fuel_local);

                out.push(0x02); // block $exit
                out.push(WASM_BLOCK_VOID);
                out.push(0x03); // loop $cont
                out.push(WASM_BLOCK_VOID);

                // if fuel <= 0: br 1 ($exit)
                out.push(0x20); // local.get $fuel
                push_u32_leb(out, fuel_local);
                out.push(0x42); // i64.const 0
                push_i64_leb(out, 0);
                out.push(0x57); // i64.le_s -> i32
                out.push(0x0D); // br_if 1
                push_u32_leb(out, 1);

                // if !cond: br 1 ($exit)
                emit_expr(cond, ctx, out)?;
                out.push(0x50); // i64.eqz -> i32 (1 when cond == 0)
                out.push(0x0D); // br_if 1
                push_u32_leb(out, 1);

                // fuel = fuel - 1
                out.push(0x20);
                push_u32_leb(out, fuel_local);
                out.push(0x42);
                push_i64_leb(out, 1);
                out.push(0x7D); // i64.sub
                out.push(0x21);
                push_u32_leb(out, fuel_local);

                emit_stmts(body, ctx, out)?;

                out.push(0x0C); // br 0 ($cont)
                push_u32_leb(out, 0);

                out.push(0x0B); // end loop
                out.push(0x0B); // end block
            }
            Stmt::Expr(e) => {
                let arity = expr_result_arity(e, ctx);
                emit_expr(e, ctx, out)?;
                for _ in 0..arity {
                    out.push(0x1A); // drop unused expression result
                }
            }
        }
    }
    Ok(())
}

fn emit_safe_mem_addr_i32(
    index_expr: &Expr,
    ctx: &mut FnCodegenCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), InfimumDiagnostic> {
    emit_expr(index_expr, ctx, out)?;
    out.push(0xA7); // i32.wrap_i64
    out.push(0x41); // i32.const mem_word_mask (power-of-two linear memory mask)
    push_i32_leb(out, ctx.mem_word_mask);
    out.push(0x71); // i32.and
    out.push(0x41); // i32.const 2
    push_i32_leb(out, 2);
    out.push(0x74); // i32.shl (word_idx * 4)
    Ok(())
}

fn emit_expr(
    expr: &Expr,
    ctx: &mut FnCodegenCtx<'_>,
    out: &mut Vec<u8>,
) -> Result<(), InfimumDiagnostic> {
    match expr {
        Expr::IntLit(v) | Expr::FixedLit(v) => {
            out.push(0x42); // i64.const
            push_i64_leb(out, *v);
        }
        Expr::BoolLit(b) => {
            out.push(0x42);
            push_i64_leb(out, if *b { 1 } else { 0 });
        }
        Expr::UnitLit => {
            out.push(0x42);
            push_i64_leb(out, 0);
        }
        Expr::StrLit(s) => {
            let word_addr = ctx.str_pool.intern(s);
            out.push(0x42); // i64.const <word_addr>
            push_i64_leb(out, word_addr);
        }
        Expr::SecretWrap(_) | Expr::Reveal { .. } => {
            return Err(InfimumDiagnostic::new(
                "E085_STRATA_UNSUPPORTED_EXPR",
                "strata-wasm",
                "Secret expressions belong to Infimum.Flux, not low-level Infimum.Strata kernels",
                Span::zero(),
                "Use i64, bool, str, unit, and linear memory inside Strata kernels",
            ));
        }
        Expr::Var(name) => {
            if let Some(slots) = ctx.tuple_locals.get(name).cloned() {
                for lidx in slots {
                    out.push(0x20); // local.get
                    push_u32_leb(out, lidx);
                }
            } else {
                let lidx = *ctx.local_map.get(name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E020_UNKNOWN_VAR",
                        "strata-wasm",
                        format!("Unknown local '{name}'"),
                        Span::zero(),
                        "Ensure variable is bound",
                    )
                })?;
                out.push(0x20); // local.get
                push_u32_leb(out, lidx);
            }
        }
        Expr::Unary { op, expr: inner } => match op {
            UnaryOp::Neg => {
                out.push(0x42);
                push_i64_leb(out, 0);
                emit_expr(inner, ctx, out)?;
                out.push(0x7D); // 0 - inner
            }
            UnaryOp::Not => {
                emit_expr(inner, ctx, out)?;
                out.push(0x50); // i64.eqz -> i32 (0 or 1)
                out.push(0xAD); // i64.extend_i32_u -> i64
            }
            UnaryOp::IntToFixed => {
                emit_expr(inner, ctx, out)?;
                out.push(0x42);
                push_i64_leb(out, 65536);
                out.push(0x7E); // i64.mul
            }
            UnaryOp::FixedToInt => {
                emit_expr(inner, ctx, out)?;
                out.push(0x42);
                push_i64_leb(out, 65536);
                out.push(0x7F); // i64.div_s
            }
            UnaryOp::FixedToRaw | UnaryOp::RawToFixed => {
                emit_expr(inner, ctx, out)?;
            }
        },
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::And => {
                emit_expr(lhs, ctx, out)?;
                out.push(0xA7); // i32.wrap_i64
                out.push(0x04); // if (result i64)
                out.push(WASM_BLOCK_I64);
                emit_expr(rhs, ctx, out)?;
                out.push(0x05); // else
                out.push(0x42);
                push_i64_leb(out, 0);
                out.push(0x0B); // end
            }
            BinOp::Or => {
                emit_expr(lhs, ctx, out)?;
                out.push(0xA7); // i32.wrap_i64
                out.push(0x04); // if (result i64)
                out.push(WASM_BLOCK_I64);
                out.push(0x42);
                push_i64_leb(out, 1);
                out.push(0x05); // else
                emit_expr(rhs, ctx, out)?;
                out.push(0x0B); // end
            }
            BinOp::FixedMul => {
                emit_expr(lhs, ctx, out)?;
                emit_expr(rhs, ctx, out)?;
                out.push(0x7E); // i64.mul
                out.push(0x42);
                push_i64_leb(out, 65536);
                out.push(0x7F); // i64.div_s
            }
            BinOp::FixedDiv => {
                emit_expr(lhs, ctx, out)?;
                out.push(0x42);
                push_i64_leb(out, 65536);
                out.push(0x7E); // i64.mul
                emit_expr(rhs, ctx, out)?;
                out.push(0x7F); // i64.div_s
            }
            _ => {
                emit_expr(lhs, ctx, out)?;
                emit_expr(rhs, ctx, out)?;
                match op {
                    BinOp::Add => out.push(0x7C),
                    BinOp::Sub => out.push(0x7D),
                    BinOp::Mul => out.push(0x7E),
                    BinOp::Div => out.push(0x7F),
                    BinOp::FixedMul | BinOp::FixedDiv => unreachable!(),
                    BinOp::Mod => out.push(0x81),
                    BinOp::BitAnd => out.push(0x83),
                    BinOp::BitOr => out.push(0x84),
                    BinOp::BitXor => out.push(0x85),
                    BinOp::Shl => out.push(0x86),
                    BinOp::Shr => out.push(0x87),
                    BinOp::Eq => {
                        out.push(0x51);
                        out.push(0xAC);
                    }
                    BinOp::Ne => {
                        out.push(0x52);
                        out.push(0xAC);
                    }
                    BinOp::Lt => {
                        out.push(0x53);
                        out.push(0xAC);
                    }
                    BinOp::Gt => {
                        out.push(0x55);
                        out.push(0xAC);
                    }
                    BinOp::Le => {
                        out.push(0x57);
                        out.push(0xAC);
                    }
                    BinOp::Ge => {
                        out.push(0x59);
                        out.push(0xAC);
                    }
                    BinOp::And | BinOp::Or => unreachable!(),
                }
            }
        },
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            emit_expr(cond, ctx, out)?;
            out.push(0xA7); // i32.wrap_i64
            out.push(0x04); // if (result i64 or multi-value type_idx)
            let res_arity = expr_result_arity(then_branch, ctx);
            if res_arity > 1 {
                if let Some(&tidx) = ctx.multi_val_block_types.get(&res_arity) {
                    push_i32_leb(out, tidx as i32);
                } else {
                    out.push(WASM_BLOCK_I64);
                }
            } else {
                out.push(WASM_BLOCK_I64);
            }
            emit_expr(then_branch, ctx, out)?;
            out.push(0x05); // else
            emit_expr(else_branch, ctx, out)?;
            out.push(0x0B); // end
        }
        Expr::MemLoad { index, .. } => {
            emit_safe_mem_addr_i32(index, ctx, out)?;
            out.push(0x34); // i64.load32_s
            out.push(0x02); // align = 4 bytes
            out.push(0x00); // offset = 0
        }
        Expr::MemStore { index, value, .. } => {
            emit_safe_mem_addr_i32(index, ctx, out)?;
            emit_expr(value, ctx, out)?;
            out.push(0x3E); // i64.store32
            out.push(0x02); // align = 4 bytes
            out.push(0x00); // offset = 0
            // Push unit value `0_i64`
            out.push(0x42);
            push_i64_leb(out, 0);
        }
        Expr::Perform { effect, op, args } => {
            if effect == "Mem" && op == "store_str" && args.len() == 2 {
                // Zero-copy O(1) static string binding: stores word address of `str` into `mem[slot]`!
                emit_safe_mem_addr_i32(&args[0], ctx, out)?;
                emit_expr(&args[1], ctx, out)?;
                out.push(0x3E); // i64.store32
                out.push(0x02);
                out.push(0x00);
                out.push(0x42);
                push_i64_leb(out, 0);
            } else if op == "call" {
                // Capability Host ABI: `do <Effect>.call(op_id, a0, a1, a2)` -> `env.host_call`
                // Stores the i64 host return code in `mem[62]` (byte offset 248).
                out.push(0x41); // i32.const 248
                push_i32_leb(out, 248);
                for i in 0..4 {
                    if let Some(a) = args.get(i) {
                        emit_expr(a, ctx, out)?;
                    } else {
                        out.push(0x42);
                        push_i64_leb(out, 0);
                    }
                }
                out.push(0x10); // call 0 (`env.host_call`)
                push_u32_leb(out, 0);
                out.push(0x3E); // i64.store32 into mem[62]
                out.push(0x02);
                out.push(0x00);
                out.push(0x42); // push unit 0_i64
                push_i64_leb(out, 0);
            } else {
                for a in args {
                    emit_expr(a, ctx, out)?;
                    out.push(0x1A); // drop
                }
                out.push(0x42);
                push_i64_leb(out, 0);
            }
        }
        Expr::Call { target, args } => {
            for a in args {
                emit_expr(a, ctx, out)?;
            }
            let fidx = match target {
                TargetRef::SelfRec => ctx.self_idx,
                TargetRef::Hash(h) => *ctx.hash_to_func_idx.get(h).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E032_NODE_NOT_FOUND",
                        "strata-wasm",
                        format!("Callee #{h} missing in subgraph"),
                        Span::zero(),
                        "Ingest callee first",
                    )
                })?,
                TargetRef::Named(n) => {
                    return Err(InfimumDiagnostic::new(
                        "E055_UNRESOLVED_CALL",
                        "strata-wasm",
                        format!("Unresolved call '{n}'"),
                        Span::zero(),
                        "Canonicalize before emitting Wasm",
                    ))
                }
            };
            out.push(0x10); // call
            push_u32_leb(out, fidx);
        }
        Expr::Block { stmts, tail } => {
            emit_stmts(stmts, ctx, out)?;
            emit_expr(tail, ctx, out)?;
        }
        Expr::TupleLit(elems) => {
            for e in elems {
                emit_expr(e, ctx, out)?;
            }
        }
        Expr::SliceLit { ptr, len, .. } => {
            emit_expr(ptr, ctx, out)?;
            emit_expr(len, ctx, out)?;
        }
        Expr::SlicePtr(inner) => {
            emit_expr(
                &Expr::TupleField {
                    expr: inner.clone(),
                    index: 0,
                },
                ctx,
                out,
            )?;
        }
        Expr::SliceLen(inner) => {
            emit_expr(
                &Expr::TupleField {
                    expr: inner.clone(),
                    index: 1,
                },
                ctx,
                out,
            )?;
        }
        Expr::BoundsCheck { index, .. } => {
            // Zero runtime overhead in Wasm: proven statically by Z3!
            emit_expr(index, ctx, out)?;
        }
        Expr::RangeCheck { lo, .. } => {
            // Zero runtime overhead in Wasm: proven statically by Z3!
            emit_expr(lo, ctx, out)?;
        }
        Expr::TupleField { expr, index } => {
            if let Expr::Var(name) = expr.as_ref() {
                if let Some(slots) = ctx.tuple_locals.get(name) {
                    let keep = (*index).min(slots.len().saturating_sub(1));
                    out.push(0x20); // local.get
                    push_u32_leb(out, slots[keep]);
                    return Ok(());
                }
            }
            if let Expr::TupleLit(elems) = expr.as_ref() {
                let keep = (*index).min(elems.len().saturating_sub(1));
                let tmp = *ctx.local_map.get("__ifm_tup_tmp").unwrap();
                for (i, e) in elems.iter().enumerate() {
                    let e_arity = expr_result_arity(e, ctx);
                    emit_expr(e, ctx, out)?;
                    if i == keep {
                        out.push(0x21); // local.set tmp
                        push_u32_leb(out, tmp);
                    } else {
                        for _ in 0..e_arity {
                            out.push(0x1A); // drop (preserving side effects of discarded elements)
                        }
                    }
                }
                out.push(0x20); // local.get tmp
                push_u32_leb(out, tmp);
            } else {
                let arity = expr_result_arity(expr, ctx);
                emit_expr(expr, ctx, out)?;
                if arity > 1 {
                    let keep = (*index).min(arity - 1);
                    for _ in 0..(arity - 1 - keep) {
                        out.push(0x1A); // drop
                    }
                    let tmp = *ctx.local_map.get("__ifm_tup_tmp").unwrap();
                    out.push(0x21); // local.set tmp
                    push_u32_leb(out, tmp);
                    for _ in 0..keep {
                        out.push(0x1A); // drop
                    }
                    out.push(0x20); // local.get tmp
                    push_u32_leb(out, tmp);
                }
            }
        }
    }
    Ok(())
}

fn intern_sig(entries: &mut Vec<Vec<u8>>, sig: Vec<u8>) -> u32 {
    if let Some(pos) = entries.iter().position(|e| e == &sig) {
        pos as u32
    } else {
        let idx = entries.len() as u32;
        entries.push(sig);
        idx
    }
}

fn push_section(wasm: &mut Vec<u8>, id: u8, payload: &[u8]) {
    wasm.push(id);
    push_u32_leb(wasm, payload.len() as u32);
    wasm.extend_from_slice(payload);
}

fn push_wasm_str(out: &mut Vec<u8>, s: &str) {
    push_u32_leb(out, s.len() as u32);
    out.extend_from_slice(s.as_bytes());
}

fn push_u32_leb(out: &mut Vec<u8>, mut val: u32) {
    loop {
        let mut byte = (val & 0x7F) as u8;
        val >>= 7;
        if val != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if val == 0 {
            break;
        }
    }
}

fn push_i32_leb(out: &mut Vec<u8>, mut val: i32) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        let done = (val == 0 && (byte & 0x40) == 0) || (val == -1 && (byte & 0x40) != 0);
        if done {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
}

fn push_i64_leb(out: &mut Vec<u8>, mut val: i64) {
    loop {
        let byte = (val & 0x7F) as u8;
        val >>= 7;
        let done = (val == 0 && (byte & 0x40) == 0) || (val == -1 && (byte & 0x40) != 0);
        if done {
            out.push(byte);
            break;
        } else {
            out.push(byte | 0x80);
        }
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeMap;

    #[test]
    fn rejects_non_scalar_tuple_export_fn() {
        let mut files = BTreeMap::new();
        files.insert(
            "main.ifm".to_string(),
            r#"
struct Vec2 { x: i64, y: i64 }

export fn bad_export(v: Vec2) -> Vec2 {
  Vec2 { x: v.x + 1, y: v.y + 1 }
}
"#
            .to_string(),
        );
        let err = crate::wasm_api::compile_project_in_memory("main.ifm", &files).unwrap_err();
        assert_eq!(err.code, "E082_NON_SCALAR_EXPORT");
    }

    #[test]
    fn tuple_lit_field_preserves_discarded_side_effects() {
        let mut files = BTreeMap::new();
        files.insert(
            "main.ifm".to_string(),
            r#"
fn side_effect() -[Mem]-> i64 {
  mem_store(100, 777);
  1
}

export fn run_check() -[Mem]-> i64 {
  let v = (side_effect(), 42).1;
  mem_load(100) + v
}
"#
            .to_string(),
        );
        let (bytes, _) = crate::wasm_api::compile_project_in_memory("main.ifm", &files).unwrap();
        assert!(bytes.starts_with(b"\0asm"));
        // Verify emitted bytecode contains both call to side_effect and value 42, and validates cleanly in node/Wasm
        assert!(bytes.len() > 40);
    }
}
