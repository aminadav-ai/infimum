// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{
    BinOp, ConstDef, ConstValue, EnumDef, EnumVariant, Expr, FnDef, LayoutDef, LayoutField, Param,
    Stmt, TargetRef, Type, UnaryOp,
};
use crate::diag::{InfimumDiagnostic, Span};

use super::raw_ast::{
    LayoutFieldMeta, RawExpr, RawFnDef, RawItem, RawLayoutLoad, RawMatchArm, RawMatchPattern,
    RawModCall, RawSliceIndex, RawSliceRange, RawSliceStore, RawStmt, RawStructLit, RawType,
    RawUnit, StructDef, UnitId,
};
use crate::lexer::tokenize;
use super::scope::{SymRef, UnitTables, unit_canonical_name};
use super::syntax::Parser;

#[derive(Clone)]
pub(super) struct SliceElemMeta {
    pub(super) elem_ty: Type,
    pub(super) struct_canon: Option<String>,
    pub(super) stride: i64,
    pub(super) is_fixed: bool,
}

pub(super) struct ModuleResolver {
    pub(super) tables: Vec<UnitTables>,
    pub(super) unit_labels: Vec<String>,
    pub(super) cur_uid: UnitId,
    pub(super) has_std_gfx: bool,
    pub(super) std_text_uid: Option<UnitId>,
    pub(super) consts: BTreeMap<String, ConstDef>,
    pub(super) enums: BTreeMap<String, EnumDef>,
    pub(super) structs: BTreeMap<String, StructDef>,
    pub(super) layouts: BTreeMap<String, LayoutDef>,
    pub(super) layout_field_meta: BTreeMap<(String, String), LayoutFieldMeta>,
    pub(super) layout_spans: Vec<(i64, i64, String, Span)>,
    pub(super) auto_layout_cursor: i64,
    pub(super) match_counter: usize,
    pub(super) addr_counter: usize,
    pub(super) loop_counter: usize,
    pub(super) loop_stack: Vec<LoopCtrlCtx>,
    pub(super) struct_vars: Vec<BTreeMap<String, String>>,
    pub(super) fn_ret_struct: BTreeMap<String, String>,
    pub(super) slice_vars: Vec<BTreeMap<String, SliceElemMeta>>,
    pub(super) fn_ret_slice: BTreeMap<String, SliceElemMeta>,
    pub(super) forma_subst: BTreeMap<String, Expr>,
}

#[derive(Clone)]
pub(super) struct LoopCtrlCtx {
    pub(super) brk_var: Option<String>,
    pub(super) cont_var: Option<String>,
}

pub(super) fn expr_has_loop_ctrl(e: &RawExpr) -> (bool, bool) {
    match e {
        RawExpr::If {
            then_branch,
            else_branch,
            ..
        } => {
            let (b1, c1) = expr_has_loop_ctrl(then_branch);
            let (b2, c2) = expr_has_loop_ctrl(else_branch);
            (b1 || b2, c1 || c2)
        }
        RawExpr::Block { stmts, tail } => {
            let (b1, c1) = stmts_have_loop_ctrl(stmts);
            let (b2, c2) = expr_has_loop_ctrl(tail);
            (b1 || b2, c1 || c2)
        }
        RawExpr::Match { arms, .. } => {
            let mut b = false;
            let mut c = false;
            for arm in arms {
                let (ab, ac) = expr_has_loop_ctrl(&arm.body);
                b |= ab;
                c |= ac;
            }
            (b, c)
        }
        _ => (false, false),
    }
}

pub(super) fn stmt_has_loop_ctrl(s: &RawStmt) -> (bool, bool) {
    match s {
        RawStmt::Break(_) => (true, false),
        RawStmt::Continue(_) => (false, true),
        RawStmt::Expr(e) => expr_has_loop_ctrl(e),
        // Nested loops handle their own break/continue
        RawStmt::While { .. }
        | RawStmt::For { .. }
        | RawStmt::Let { .. }
        | RawStmt::LetTuple { .. }
        | RawStmt::Assign { .. }
        | RawStmt::LayoutStore { .. }
        | RawStmt::SliceStore(_) => (false, false),
    }
}

pub(super) fn stmts_have_loop_ctrl(stmts: &[RawStmt]) -> (bool, bool) {
    let mut b = false;
    let mut c = false;
    for s in stmts {
        let (sb, sc) = stmt_has_loop_ctrl(s);
        b |= sb;
        c |= sc;
    }
    (b, c)
}

impl ModuleResolver {
    pub(super) fn new(
        tables: Vec<UnitTables>,
        unit_labels: Vec<String>,
        entry_uid: UnitId,
        has_std_gfx: bool,
        std_text_uid: Option<UnitId>,
    ) -> Self {
        Self {
            tables,
            unit_labels,
            cur_uid: entry_uid,
            has_std_gfx,
            std_text_uid,
            consts: BTreeMap::new(),
            enums: BTreeMap::new(),
            structs: BTreeMap::new(),
            layouts: BTreeMap::new(),
            layout_field_meta: BTreeMap::new(),
            layout_spans: Vec::new(),
            auto_layout_cursor: 705_536,
            match_counter: 0,
            addr_counter: 0,
            loop_counter: 0,
            loop_stack: Vec::new(),
            struct_vars: Vec::new(),
            fn_ret_struct: BTreeMap::new(),
            slice_vars: Vec::new(),
            fn_ret_slice: BTreeMap::new(),
            forma_subst: BTreeMap::new(),
        }
    }

    fn lookup_namespace(&self, alias: &str) -> Option<UnitId> {
        self.tables[self.cur_uid]
            .scope
            .namespaces
            .get(alias)
            .map(|n| n.target_uid)
    }

    fn private_err(&self, target_uid: UnitId, name: &str, span: Span) -> InfimumDiagnostic {
        let lbl = &self.unit_labels[target_uid];
        InfimumDiagnostic::new(
            "E068_PRIVATE_ITEM",
            "resolve",
            format!("Symbol '{name}' in '{lbl}' is private; mark it 'pub' to export"),
            span,
            format!("Add 'pub' to '{name}' in '{lbl}'"),
        )
    }

    fn lookup_const_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.consts.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.consts.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.consts.get(name))
        }
    }

    fn lookup_enum_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.enums.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.enums.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.enums.get(name))
        }
    }

    fn lookup_struct_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.structs.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.structs.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.structs.get(name))
        }
    }

    fn lookup_layout_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.layouts.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.layouts.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.layouts.get(name))
        }
    }

    pub(super) fn lookup_fn_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.fns.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.fns.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.fns.get(name))
        }
    }

    pub(super) fn lookup_forma_sym(&self, module: Option<&str>, name: &str, span: Span) -> Result<Option<&SymRef>, InfimumDiagnostic> {
        if let Some(m) = module {
            let Some(tid) = self.lookup_namespace(m) else {
                return Ok(None);
            };
            if let Some(sym) = self.tables[tid].exports.formas.get(name) {
                return Ok(Some(sym));
            }
            if self.tables[tid].scope.formas.contains_key(name) {
                return Err(self.private_err(tid, name, span));
            }
            Ok(None)
        } else {
            Ok(self.tables[self.cur_uid].scope.formas.get(name))
        }
    }

    fn resolve_layout_canon(
        &self,
        module: Option<&str>,
        layout: &str,
        span: Span,
    ) -> Result<String, InfimumDiagnostic> {
        if let Some(sym) = self.lookup_layout_sym(module, layout, span)? {
            return Ok(sym.canonical_name.clone());
        }
        let disp = match module {
            Some(m) => format!("{m}::{layout}"),
            None => layout.to_string(),
        };
        Err(InfimumDiagnostic::new(
            "E061_UNKNOWN_LAYOUT_FIELD",
            "resolve",
            format!("Unknown layout '{disp}'"),
            span,
            format!("Declare 'layout {layout}' (or import it with 'use') before accessing it"),
        ))
    }

    pub(super) fn resolve_struct_canon(
        &self,
        module: Option<&str>,
        name: &str,
        span: Span,
    ) -> Result<String, InfimumDiagnostic> {
        if let Some(sym) = self.lookup_struct_sym(module, name, span)? {
            return Ok(sym.canonical_name.clone());
        }
        let disp = match module {
            Some(m) => format!("{m}::{name}"),
            None => name.to_string(),
        };
        Err(InfimumDiagnostic::new(
            "E014_UNKNOWN_TYPE",
            "resolve",
            format!("Unknown struct type '{disp}'"),
            span,
            format!("Declare 'pub struct {name} {{ ... }}' or import it with 'use'"),
        ))
    }

    fn struct_tuple_type(&self, sdef: &StructDef) -> Type {
        let elems = sdef
            .field_names
            .iter()
            .map(|fname| {
                if sdef.field_fixed.contains(fname) {
                    Type::Fixed
                } else {
                    Type::I64
                }
            })
            .collect();
        Type::Tuple(elems)
    }

    pub(super) fn resolve_slice_elem(&self, elem: &RawType) -> Result<SliceElemMeta, InfimumDiagnostic> {
        match elem {
            RawType::Builtin(Type::I64) => Ok(SliceElemMeta {
                elem_ty: Type::I64,
                struct_canon: None,
                stride: 1,
                is_fixed: false,
            }),
            RawType::Builtin(Type::Fixed) => Ok(SliceElemMeta {
                elem_ty: Type::Fixed,
                struct_canon: None,
                stride: 1,
                is_fixed: true,
            }),
            RawType::Named { module, name, span } => {
                let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
                let sdef = self.structs.get(&scanon).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_STRUCT",
                        "parse",
                        format!("Unknown struct '{name}' in slice type"),
                        *span,
                        "Declare the struct before using it in '[StructName]'",
                    )
                })?;
                Ok(SliceElemMeta {
                    elem_ty: self.struct_tuple_type(sdef),
                    struct_canon: Some(scanon),
                    stride: sdef.size,
                    is_fixed: false,
                })
            }
            RawType::Secret(_) => Err(InfimumDiagnostic::new(
                "E062_SECRET_IN_SLICE",
                "parse",
                "Secret<T> cannot be stored inside a linear-memory slice '[T]'",
                Span::zero(),
                "Slices support 'i64', 'word', 'fixed', and value 'struct' types",
            )),
            _ => Err(InfimumDiagnostic::new(
                "E014_UNKNOWN_TYPE",
                "parse",
                "Unsupported slice element type",
                Span::zero(),
                "Slice elements must be 'i64', 'word', 'fixed', or a value 'struct' (e.g. '[i64]', '[fixed]', '[Drone]')",
            )),
        }
    }

    fn resolve_raw_type(&self, rt: &RawType) -> Result<Type, InfimumDiagnostic> {
        match rt {
            RawType::Builtin(t) => Ok(t.clone()),
            RawType::Secret(inner) => Ok(Type::Secret(Box::new(self.resolve_raw_type(inner)?))),
            RawType::Slice(inner) => {
                let meta = self.resolve_slice_elem(inner)?;
                Ok(Type::Slice(Box::new(meta.elem_ty)))
            }
            RawType::Tuple(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.resolve_raw_type(e)?);
                }
                Ok(Type::Tuple(out))
            }
            RawType::Named { module, name, span } => {
                if let Some(sym) = self.lookup_struct_sym(module.as_deref(), name, *span)? {
                    if let Some(sdef) = self.structs.get(&sym.canonical_name) {
                        return Ok(self.struct_tuple_type(sdef));
                    }
                }
                let disp = match module {
                    Some(m) => format!("{m}::{name}"),
                    None => name.clone(),
                };
                Err(InfimumDiagnostic::new(
                    "E014_UNKNOWN_TYPE",
                    "parse",
                    format!("Unknown type '{disp}'"),
                    *span,
                    "Use a built-in type (i64, fixed, bool, str, unit, Secret<T>, (T1, T2), [T]) or a visible 'struct' name",
                ))
            }
        }
    }

    fn lookup_struct_var(&self, name: &str) -> Option<String> {
        for scope in self.struct_vars.iter().rev() {
            if let Some(sname) = scope.get(name) {
                return Some(sname.clone());
            }
        }
        None
    }

    fn lookup_slice_var(&self, name: &str) -> Option<SliceElemMeta> {
        for scope in self.slice_vars.iter().rev() {
            if let Some(meta) = scope.get(name) {
                return Some(meta.clone());
            }
        }
        None
    }

    fn slice_meta_from_layout_field(&self, meta: &LayoutFieldMeta) -> Option<SliceElemMeta> {
        if let Some(ref sc) = meta.struct_type {
            let sdef = self.structs.get(sc)?;
            Some(SliceElemMeta {
                elem_ty: self.struct_tuple_type(sdef),
                struct_canon: Some(sc.clone()),
                stride: sdef.size,
                is_fixed: false,
            })
        } else if meta.is_fixed {
            Some(SliceElemMeta {
                elem_ty: Type::Fixed,
                struct_canon: None,
                stride: 1,
                is_fixed: true,
            })
        } else {
            Some(SliceElemMeta {
                elem_ty: Type::I64,
                struct_canon: None,
                stride: 1,
                is_fixed: false,
            })
        }
    }

    fn infer_raw_expr_slice(&self, expr: &RawExpr) -> Option<SliceElemMeta> {
        match expr {
            RawExpr::Var(v) => self.lookup_slice_var(v),
            RawExpr::SliceRange(sr) => {
                if let RawExpr::LayoutLoad(ll) = &sr.base {
                    if ll.index.is_none() && ll.subfield.is_none() {
                        if let Some(sym) = self
                            .lookup_layout_sym(ll.module.as_deref(), &ll.layout, ll.span)
                            .ok()
                            .flatten()
                        {
                            if let Some(meta) = self
                                .layout_field_meta
                                .get(&(sym.canonical_name.clone(), ll.field.clone()))
                            {
                                if meta.is_array || meta.is_vec {
                                    return self.slice_meta_from_layout_field(meta);
                                }
                            }
                        }
                    }
                }
                self.infer_raw_expr_slice(&sr.base)
            }
            RawExpr::Call {
                target: TargetRef::Named(fname),
                ..
            } => match fname.as_str() {
                "slice_i64" | "str_as_slice" => Some(SliceElemMeta {
                    elem_ty: Type::I64,
                    struct_canon: None,
                    stride: 1,
                    is_fixed: false,
                }),
                "slice_fixed" => Some(SliceElemMeta {
                    elem_ty: Type::Fixed,
                    struct_canon: None,
                    stride: 1,
                    is_fixed: true,
                }),
                _ => {
                    let canon = self
                        .lookup_fn_sym(None, fname, Span::zero())
                        .ok()
                        .flatten()
                        .map(|s| s.canonical_name.as_str())
                        .unwrap_or(fname.as_str());
                    self.fn_ret_slice.get(canon).cloned()
                }
            },
            RawExpr::ModCall(mc) => {
                let sym = self
                    .lookup_fn_sym(Some(&mc.module), &mc.func, mc.span)
                    .ok()
                    .flatten()?;
                self.fn_ret_slice.get(&sym.canonical_name).cloned()
            }
            RawExpr::If {
                then_branch,
                else_branch,
                ..
            } => self
                .infer_raw_expr_slice(then_branch)
                .or_else(|| self.infer_raw_expr_slice(else_branch)),
            RawExpr::Block { tail, .. } => self.infer_raw_expr_slice(tail),
            _ => None,
        }
    }

    fn emit_load_struct_at(&mut self, addr_expr: Expr, sdef: &StructDef) -> Expr {
        let base_var = format!("__ifm_sa_{}", self.addr_counter);
        self.addr_counter += 1;
        let mut elems = Vec::with_capacity(sdef.field_names.len());
        for fname in &sdef.field_names {
            let foff = sdef.fields[fname];
            let finv = sdef.field_invs.get(fname).copied();
            let ffixed = sdef.field_fixed.contains(fname);
            let faddr = if foff == 0 {
                Expr::Var(base_var.clone())
            } else {
                Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(base_var.clone())),
                    rhs: Box::new(Expr::IntLit(foff)),
                }
            };
            elems.push(Expr::MemLoad {
                index: Box::new(faddr),
                inv: finv,
                fixed_ty: ffixed,
            });
        }
        Expr::Block {
            stmts: vec![Stmt::Let {
                name: base_var,
                mutable: false,
                ty: Some(Type::I64),
                value: addr_expr,
            }],
            tail: Box::new(Expr::TupleLit(elems)),
        }
    }

    fn emit_store_struct_at(
        &mut self,
        addr_expr: Expr,
        val_expr: Expr,
        sdef: &StructDef,
    ) -> Vec<Stmt> {
        let sv_tmp = format!("__ifm_sv_{}", self.addr_counter);
        self.addr_counter += 1;
        let base_var = format!("__ifm_sa_{}", self.addr_counter);
        self.addr_counter += 1;
        let mut stmts = Vec::with_capacity(2 + sdef.field_names.len());
        stmts.push(Stmt::Let {
            name: sv_tmp.clone(),
            mutable: false,
            ty: Some(self.struct_tuple_type(sdef)),
            value: val_expr,
        });
        stmts.push(Stmt::Let {
            name: base_var.clone(),
            mutable: false,
            ty: Some(Type::I64),
            value: addr_expr,
        });
        for (fidx, fname) in sdef.field_names.iter().enumerate() {
            let foff = sdef.fields[fname];
            let finv = sdef.field_invs.get(fname).copied();
            let ffixed = sdef.field_fixed.contains(fname);
            let faddr = if foff == 0 {
                Expr::Var(base_var.clone())
            } else {
                Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(base_var.clone())),
                    rhs: Box::new(Expr::IntLit(foff)),
                }
            };
            stmts.push(Stmt::Expr(Expr::MemStore {
                index: Box::new(faddr),
                value: Box::new(Expr::TupleField {
                    expr: Box::new(Expr::Var(sv_tmp.clone())),
                    index: fidx,
                }),
                inv: finv,
                fixed_ty: ffixed,
            }));
        }
        stmts
    }

    fn infer_raw_expr_struct(&self, expr: &RawExpr) -> Option<String> {
        match expr {
            RawExpr::StructLit(sl) => self
                .lookup_struct_sym(sl.module.as_deref(), &sl.name, sl.span)
                .ok()
                .flatten()
                .map(|s| s.canonical_name.clone()),
            RawExpr::Var(v) => self.lookup_struct_var(v),
            RawExpr::SliceIndex(si) if si.subfield.is_none() => self
                .infer_raw_expr_slice(&si.base)
                .and_then(|m| m.struct_canon),
            RawExpr::Call {
                target: TargetRef::Named(fname),
                ..
            } => {
                let canon = self
                    .lookup_fn_sym(None, fname, Span::zero())
                    .ok()
                    .flatten()
                    .map(|s| s.canonical_name.as_str())
                    .unwrap_or(fname.as_str());
                self.fn_ret_struct.get(canon).cloned()
            }
            RawExpr::ModCall(mc) => {
                let sym = self
                    .lookup_fn_sym(Some(&mc.module), &mc.func, mc.span)
                    .ok()
                    .flatten()?;
                self.fn_ret_struct.get(&sym.canonical_name).cloned()
            }
            RawExpr::LayoutLoad(ll) if ll.subfield.is_none() => {
                let lcanon = self
                    .lookup_layout_sym(ll.module.as_deref(), &ll.layout, ll.span)
                    .ok()
                    .flatten()?
                    .canonical_name
                    .clone();
                let meta = self.layout_field_meta.get(&(lcanon, ll.field.clone()))?;
                meta.struct_type.clone()
            }
            RawExpr::If {
                then_branch,
                else_branch,
                ..
            } => self
                .infer_raw_expr_struct(then_branch)
                .or_else(|| self.infer_raw_expr_struct(else_branch)),
            RawExpr::Block { tail, .. } => self.infer_raw_expr_struct(tail),
            _ => None,
        }
    }

    fn eval_const(&self, expr: &RawExpr, span: Span) -> Result<ConstValue, InfimumDiagnostic> {
        match expr {
            RawExpr::IntLit(v) => Ok(ConstValue::Int(*v)),
            RawExpr::FixedLit(v) => Ok(ConstValue::Fixed(*v)),
            RawExpr::BoolLit(b) => Ok(ConstValue::Bool(*b)),
            RawExpr::Var(name) => {
                if let Some(sub) = self.forma_subst.get(name) {
                    match sub {
                        Expr::IntLit(v) => return Ok(ConstValue::Int(*v)),
                        Expr::FixedLit(v) => return Ok(ConstValue::Fixed(*v)),
                        Expr::BoolLit(b) => return Ok(ConstValue::Bool(*b)),
                        _ => {}
                    }
                }
                if let Some(sym) = self.lookup_const_sym(None, name, span)? {
                    if let Some(c) = self.consts.get(&sym.canonical_name) {
                        return Ok(c.value);
                    }
                }
                Err(InfimumDiagnostic::new(
                    "E064_CONST_EVAL",
                    "const-eval",
                    format!("Unknown constant '{name}' in constant expression"),
                    span,
                    "Declare the constant with 'const NAME: i64 = ...;' (or import a 'pub const' with 'use') before referencing it",
                ))
            }
            RawExpr::Path { segs, span: pspan } => self.resolve_path_const(segs, *pspan),
            RawExpr::Unary { op, expr: inner } => {
                let v = self.eval_const(inner, span)?;
                match (op, v) {
                    (UnaryOp::Neg, ConstValue::Int(n)) => n
                        .checked_neg()
                        .map(ConstValue::Int)
                        .ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Integer overflow negating {n} in constant expression"),
                                span,
                                "Keep constant expressions within signed 64-bit integer bounds",
                            )
                        }),
                    (UnaryOp::Neg, ConstValue::Fixed(n)) => n
                        .checked_neg()
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("-", span)),
                    (UnaryOp::Not, ConstValue::Bool(b)) => Ok(ConstValue::Bool(!b)),
                    _ => Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!("Invalid unary operator '{}' in constant expression", op.as_str()),
                        span,
                        "Use '-' on i64 constants and '!' on bool constants",
                    )),
                }
            }
            RawExpr::Binary { op, lhs, rhs } => {
                let lv = self.eval_const(lhs, span)?;
                let rv = self.eval_const(rhs, span)?;
                match (op, lv, rv) {
                    (BinOp::Add, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_add(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("+", span)),
                    (BinOp::Sub, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_sub(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("-", span)),
                    (BinOp::Add, ConstValue::Fixed(a), ConstValue::Fixed(b)) => a
                        .checked_add(b)
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("+", span)),
                    (BinOp::Sub, ConstValue::Fixed(a), ConstValue::Fixed(b)) => a
                        .checked_sub(b)
                        .map(ConstValue::Fixed)
                        .ok_or_else(|| const_overflow("-", span)),
                    (BinOp::Mul, ConstValue::Fixed(a), ConstValue::Fixed(b)) => {
                        let prod = (a as i128 * b as i128) / 65536;
                        i64::try_from(prod)
                            .map(ConstValue::Fixed)
                            .map_err(|_| const_overflow("*", span))
                    }
                    (BinOp::Div, ConstValue::Fixed(a), ConstValue::Fixed(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Division by zero in fixed-point constant expression",
                                span,
                                "Ensure divisor is non-zero",
                            ));
                        }
                        let q = (a as i128 * 65536) / (b as i128);
                        i64::try_from(q)
                            .map(ConstValue::Fixed)
                            .map_err(|_| const_overflow("/", span))
                    }
                    (BinOp::Mul, ConstValue::Int(a), ConstValue::Int(b)) => a
                        .checked_mul(b)
                        .map(ConstValue::Int)
                        .ok_or_else(|| const_overflow("*", span)),
                    (BinOp::Div, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Division by zero in constant expression",
                                span,
                                "Ensure the divisor in a constant expression is non-zero",
                            ));
                        }
                        a.checked_div(b)
                            .map(ConstValue::Int)
                            .ok_or_else(|| const_overflow("/", span))
                    }
                    (BinOp::Mod, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if b == 0 {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                "Modulo by zero in constant expression",
                                span,
                                "Ensure the divisor in a constant expression is non-zero",
                            ));
                        }
                        a.checked_rem(b)
                            .map(ConstValue::Int)
                            .ok_or_else(|| const_overflow("%", span))
                    }
                    (BinOp::BitAnd, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a & b))
                    }
                    (BinOp::BitOr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a | b))
                    }
                    (BinOp::BitXor, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Int(a ^ b))
                    }
                    (BinOp::Shl, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if !(0..64).contains(&b) {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Invalid shift amount {b} in constant expression"),
                                span,
                                "Use a shift count in the range 0..63",
                            ));
                        }
                        Ok(ConstValue::Int(a.wrapping_shl(b as u32)))
                    }
                    (BinOp::Shr, ConstValue::Int(a), ConstValue::Int(b)) => {
                        if !(0..64).contains(&b) {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "const-eval",
                                format!("Invalid shift amount {b} in constant expression"),
                                span,
                                "Use a shift count in the range 0..63",
                            ));
                        }
                        Ok(ConstValue::Int(a.wrapping_shr(b as u32)))
                    }
                    (BinOp::Eq, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a == b))
                    }
                    (BinOp::Ne, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a != b))
                    }
                    (BinOp::Lt, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a < b))
                    }
                    (BinOp::Le, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a <= b))
                    }
                    (BinOp::Gt, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a > b))
                    }
                    (BinOp::Ge, ConstValue::Int(a), ConstValue::Int(b)) => {
                        Ok(ConstValue::Bool(a >= b))
                    }
                    (BinOp::And, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a && b))
                    }
                    (BinOp::Or, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a || b))
                    }
                    (BinOp::Eq, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a == b))
                    }
                    (BinOp::Ne, ConstValue::Bool(a), ConstValue::Bool(b)) => {
                        Ok(ConstValue::Bool(a != b))
                    }
                    _ => Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!(
                            "Type mismatch for operator '{}' in constant expression",
                            op.as_str()
                        ),
                        span,
                        "Ensure operand types match the operator in constant expressions",
                    )),
                }
            }
            _ => Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "const-eval",
                "Non-constant expression used where a compile-time constant is required",
                span,
                "Use integer/bool literals, arithmetic/bitwise operators, const names, Enum::Variant, or Layout::field",
            )),
        }
    }

    fn eval_const_i64(&self, expr: &RawExpr, span: Span, context: &str) -> Result<i64, InfimumDiagnostic> {
        match self.eval_const(expr, span)? {
            ConstValue::Int(v) | ConstValue::Fixed(v) => Ok(v),
            ConstValue::Bool(_) => Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "const-eval",
                format!("Expected numeric constant for {context}, found bool"),
                span,
                "Provide an integer or fixed-point constant expression",
            )),
        }
    }

    fn resolve_layout_prop_by_canon(
        &self,
        lcanon: &str,
        disp_layout: &str,
        member: &str,
        prop: &str,
        span: Span,
    ) -> Result<i64, InfimumDiagnostic> {
        let ldef = self.layouts.get(lcanon).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout '{disp_layout}' in '{disp_layout}::{member}::{prop}'"),
                span,
                format!("Declare 'layout {disp_layout}' before referencing its field properties"),
            )
        })?;
        let field = ldef
            .fields
            .iter()
            .find(|f| f.name == member)
            .ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Layout '{disp_layout}' has no field '{member}'"),
                    span,
                    format!("Check the fields declared in 'layout {disp_layout}'"),
                )
            })?;
        let meta = self
            .layout_field_meta
            .get(&(lcanon.to_string(), member.to_string()));
        match prop {
            "len" => Ok(field.len),
            "end" => Ok(field.offset + field.len),
            "stride" => Ok(meta.map(|m| m.stride).unwrap_or(1)),
            "count" => Ok(meta.map(|m| m.count).unwrap_or(field.len)),
            other => Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout field property '{other}' on '{disp_layout}::{member}'"),
                span,
                "Supported layout field properties are '::len', '::end', '::stride', and '::count'",
            )),
        }
    }

    fn resolve_member_in_scope(
        &self,
        module: Option<&str>,
        head: &str,
        member: &str,
        span: Span,
    ) -> Result<Option<ConstValue>, InfimumDiagnostic> {
        let disp = match module {
            Some(m) => format!("{m}::{head}"),
            None => head.to_string(),
        };
        if let Some(sym) = self.lookup_enum_sym(module, head, span)? {
            if let Some(edef) = self.enums.get(&sym.canonical_name) {
                if let Some(v) = edef.variants.iter().find(|v| v.name == member) {
                    return Ok(Some(ConstValue::Int(v.value)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_ENUM_VARIANT",
                    "resolve",
                    format!("Enum '{disp}' has no variant '{member}'"),
                    span,
                    format!("Check the variants declared in 'enum {head}'"),
                ));
            }
        }
        if let Some(sym) = self.lookup_layout_sym(module, head, span)? {
            if let Some(ldef) = self.layouts.get(&sym.canonical_name) {
                if let Some(f) = ldef.fields.iter().find(|f| f.name == member) {
                    return Ok(Some(ConstValue::Int(f.offset)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Layout '{disp}' has no field '{member}'"),
                    span,
                    format!("Check the fields declared in 'layout {head}'"),
                ));
            }
        }
        if let Some(sym) = self.lookup_struct_sym(module, head, span)? {
            if let Some(sdef) = self.structs.get(&sym.canonical_name) {
                if member == "SIZE" || member == "size" {
                    return Ok(Some(ConstValue::Int(sdef.size)));
                }
                if let Some(&off) = sdef.fields.get(member) {
                    return Ok(Some(ConstValue::Int(off)));
                }
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Struct '{disp}' has no field '{member}'"),
                    span,
                    format!(
                        "Available fields in 'struct {head}': {} (plus '::SIZE')",
                        sdef.field_names.join(", ")
                    ),
                ));
            }
        }
        Ok(None)
    }

    fn resolve_path_const(&self, segs: &[String], span: Span) -> Result<ConstValue, InfimumDiagnostic> {
        let full = segs.join("::");
        match segs {
            [head, member] => {
                // 1. Module constant: `m::CONST`
                if let Some(sym) = self.lookup_const_sym(Some(head), member, span)? {
                    if let Some(c) = self.consts.get(&sym.canonical_name) {
                        return Ok(c.value);
                    }
                }
                // 2. Local/imported Enum::Variant, Layout::field, Struct::SIZE/field
                if let Some(val) = self.resolve_member_in_scope(None, head, member, span)? {
                    return Ok(val);
                }
                Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_ENUM_VARIANT",
                    "resolve",
                    format!("Unknown module, enum, struct, or layout '{head}' in '{full}'"),
                    span,
                    format!("Check that '{head}' is declared or imported with 'use' and '{member}' is marked 'pub'"),
                ))
            }
            [a, b, c] => {
                // 1. Qualified module member: `m::Enum::Variant`, `m::Layout::field`, `m::Struct::SIZE`
                if self.lookup_namespace(a).is_some() {
                    if let Some(val) = self.resolve_member_in_scope(Some(a), b, c, span)? {
                        return Ok(val);
                    }
                    return Err(InfimumDiagnostic::new(
                        "E061_UNKNOWN_ENUM_VARIANT",
                        "resolve",
                        format!("Unknown exported enum, struct, or layout '{b}' in module '{a}' for '{full}'"),
                        span,
                        format!("Ensure '{b}' is marked 'pub' in module '{a}'"),
                    ));
                }
                // 2. Local/imported layout field property: `Layout::field::len/end/stride/count`
                let lcanon = self.resolve_layout_canon(None, a, span)?;
                Ok(ConstValue::Int(
                    self.resolve_layout_prop_by_canon(&lcanon, a, b, c, span)?,
                ))
            }
            [m, layout, field, prop] => {
                let lcanon = self.resolve_layout_canon(Some(m), layout, span)?;
                let disp = format!("{m}::{layout}");
                Ok(ConstValue::Int(
                    self.resolve_layout_prop_by_canon(&lcanon, &disp, field, prop, span)?,
                ))
            }
            _ => Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_ENUM_VARIANT",
                "resolve",
                format!("Unsupported path '{full}'"),
                span,
                "Use up to 4 path segments (e.g. m::CONST, m::Enum::Var, m::Layout::field::count)",
            )),
        }
    }

    fn lower_layout_addr_expr(
        &mut self,
        layout: &str,
        field: &str,
        index: Option<&RawExpr>,
        subfield: Option<&str>,
        locals: &mut Vec<BTreeSet<String>>,
        span: Span,
    ) -> Result<Expr, InfimumDiagnostic> {
        let ldef = self.layouts.get(layout).ok_or_else(|| {
            InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Unknown layout '{layout}' in '{layout}.{field}'"),
                span,
                format!("Declare 'layout {layout}' before accessing '{layout}.{field}'"),
            )
        })?;
        if !ldef.fields.iter().any(|f| f.name == field) {
            let names: Vec<&str> = ldef.fields.iter().take(12).map(|f| f.name.as_str()).collect();
            return Err(InfimumDiagnostic::new(
                "E061_UNKNOWN_LAYOUT_FIELD",
                "resolve",
                format!("Layout '{layout}' has no field '{field}'"),
                span,
                format!("Available fields in 'layout {layout}': {}", names.join(", ")),
            ));
        }
        let meta = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))
            .cloned()
            .ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!("Missing layout metadata for '{layout}.{field}'"),
                    span,
                    "Internal layout metadata error",
                )
            })?;

        if meta.is_array && index.is_none() {
            let hint = match &meta.struct_type {
                Some(_) => format!(
                    "Use '{layout}.{field}[i].<field>' to access an element, or '{layout}::{field}' for the base word address"
                ),
                None => format!(
                    "Use '{layout}.{field}[i]' to access an element, or '{layout}::{field}' for the base word address"
                ),
            };
            return Err(InfimumDiagnostic::new(
                "E061_LAYOUT_ARRAY_ACCESS",
                "resolve",
                format!("Array field '{layout}.{field}' requires an index '[i]'"),
                span,
                hint,
            ));
        }
        if !meta.is_array && index.is_some() {
            return Err(InfimumDiagnostic::new(
                "E061_LAYOUT_ARRAY_ACCESS",
                "resolve",
                format!("Field '{layout}.{field}' is not an array; cannot index with '[...]'"),
                span,
                format!("Remove '[...]' or declare '{field}' as 'words[N]' / 'Struct[N]' in 'layout {layout}'"),
            ));
        }

        let sub_off: i64 = match (&meta.struct_type, subfield) {
            (Some(sname), Some(sf)) => {
                let sdef = self.structs.get(sname).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_LAYOUT_FIELD",
                        "resolve",
                        format!("Unknown struct type '{sname}'"),
                        span,
                        format!("Declare 'struct {sname} {{ ... }}'"),
                    )
                })?;
                *sdef.fields.get(sf).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_LAYOUT_FIELD",
                        "resolve",
                        format!("Struct '{sname}' has no field '{sf}'"),
                        span,
                        format!(
                            "Available fields in 'struct {sname}': {}",
                            sdef.field_names.join(", ")
                        ),
                    )
                })?
            }
            (Some(sname), None) => {
                let first = self
                    .structs
                    .get(sname)
                    .and_then(|s| s.field_names.first())
                    .map(|s| s.as_str())
                    .unwrap_or("x");
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!(
                        "Struct field '{layout}.{field}' of type '{sname}' requires a subfield access"
                    ),
                    span,
                    format!(
                        "Access a specific field, e.g. '{layout}.{field}{}.{first}'",
                        if meta.is_array { "[i]" } else { "" }
                    ),
                ));
            }
            (None, Some(sf)) => {
                return Err(InfimumDiagnostic::new(
                    "E061_UNKNOWN_LAYOUT_FIELD",
                    "resolve",
                    format!(
                        "Field '{layout}.{field}' is a plain word, not a struct; cannot access '.{sf}'"
                    ),
                    span,
                    format!("Remove '.{sf}' or declare '{field}' with a struct type"),
                ));
            }
            (None, None) => 0,
        };

        let base_word = meta.offset + sub_off;
        match index {
            None => Ok(Expr::IntLit(base_word)),
            Some(idx_raw) => {
                let idx_expr = self.lower_expr(idx_raw, locals)?;
                let scaled = if meta.stride == 1 {
                    idx_expr
                } else {
                    Expr::Binary {
                        op: BinOp::Mul,
                        lhs: Box::new(idx_expr),
                        rhs: Box::new(Expr::IntLit(meta.stride)),
                    }
                };
                if base_word == 0 {
                    Ok(scaled)
                } else {
                    Ok(Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(Expr::IntLit(base_word)),
                        rhs: Box::new(scaled),
                    })
                }
            }
        }
    }

    fn resolve_std_text_fn(&self, name: &str, span: Span) -> Result<String, InfimumDiagnostic> {
        if let Some(tid) = self.std_text_uid {
            if let Some(sym) = self.tables[tid].exports.fns.get(name) {
                return Ok(sym.canonical_name.clone());
            }
        }
        if let Some(sym) = self.lookup_fn_sym(None, name, span)? {
            return Ok(sym.canonical_name.clone());
        }
        Err(InfimumDiagnostic::new(
            "E064_CONST_EVAL",
            "resolve",
            format!("ui_fmt requires '{name}' from std/text.ifm"),
            span,
            "Import 'use \"std/text.ifm\";' or 'use \"std/prelude.ifm\";'",
        ))
    }

    fn inline_clamp_nonneg(&mut self, idx_expr: Expr, max_val: i64) -> Expr {
        let tmp = format!("__ifm_idx_{}", self.addr_counter);
        self.addr_counter += 1;
        Expr::Block {
            stmts: vec![Stmt::Let {
                name: tmp.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: idx_expr,
            }],
            tail: Box::new(Expr::If {
                cond: Box::new(Expr::Binary {
                    op: BinOp::Lt,
                    lhs: Box::new(Expr::Var(tmp.clone())),
                    rhs: Box::new(Expr::IntLit(0)),
                }),
                then_branch: Box::new(Expr::IntLit(0)),
                else_branch: Box::new(Expr::If {
                    cond: Box::new(Expr::Binary {
                        op: BinOp::Gt,
                        lhs: Box::new(Expr::Var(tmp.clone())),
                        rhs: Box::new(Expr::IntLit(max_val)),
                    }),
                    then_branch: Box::new(Expr::IntLit(max_val)),
                    else_branch: Box::new(Expr::Var(tmp)),
                }),
            }),
        }
    }

    pub(super) fn register_item(&mut self, item: RawItem, owner_u: &RawUnit) -> Result<(), InfimumDiagnostic> {
        match item {
            RawItem::Const(rc) => {
                let cname = unit_canonical_name(owner_u, &rc.name, false);
                let val = self.eval_const(&rc.expr, rc.span)?;
                let matches_ty = matches!(
                    (&rc.ty, val),
                    (Type::I64, ConstValue::Int(_))
                        | (Type::Fixed, ConstValue::Fixed(_))
                        | (Type::Bool, ConstValue::Bool(_))
                );
                if !matches_ty {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "const-eval",
                        format!(
                            "Constant '{}' declared as '{}' does not match initializer value",
                            rc.name, rc.ty
                        ),
                        rc.span,
                        "Ensure the constant's declared type matches its initializer expression",
                    ));
                }
                let def = ConstDef {
                    name: rc.name.clone(),
                    ty: rc.ty,
                    value: val,
                };
                self.consts.insert(cname, def);
            }
            RawItem::Enum(re) => {
                let cname = unit_canonical_name(owner_u, &re.name, false);
                let mut next_val: i64 = 0;
                let mut seen_variants = BTreeSet::new();
                let mut variants = Vec::with_capacity(re.variants.len());
                for rv in re.variants {
                    if !seen_variants.insert(rv.name.clone()) {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "resolve",
                            format!("Duplicate variant '{}' in enum '{}'", rv.name, re.name),
                            rv.span,
                            "Ensure all enum variant names are unique",
                        ));
                    }
                    let val = match rv.value {
                        Some(expr) => self.eval_const_i64(
                            &expr,
                            rv.span,
                            &format!("enum variant {}::{}", re.name, rv.name),
                        )?,
                        None => next_val,
                    };
                    next_val = val.checked_add(1).ok_or_else(|| const_overflow("+", rv.span))?;
                    variants.push(EnumVariant {
                        name: rv.name,
                        value: val,
                    });
                }
                let def = EnumDef {
                    name: re.name.clone(),
                    variants,
                };
                self.enums.insert(cname, def);
            }
            RawItem::Struct(rs) => {
                let cname = unit_canonical_name(owner_u, &rs.name, false);
                if rs.fields.is_empty() {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "layout",
                        format!("Struct '{}' must declare at least one field", rs.name),
                        rs.span,
                        "Add at least one 'field: word' inside the struct body",
                    ));
                }
                let mut fields = BTreeMap::new();
                let mut field_invs = BTreeMap::new();
                let mut field_fixed = BTreeSet::new();
                let mut field_names = Vec::with_capacity(rs.fields.len());
                for (idx, (fname, is_fixed, finv, fspan)) in rs.fields.into_iter().enumerate() {
                    if fields.insert(fname.clone(), idx as i64).is_some() {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "layout",
                            format!("Duplicate field '{}' in struct '{}'", fname, rs.name),
                            fspan,
                            "Ensure all field names within a struct are unique",
                        ));
                    }
                    if is_fixed {
                        field_fixed.insert(fname.clone());
                    }
                    if let Some(ri) = finv {
                        let lo = self.eval_const_i64(&ri.lo, ri.span, "struct field invariant lower bound")?;
                        let hi_raw = self.eval_const_i64(&ri.hi, ri.span, "struct field invariant upper bound")?;
                        let hi = if ri.inclusive { hi_raw } else { hi_raw.saturating_sub(1) };
                        if lo > hi {
                            return Err(InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "layout",
                                format!("Empty invariant range {lo}..={hi} on struct field '{}::{}'", rs.name, fname),
                                ri.span,
                                "Ensure lower bound <= upper bound in 'inv lo..=hi'",
                            ));
                        }
                        field_invs.insert(fname.clone(), (lo, hi));
                    }
                    field_names.push(fname);
                }
                let size = field_names.len() as i64;
                self.structs.insert(
                    cname,
                    StructDef {
                        fields,
                        field_invs,
                        field_fixed,
                        field_names,
                        size,
                    },
                );
            }
            RawItem::Layout(rl) => {
                let cname = unit_canonical_name(owner_u, &rl.name, false);
                let auto_placed = rl.base_expr.is_none();
                let base = match rl.base_expr {
                    Some(ref expr) => {
                        self.eval_const_i64(expr, rl.span, &format!("layout '{}' base", rl.name))?
                    }
                    None => self.auto_layout_cursor,
                };
                if base < 0 {
                    return Err(InfimumDiagnostic::new(
                        "E064_CONST_EVAL",
                        "layout",
                        format!("Layout '{}' base offset ({base}) cannot be negative", rl.name),
                        rl.span,
                        "Specify a non-negative base word offset (e.g. 'at 0')",
                    ));
                }
                let mut cursor = base;
                let mut seen_fields = BTreeSet::new();
                let mut fields = Vec::with_capacity(rl.fields.len());
                for rf in rl.fields {
                    if !seen_fields.insert(rf.name.clone()) {
                        return Err(InfimumDiagnostic::new(
                            "E065_DUPLICATE_DECL",
                            "layout",
                            format!("Duplicate field '{}' in layout '{}'", rf.name, rl.name),
                            rf.span,
                            "Ensure all field names within a layout are unique",
                        ));
                    }
                    let (resolved_struct_type, stride) = match &rf.struct_type {
                        Some(sname) => {
                            let scanon = self.resolve_struct_canon(
                                rf.struct_module.as_deref(),
                                sname,
                                rf.span,
                            )?;
                            let sdef = self.structs.get(&scanon).ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_LAYOUT_FIELD",
                                    "layout",
                                    format!(
                                        "Unknown type '{}' for layout field '{}::{}'",
                                        sname, rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Use 'word', 'words[N]', or declare 'struct TypeName { ... }' before this layout",
                                )
                            })?;
                            (Some(scanon), sdef.size)
                        }
                        None => (None, 1),
                    };
                    let count = match rf.len_expr {
                        Some(ref expr) => {
                            let l = self.eval_const_i64(
                                expr,
                                rf.span,
                                &format!("layout field '{}::{}' length", rl.name, rf.name),
                            )?;
                            if l <= 0 {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!(
                                        "Layout buffer '{}::{}' length ({l}) must be positive",
                                        rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Use a positive integer length for '[N]'",
                                ));
                            }
                            l
                        }
                        None => 1,
                    };
                    let len = count
                        .checked_mul(stride)
                        .ok_or_else(|| const_overflow("*", rf.span))?;
                    let offset = match rf.at_expr {
                        Some(ref expr) => {
                            let o = self.eval_const_i64(
                                expr,
                                rf.span,
                                &format!("layout field '{}::{}' offset", rl.name, rf.name),
                            )?;
                            if o < 0 {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!(
                                        "Layout field '{}::{}' offset ({o}) cannot be negative",
                                        rl.name, rf.name
                                    ),
                                    rf.span,
                                    "Specify a non-negative word offset after 'at'",
                                ));
                            }
                            o
                        }
                        None => cursor,
                    };
                    let end = offset
                        .checked_add(len)
                        .ok_or_else(|| const_overflow("+", rf.span))?;
                    cursor = end;
                    self.layout_spans.push((
                        offset,
                        end,
                        format!("{}::{}", rl.name, rf.name),
                        rf.span,
                    ));
                    let field_inv = match rf.inv {
                        Some(ri) => {
                            let lo = self.eval_const_i64(&ri.lo, ri.span, "layout field invariant lower bound")?;
                            let hi_raw = self.eval_const_i64(&ri.hi, ri.span, "layout field invariant upper bound")?;
                            let hi = if ri.inclusive { hi_raw } else { hi_raw.saturating_sub(1) };
                            if lo > hi {
                                return Err(InfimumDiagnostic::new(
                                    "E064_CONST_EVAL",
                                    "layout",
                                    format!("Empty invariant range {lo}..={hi} on layout field '{}::{}'", rl.name, rf.name),
                                    ri.span,
                                    "Ensure lower bound <= upper bound in 'inv lo..=hi'",
                                ));
                            }
                            Some((lo, hi))
                        }
                        None => None,
                    };
                    self.layout_field_meta.insert(
                        (cname.clone(), rf.name.clone()),
                        LayoutFieldMeta {
                            offset,
                            is_array: rf.is_array,
                            is_vec: rf.is_vec,
                            is_fixed: rf.is_fixed,
                            struct_type: resolved_struct_type,
                            stride,
                            count,
                            inv: field_inv,
                        },
                    );
                    fields.push(LayoutField {
                        name: rf.name,
                        offset,
                        len,
                        is_array: rf.is_array,
                        is_vec: rf.is_vec,
                        cap: count,
                        stride,
                    });
                }
                if auto_placed && cursor > self.auto_layout_cursor {
                    self.auto_layout_cursor = cursor;
                }
                let def = LayoutDef {
                    name: rl.name.clone(),
                    base,
                    fields,
                };
                self.layouts.insert(cname, def);
            }
            RawItem::Mod(_) => {}
        }
        Ok(())
    }

    pub(super) fn verify_no_layout_overlaps(&self) -> Result<(), InfimumDiagnostic> {
        // Protect std/gfx reserved runtime words (11 = cursor_kind, 12 = prim_count) in GPU apps
        if self.has_std_gfx {
            for (start, end, label, span) in &self.layout_spans {
                let field_name = label.rsplit("::").next().unwrap_or("");
                if *start <= 11 && 11 < *end && field_name != "cursor_kind" {
                    return Err(InfimumDiagnostic::new(
                        "E066_RESERVED_RUNTIME_ADDR",
                        "layout",
                        format!(
                            "Layout field '{label}' ([{start}..{end})) overwrites reserved std/gfx slot 11 (cursor_kind)"
                        ),
                        *span,
                        "Omit 'at 0' (layouts without 'at' auto-place safely at word 705536+) or explicitly pin 'cursor_kind: word at 11'",
                    ));
                }
                if *start <= 12 && 12 < *end && field_name != "prim_count" {
                    return Err(InfimumDiagnostic::new(
                        "E066_RESERVED_RUNTIME_ADDR",
                        "layout",
                        format!(
                            "Layout field '{label}' ([{start}..{end})) overwrites reserved std/gfx slot 12 (prim_count), which causes a black screen"
                        ),
                        *span,
                        "Omit 'at 0' (layouts without 'at' auto-place safely at word 705536+) or explicitly pin 'prim_count: word at 12'",
                    ));
                }
            }
        }

        const LINEAR_MEM_MAX_WORDS: i64 = 16_777_216; // Up to 1024 Wasm pages = 64 MiB (auto-sized 8/16/32/64 MiB)
        for (start, end, label, span) in &self.layout_spans {
            if *start < 0 || *end > LINEAR_MEM_MAX_WORDS {
                return Err(InfimumDiagnostic::new(
                    "E067_LAYOUT_OUT_OF_MEMORY",
                    "layout",
                    format!(
                        "Layout field '{label}' ([{start}..{end})) exceeds the 64 MiB linear memory ceiling ({LINEAR_MEM_MAX_WORDS} words)"
                    ),
                    *span,
                    format!(
                        "Place '{label}' strictly within [0..{LINEAR_MEM_MAX_WORDS}) words (e.g. shrink large buffers or lower its 'at' offset) to prevent address wrap-around corruption"
                    ),
                ));
            }
        }

        let mut intervals = self.layout_spans.clone();
        intervals.sort_by(|a, b| a.0.cmp(&b.0).then_with(|| a.1.cmp(&b.1)));

        let mut max_end: Option<(i64, i64, String)> = None;
        for (start, end, label, span) in intervals {
            if let Some((prev_start, prev_end, ref prev_label)) = max_end {
                if prev_end > start {
                    return Err(InfimumDiagnostic::new(
                        "E062_LAYOUT_OVERLAP",
                        "layout",
                        format!(
                            "Linear memory layout overlap: '{prev_label}' ([{prev_start}..{prev_end})) overlaps '{label}' ([{start}..{end}))"
                        ),
                        span,
                        format!(
                            "Move '{label}' to at least word offset {prev_end} (or shrink '{prev_label}') so all layout regions are strictly disjoint"
                        ),
                    ));
                }
            }
            let replace = match &max_end {
                Some((_, prev_end, _)) => end > *prev_end,
                None => true,
            };
            if replace {
                max_end = Some((start, end, label));
            }
        }
        Ok(())
    }

    pub(super) fn lower_fn(&mut self, raw: RawFnDef) -> Result<FnDef, InfimumDiagnostic> {
        let mut params = Vec::with_capacity(raw.params.len());
        let mut init_struct_vars = BTreeMap::new();
        let mut init_slice_vars = BTreeMap::new();
        for (pname, raw_ty) in &raw.params {
            let resolved_ty = self.resolve_raw_type(raw_ty)?;
            if let RawType::Named { module, name, span } = raw_ty {
                let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
                init_struct_vars.insert(pname.clone(), scanon);
            } else if let RawType::Slice(elem) = raw_ty {
                let smeta = self.resolve_slice_elem(elem)?;
                init_slice_vars.insert(pname.clone(), smeta);
            }
            params.push(Param {
                name: pname.clone(),
                ty: resolved_ty,
            });
        }
        let ret_type = self.resolve_raw_type(&raw.ret_type)?;
        if let RawType::Named { module, name, span } = &raw.ret_type {
            let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
            init_struct_vars.insert("result".to_string(), scanon);
        } else if let RawType::Slice(elem) = &raw.ret_type {
            let smeta = self.resolve_slice_elem(elem)?;
            init_slice_vars.insert("result".to_string(), smeta);
        }
        self.struct_vars.push(init_struct_vars);
        self.slice_vars.push(init_slice_vars);

        let mut locals: Vec<BTreeSet<String>> = vec![params.iter().map(|p| p.name.clone()).collect()];

        let mut requires = Vec::with_capacity(raw.requires.len());
        for r in &raw.requires {
            requires.push(self.lower_expr(r, &mut locals)?);
        }
        let mut ensures = Vec::with_capacity(raw.ensures.len());
        for e in &raw.ensures {
            ensures.push(self.lower_expr(e, &mut locals)?);
        }
        let body = self.lower_expr(&raw.body, &mut locals)?;
        self.struct_vars.pop();
        self.slice_vars.pop();

        Ok(FnDef {
            name: raw.name,
            exported: raw.exported,
            params,
            effects: raw.effects,
            ret_type,
            requires,
            ensures,
            body,
            source_file: raw.source_file,
            span: Some(raw.span),
        })
    }

    fn lookup_layout_inv(
        &self,
        layout: &str,
        field: &str,
        subfield: Option<&str>,
    ) -> Option<(i64, i64)> {
        let meta = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))?;
        if let Some(sub) = subfield {
            let sname = meta.struct_type.as_deref()?;
            let sdef = self.structs.get(sname)?;
            sdef.field_invs.get(sub).copied()
        } else {
            meta.inv
        }
    }

    fn lookup_layout_fixed(
        &self,
        layout: &str,
        field: &str,
        subfield: Option<&str>,
    ) -> bool {
        let Some(meta) = self
            .layout_field_meta
            .get(&(layout.to_string(), field.to_string()))
        else {
            return false;
        };
        if let Some(sub) = subfield {
            let Some(sname) = meta.struct_type.as_deref() else {
                return false;
            };
            let Some(sdef) = self.structs.get(sname) else {
                return false;
            };
            sdef.field_fixed.contains(sub)
        } else {
            meta.is_fixed
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn desugar_ui_fmt(
        &mut self,
        fmt_str: &str,
        x_raw: &RawExpr,
        y_raw: &RawExpr,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        col_raw: &RawExpr,
        clip_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        enum Seg {
            Lit(String),
            Expr(RawExpr, Option<i64>),
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut lit = String::new();
        let chars: Vec<char> = fmt_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                if !lit.is_empty() {
                    segs.push(Seg::Lit(std::mem::take(&mut lit)));
                }
                i += 1;
                let start = i;
                let mut depth = 1usize;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E070_INVALID_FMT_STRING",
                        "parse",
                        format!("Unclosed '{{' in ui_fmt string \"{fmt_str}\""),
                        Span::zero(),
                        "Close each placeholder with '}' (or escape a literal brace as '{{')",
                    ));
                }
                let expr_src: String = chars[start..i].iter().collect();
                i += 1; // skip '}'
                let (expr_body, fixed_prec) = if let Some((lhs, prec_str)) = expr_src.rsplit_once(":.") {
                    let prec = prec_str.trim().parse::<i64>().map_err(|_| {
                        InfimumDiagnostic::new(
                            "E070_INVALID_FMT_STRING",
                            "parse",
                            format!("Invalid precision specifier ':.{prec_str}' in ui_fmt string \"{fmt_str}\""),
                            Span::zero(),
                            "Use ':.0', ':.1', ':.2', ':.3', or ':.4' for fixed-point formatting",
                        )
                    })?;
                    (lhs.trim(), Some(prec.clamp(0, 4)))
                } else {
                    (expr_src.trim(), None)
                };
                let toks = tokenize(expr_body)?;
                let mut sub_parser = Parser { tokens: toks, idx: 0 };
                let parsed_expr = sub_parser.parse_expr()?;
                segs.push(Seg::Expr(parsed_expr, fixed_prec));
            } else if chars[i] == '}' && i + 1 < chars.len() && chars[i + 1] == '}' {
                lit.push('}');
                i += 2;
            } else {
                lit.push(chars[i]);
                i += 1;
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }

        let id = self.addr_counter;
        self.addr_counter += 1;
        let vx = format!("__ifm_fx_{id}");
        let vw = format!("__ifm_fw_{id}");
        let vy = format!("__ifm_fy_{id}");
        let vsz = format!("__ifm_fsz_{id}");
        let vst = format!("__ifm_fst_{id}");
        let vcol = format!("__ifm_fcol_{id}");
        let vclip = format!("__ifm_fclip_{id}");

        let mut stmts = vec![
            Stmt::Let {
                name: vx.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(x_raw, locals)?,
            },
            Stmt::Let {
                name: vw.clone(),
                mutable: true,
                ty: Some(Type::I64),
                value: Expr::IntLit(0),
            },
            Stmt::Let {
                name: vy.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(y_raw, locals)?,
            },
            Stmt::Let {
                name: vsz.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(sz_raw, locals)?,
            },
            Stmt::Let {
                name: vst.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(st_raw, locals)?,
            },
            Stmt::Let {
                name: vcol.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(col_raw, locals)?,
            },
            Stmt::Let {
                name: vclip.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(clip_raw, locals)?,
            },
        ];

        for seg in segs {
            let cur_x = Expr::Binary {
                op: BinOp::Add,
                lhs: Box::new(Expr::Var(vx.clone())),
                rhs: Box::new(Expr::Var(vw.clone())),
            };
            let call = match seg {
                Seg::Lit(s) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_text", Span::zero())?),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        Expr::StrLit(s),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
                Seg::Expr(e, Some(prec)) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_fixed", Span::zero())?),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        self.lower_expr(&e, locals)?,
                        Expr::IntLit(prec),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
                Seg::Expr(e, None) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_int", Span::zero())?),
                    args: vec![
                        cur_x,
                        Expr::Var(vy.clone()),
                        self.lower_expr(&e, locals)?,
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                        Expr::Var(vcol.clone()),
                        Expr::Var(vclip.clone()),
                    ],
                },
            };
            stmts.push(Stmt::Assign {
                name: vw.clone(),
                value: Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(vw.clone())),
                    rhs: Box::new(call),
                },
            });
        }

        Ok(Expr::Block {
            stmts,
            tail: Box::new(Expr::Var(vw)),
        })
    }

    fn desugar_ui_fmt_width(
        &mut self,
        fmt_str: &str,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        enum Seg {
            Lit(String),
            Expr(RawExpr, Option<i64>),
        }
        let mut segs: Vec<Seg> = Vec::new();
        let mut lit = String::new();
        let chars: Vec<char> = fmt_str.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            if chars[i] == '{' {
                if i + 1 < chars.len() && chars[i + 1] == '{' {
                    lit.push('{');
                    i += 2;
                    continue;
                }
                if !lit.is_empty() {
                    segs.push(Seg::Lit(std::mem::take(&mut lit)));
                }
                i += 1;
                let start = i;
                let mut depth = 1usize;
                while i < chars.len() && depth > 0 {
                    if chars[i] == '{' {
                        depth += 1;
                    } else if chars[i] == '}' {
                        depth -= 1;
                        if depth == 0 {
                            break;
                        }
                    }
                    i += 1;
                }
                if depth != 0 {
                    return Err(InfimumDiagnostic::new(
                        "E070_INVALID_FMT_STRING",
                        "parse",
                        format!("Unclosed '{{' in ui_fmt_width string \"{fmt_str}\""),
                        Span::zero(),
                        "Close each placeholder with '}'",
                    ));
                }
                let expr_src: String = chars[start..i].iter().collect();
                i += 1;
                let (expr_body, fixed_prec) = if let Some((lhs, prec_str)) = expr_src.rsplit_once(":.") {
                    let prec = prec_str.trim().parse::<i64>().map_err(|_| {
                        InfimumDiagnostic::new(
                            "E070_INVALID_FMT_STRING",
                            "parse",
                            format!("Invalid precision specifier ':.{prec_str}' in ui_fmt_width string \"{fmt_str}\""),
                            Span::zero(),
                            "Use ':.0' .. ':.4' for fixed-point formatting",
                        )
                    })?;
                    (lhs.trim(), Some(prec.clamp(0, 4)))
                } else {
                    (expr_src.trim(), None)
                };
                let toks = tokenize(expr_body)?;
                let mut sub_parser = Parser { tokens: toks, idx: 0 };
                let parsed_expr = sub_parser.parse_expr()?;
                segs.push(Seg::Expr(parsed_expr, fixed_prec));
            } else if chars[i] == '}' && i + 1 < chars.len() && chars[i + 1] == '}' {
                lit.push('}');
                i += 2;
            } else {
                lit.push(chars[i]);
                i += 1;
            }
        }
        if !lit.is_empty() {
            segs.push(Seg::Lit(lit));
        }

        let id = self.addr_counter;
        self.addr_counter += 1;
        let vw = format!("__ifm_fw_{id}");
        let vsz = format!("__ifm_fwsz_{id}");
        let vst = format!("__ifm_fwst_{id}");
        let mut stmts = vec![
            Stmt::Let {
                name: vw.clone(),
                mutable: true,
                ty: Some(Type::I64),
                value: Expr::IntLit(0),
            },
            Stmt::Let {
                name: vsz.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(sz_raw, locals)?,
            },
            Stmt::Let {
                name: vst.clone(),
                mutable: false,
                ty: Some(Type::I64),
                value: self.lower_expr(st_raw, locals)?,
            },
        ];
        for seg in segs {
            let add_call = match seg {
                Seg::Lit(s) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_text_width", Span::zero())?),
                    args: vec![Expr::StrLit(s), Expr::Var(vsz.clone()), Expr::Var(vst.clone())],
                },
                Seg::Expr(e, Some(prec)) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_fixed_width", Span::zero())?),
                    args: vec![
                        self.lower_expr(&e, locals)?,
                        Expr::IntLit(prec),
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                    ],
                },
                Seg::Expr(e, None) => Expr::Call {
                    target: TargetRef::Named(self.resolve_std_text_fn("ui_int_width", Span::zero())?),
                    args: vec![
                        self.lower_expr(&e, locals)?,
                        Expr::Var(vsz.clone()),
                        Expr::Var(vst.clone()),
                    ],
                },
            };
            stmts.push(Stmt::Assign {
                name: vw.clone(),
                value: Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::Var(vw.clone())),
                    rhs: Box::new(add_call),
                },
            });
        }
        Ok(Expr::Block {
            stmts,
            tail: Box::new(Expr::Var(vw)),
        })
    }

    #[allow(clippy::too_many_arguments)]
    fn desugar_ui_fmt_centered(
        &mut self,
        fmt_str: &str,
        cx_raw: &RawExpr,
        y_raw: &RawExpr,
        sz_raw: &RawExpr,
        st_raw: &RawExpr,
        col_raw: &RawExpr,
        clip_raw: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        let id = self.addr_counter;
        self.addr_counter += 1;
        let vtw = format!("__ifm_fctw_{id}");
        let vcx = format!("__ifm_fccx_{id}");
        let w_expr = self.desugar_ui_fmt_width(fmt_str, sz_raw, st_raw, locals)?;
        let start_x_raw = RawExpr::Binary {
            op: BinOp::Sub,
            lhs: Box::new(RawExpr::Var(vcx.clone())),
            rhs: Box::new(RawExpr::Binary {
                op: BinOp::Div,
                lhs: Box::new(RawExpr::Var(vtw.clone())),
                rhs: Box::new(RawExpr::IntLit(2)),
            }),
        };
        let draw_expr = self.desugar_ui_fmt(
            fmt_str,
            &start_x_raw,
            y_raw,
            sz_raw,
            st_raw,
            col_raw,
            clip_raw,
            locals,
        )?;
        Ok(Expr::Block {
            stmts: vec![
                Stmt::Let {
                    name: vcx,
                    mutable: false,
                    ty: Some(Type::I64),
                    value: self.lower_expr(cx_raw, locals)?,
                },
                Stmt::Let {
                    name: vtw,
                    mutable: false,
                    ty: Some(Type::I64),
                    value: w_expr,
                },
            ],
            tail: Box::new(draw_expr),
        })
    }

    fn is_shadowed(locals: &[BTreeSet<String>], name: &str) -> bool {
        locals.iter().any(|frame| frame.contains(name))
    }

    fn active_loop_guard(&self) -> Option<Expr> {
        let ctx = self.loop_stack.last()?;
        match (&ctx.brk_var, &ctx.cont_var) {
            (Some(b), Some(c)) => Some(Expr::Binary {
                op: BinOp::And,
                lhs: Box::new(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Var(b.clone())),
                }),
                rhs: Box::new(Expr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(Expr::Var(c.clone())),
                }),
            }),
            (Some(b), None) => Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Var(b.clone())),
            }),
            (None, Some(c)) => Some(Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(Expr::Var(c.clone())),
            }),
            (None, None) => None,
        }
    }

    fn lower_stmts(
        &mut self,
        stmts: &[RawStmt],
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Vec<Stmt>, InfimumDiagnostic> {
        let mut out = Vec::with_capacity(stmts.len());
        let mut idx = 0;
        while idx < stmts.len() {
            let s = &stmts[idx];
            match s {
                RawStmt::Let {
                    name,
                    mutable,
                    ty,
                    value,
                } => {
                    let resolved_ty = match ty {
                        Some(rt) => Some(self.resolve_raw_type(rt)?),
                        None => None,
                    };
                    let inferred_struct = match ty {
                        Some(RawType::Named { module, name, span }) => {
                            Some(self.resolve_struct_canon(module.as_deref(), name, *span)?)
                        }
                        _ => self.infer_raw_expr_struct(value),
                    };
                    let inferred_slice = match ty {
                        Some(RawType::Slice(elem)) => Some(self.resolve_slice_elem(elem)?),
                        _ => self.infer_raw_expr_slice(value),
                    };
                    let v = self.lower_expr(value, locals)?;
                    if let Some(top) = locals.last_mut() {
                        top.insert(name.clone());
                    }
                    if let Some(sname) = inferred_struct {
                        if let Some(sv_top) = self.struct_vars.last_mut() {
                            sv_top.insert(name.clone(), sname);
                        }
                    }
                    if let Some(smeta) = inferred_slice {
                        if let Some(sl_top) = self.slice_vars.last_mut() {
                            sl_top.insert(name.clone(), smeta);
                        }
                    }
                    out.push(Stmt::Let {
                        name: name.clone(),
                        mutable: *mutable,
                        ty: resolved_ty,
                        value: v,
                    });
                }
                RawStmt::LetTuple { names, value } => {
                    let v = self.lower_expr(value, locals)?;
                    if let Some(top) = locals.last_mut() {
                        for (n, _) in names {
                            top.insert(n.clone());
                        }
                    }
                    out.push(Stmt::LetTuple {
                        names: names.clone(),
                        value: v,
                    });
                }
                RawStmt::Assign { name, op, value } => {
                    let rhs = self.lower_expr(value, locals)?;
                    let final_val = match op {
                        None => rhs,
                        Some(binop) => Expr::Binary {
                            op: *binop,
                            lhs: Box::new(Expr::Var(name.clone())),
                            rhs: Box::new(rhs),
                        },
                    };
                    out.push(Stmt::Assign {
                        name: name.clone(),
                        value: final_val,
                    });
                }
                RawStmt::LayoutStore {
                    module,
                    layout,
                    field,
                    index,
                    subfield,
                    op,
                    value,
                    span,
                } => {
                    // Case A: Local value-struct field mutation (`v.x = e;` / `v.x += e;`)
                    if module.is_none() && index.is_none() && subfield.is_none() {
                        if let Some(sname) = self.lookup_struct_var(layout) {
                            let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_STRUCT",
                                    "parse",
                                    format!("Unknown struct '{sname}'"),
                                    *span,
                                    "Declare the struct before using it",
                                )
                            })?;
                            let fidx = *sdef.fields.get(field).ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E063_UNKNOWN_FIELD",
                                    "parse",
                                    format!("Struct '{sname}' has no field '{field}'"),
                                    *span,
                                    format!("Available fields on '{sname}': {}", sdef.field_names.join(", ")),
                                )
                            })? as usize;
                            let rhs = self.lower_expr(value, locals)?;
                            let new_fval = match op {
                                None => rhs,
                                Some(binop) => Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::TupleField {
                                        expr: Box::new(Expr::Var(layout.clone())),
                                        index: fidx,
                                    }),
                                    rhs: Box::new(rhs),
                                },
                            };
                            let mut elems = Vec::with_capacity(sdef.field_names.len());
                            for k in 0..sdef.field_names.len() {
                                if k == fidx {
                                    elems.push(new_fval.clone());
                                } else {
                                    elems.push(Expr::TupleField {
                                        expr: Box::new(Expr::Var(layout.clone())),
                                        index: k,
                                    });
                                }
                            }
                            out.push(Stmt::Assign {
                                name: layout.clone(),
                                value: Expr::TupleLit(elems),
                            });
                            idx += 1;
                            continue;
                        }
                    }
                    let lcanon = self.resolve_layout_canon(module.as_deref(), layout, *span)?;
                    // Case B: Whole-struct store into layout (`World.hero = v;` or `World.flock[i] = s;`)
                    if subfield.is_none() && op.is_none() {
                        if let Some(meta) = self
                            .layout_field_meta
                            .get(&(lcanon.clone(), field.clone()))
                            .cloned()
                        {
                            if let Some(sname) = meta.struct_type {
                                let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                    InfimumDiagnostic::new(
                                        "E061_UNKNOWN_STRUCT",
                                        "layout",
                                        format!("Unknown struct '{sname}'"),
                                        *span,
                                        "Declare the struct before using it",
                                    )
                                })?;
                                let rhs = self.lower_expr(value, locals)?;
                                let sv_tmp = format!("__ifm_sv_{}", self.addr_counter);
                                self.addr_counter += 1;
                                out.push(Stmt::Let {
                                    name: sv_tmp.clone(),
                                    mutable: false,
                                    ty: Some(self.struct_tuple_type(&sdef)),
                                    value: rhs,
                                });
                                let base_addr_var = if let Some(idx_raw) = index {
                                    let idx_e = self.lower_expr(idx_raw, locals)?;
                                    let clamped =
                                        self.inline_clamp_nonneg(idx_e, (meta.count - 1).max(0));
                                    let base_expr = Expr::Binary {
                                        op: BinOp::Add,
                                        lhs: Box::new(Expr::IntLit(meta.offset)),
                                        rhs: Box::new(Expr::Binary {
                                            op: BinOp::Mul,
                                            lhs: Box::new(clamped),
                                            rhs: Box::new(Expr::IntLit(meta.stride)),
                                        }),
                                    };
                                    let bvar = format!("__ifm_saddr_{}", self.addr_counter);
                                    self.addr_counter += 1;
                                    out.push(Stmt::Let {
                                        name: bvar.clone(),
                                        mutable: false,
                                        ty: Some(Type::I64),
                                        value: base_expr,
                                    });
                                    Some(bvar)
                                } else {
                                    None
                                };
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    let f_addr = if let Some(ref bvar) = base_addr_var {
                                        if k == 0 {
                                            Expr::Var(bvar.clone())
                                        } else {
                                            Expr::Binary {
                                                op: BinOp::Add,
                                                lhs: Box::new(Expr::Var(bvar.clone())),
                                                rhs: Box::new(Expr::IntLit(k as i64)),
                                            }
                                        }
                                    } else {
                                        Expr::IntLit(meta.offset + k as i64)
                                    };
                                    out.push(Stmt::Expr(Expr::MemStore {
                                        index: Box::new(f_addr),
                                        value: Box::new(Expr::TupleField {
                                            expr: Box::new(Expr::Var(sv_tmp.clone())),
                                            index: k,
                                        }),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    }));
                                }
                                idx += 1;
                                continue;
                            }
                        }
                    }
                    let addr_expr = self.lower_layout_addr_expr(
                        &lcanon,
                        field,
                        index.as_ref(),
                        subfield.as_deref(),
                        locals,
                        *span,
                    )?;
                    let field_inv = self.lookup_layout_inv(&lcanon, field, subfield.as_deref());
                    let field_fixed = self.lookup_layout_fixed(&lcanon, field, subfield.as_deref());
                    let rhs = self.lower_expr(value, locals)?;
                    match op {
                        None => {
                            out.push(Stmt::Expr(Expr::MemStore {
                                index: Box::new(addr_expr),
                                value: Box::new(rhs),
                                inv: field_inv,
                                fixed_ty: field_fixed,
                            }));
                        }
                        Some(binop) => {
                            if index.is_some() {
                                let tmp = format!("__ifm_laddr_{}", self.addr_counter);
                                self.addr_counter += 1;
                                out.push(Stmt::Let {
                                    name: tmp.clone(),
                                    mutable: false,
                                    ty: Some(Type::I64),
                                    value: addr_expr,
                                });
                                let final_val = Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::MemLoad {
                                        index: Box::new(Expr::Var(tmp.clone())),
                                        inv: field_inv,
                                        fixed_ty: field_fixed,
                                    }),
                                    rhs: Box::new(rhs),
                                };
                                out.push(Stmt::Expr(Expr::MemStore {
                                    index: Box::new(Expr::Var(tmp)),
                                    value: Box::new(final_val),
                                    inv: field_inv,
                                    fixed_ty: field_fixed,
                                }));
                            } else {
                                let final_val = Expr::Binary {
                                    op: *binop,
                                    lhs: Box::new(Expr::MemLoad {
                                        index: Box::new(addr_expr.clone()),
                                        inv: field_inv,
                                        fixed_ty: field_fixed,
                                    }),
                                    rhs: Box::new(rhs),
                                };
                                out.push(Stmt::Expr(Expr::MemStore {
                                    index: Box::new(addr_expr),
                                    value: Box::new(final_val),
                                    inv: field_inv,
                                    fixed_ty: field_fixed,
                                }));
                            }
                        }
                    }
                }
                RawStmt::SliceStore(ss) => {
                    let RawSliceStore {
                        base,
                        index,
                        subfield,
                        op,
                        value,
                        span,
                    } = ss.as_ref();
                    let smeta = self.infer_raw_expr_slice(base).ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "parse",
                            "Cannot index-assign into non-slice expression",
                            *span,
                            "Index assignment 's[i] = val' requires a slice value of type '[T]'",
                        )
                    })?;
                    let base_e = self.lower_expr(base, locals)?;
                    let idx_e = self.lower_expr(index, locals)?;
                    let sb_var = format!("__ifm_sb_{}", self.addr_counter);
                    self.addr_counter += 1;
                    let si_var = format!("__ifm_si_{}", self.addr_counter);
                    self.addr_counter += 1;
                    out.push(Stmt::Let {
                        name: sb_var.clone(),
                        mutable: false,
                        ty: Some(Type::Slice(Box::new(smeta.elem_ty.clone()))),
                        value: base_e,
                    });
                    out.push(Stmt::Let {
                        name: si_var.clone(),
                        mutable: false,
                        ty: Some(Type::I64),
                        value: Expr::BoundsCheck {
                            index: Box::new(idx_e),
                            len: Box::new(Expr::SliceLen(Box::new(Expr::Var(sb_var.clone())))),
                            inclusive_hi: false,
                        },
                    });
                    let elem_offset = if smeta.stride == 1 {
                        Expr::Var(si_var)
                    } else {
                        Expr::Binary {
                            op: BinOp::Mul,
                            lhs: Box::new(Expr::Var(si_var)),
                            rhs: Box::new(Expr::IntLit(smeta.stride)),
                        }
                    };
                    let elem_addr = Expr::Binary {
                        op: BinOp::Add,
                        lhs: Box::new(Expr::SlicePtr(Box::new(Expr::Var(sb_var)))),
                        rhs: Box::new(elem_offset),
                    };

                    if let Some(sf) = subfield {
                        let scanon = smeta.struct_canon.as_ref().ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E063_UNKNOWN_FIELD",
                                "parse",
                                format!("Cannot access subfield '.{sf}' on scalar slice element"),
                                *span,
                                "Use 's[i] = val' for scalar slices '[i64]' / '[fixed]'",
                            )
                        })?;
                        let sdef = self.structs.get(scanon).cloned().ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E061_UNKNOWN_STRUCT",
                                "parse",
                                format!("Unknown struct '{scanon}'"),
                                *span,
                                "Declare the struct before using it",
                            )
                        })?;
                        let foff = *sdef.fields.get(sf).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E063_UNKNOWN_FIELD",
                                "parse",
                                format!("Struct '{scanon}' has no field '{sf}'"),
                                *span,
                                format!("Available fields: {}", sdef.field_names.join(", ")),
                            )
                        })?;
                        let finv = sdef.field_invs.get(sf).copied();
                        let ffixed = sdef.field_fixed.contains(sf);
                        let addr_var = format!("__ifm_sa_{}", self.addr_counter);
                        self.addr_counter += 1;
                        let faddr = if foff == 0 {
                            elem_addr
                        } else {
                            Expr::Binary {
                                op: BinOp::Add,
                                lhs: Box::new(elem_addr),
                                rhs: Box::new(Expr::IntLit(foff)),
                            }
                        };
                        out.push(Stmt::Let {
                            name: addr_var.clone(),
                            mutable: false,
                            ty: Some(Type::I64),
                            value: faddr,
                        });
                        let rhs = self.lower_expr(value, locals)?;
                        let final_val = match op {
                            None => rhs,
                            Some(binop) => Expr::Binary {
                                op: *binop,
                                lhs: Box::new(Expr::MemLoad {
                                    index: Box::new(Expr::Var(addr_var.clone())),
                                    inv: finv,
                                    fixed_ty: ffixed,
                                }),
                                rhs: Box::new(rhs),
                            },
                        };
                        out.push(Stmt::Expr(Expr::MemStore {
                            index: Box::new(Expr::Var(addr_var)),
                            value: Box::new(final_val),
                            inv: finv,
                            fixed_ty: ffixed,
                        }));
                    } else if let Some(ref scanon) = smeta.struct_canon {
                        if op.is_some() {
                            return Err(InfimumDiagnostic::new(
                                "E060_TYPE_MISMATCH",
                                "parse",
                                "Compound assignment (+=, -=, ...) is not supported on whole struct elements",
                                *span,
                                "Assign a whole struct with '=' or mutate an individual field like 's[i].hp += 1'",
                            ));
                        }
                        let sdef = self.structs.get(scanon).cloned().ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E061_UNKNOWN_STRUCT",
                                "parse",
                                format!("Unknown struct '{scanon}'"),
                                *span,
                                "Declare the struct before using it",
                            )
                        })?;
                        let rhs = self.lower_expr(value, locals)?;
                        out.extend(self.emit_store_struct_at(elem_addr, rhs, &sdef));
                    } else {
                        let addr_var = format!("__ifm_sa_{}", self.addr_counter);
                        self.addr_counter += 1;
                        out.push(Stmt::Let {
                            name: addr_var.clone(),
                            mutable: false,
                            ty: Some(Type::I64),
                            value: elem_addr,
                        });
                        let rhs = self.lower_expr(value, locals)?;
                        let final_val = match op {
                            None => rhs,
                            Some(binop) => Expr::Binary {
                                op: *binop,
                                lhs: Box::new(Expr::MemLoad {
                                    index: Box::new(Expr::Var(addr_var.clone())),
                                    inv: None,
                                    fixed_ty: smeta.is_fixed,
                                }),
                                rhs: Box::new(rhs),
                            },
                        };
                        out.push(Stmt::Expr(Expr::MemStore {
                            index: Box::new(Expr::Var(addr_var)),
                            value: Box::new(final_val),
                            inv: None,
                            fixed_ty: smeta.is_fixed,
                        }));
                    }
                }
                RawStmt::While {
                    cond,
                    max_iters,
                    body,
                    span,
                } => {
                    let max_val = self.eval_const_i64(max_iters, *span, "'while ... max' bound")?;
                    if max_val <= 0 || max_val > u32::MAX as i64 {
                        return Err(InfimumDiagnostic::new(
                            "E017_INVALID_LOOP_BOUND",
                            "parse",
                            format!("Loop bound 'max {max_val}' must be in range 1..={}", u32::MAX),
                            *span,
                            "Provide a positive 32-bit compile-time iteration bound after 'max'",
                        ));
                    }
                    let (has_brk, has_cont) = stmts_have_loop_ctrl(body);
                    let lid = self.loop_counter;
                    self.loop_counter += 1;
                    let brk_var = if has_brk {
                        let v = format!("__ifm_brk_{lid}");
                        out.push(Stmt::Let {
                            name: v.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                        Some(v)
                    } else {
                        None
                    };
                    let cont_var = if has_cont {
                        Some(format!("__ifm_cont_{lid}"))
                    } else {
                        None
                    };

                    let orig_c = self.lower_expr(cond, locals)?;
                    let c = match &brk_var {
                        Some(bv) => Expr::Binary {
                            op: BinOp::And,
                            lhs: Box::new(Expr::Unary {
                                op: UnaryOp::Not,
                                expr: Box::new(Expr::Var(bv.clone())),
                            }),
                            rhs: Box::new(orig_c),
                        },
                        None => orig_c,
                    };

                    locals.push(BTreeSet::new());
                    let mut b = Vec::new();
                    if let Some(cv) = &cont_var {
                        b.push(Stmt::Let {
                            name: cv.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                    }
                    self.loop_stack.push(LoopCtrlCtx {
                        brk_var: brk_var.clone(),
                        cont_var: cont_var.clone(),
                    });
                    b.extend(self.lower_stmts(body, locals)?);
                    self.loop_stack.pop();
                    locals.pop();

                    out.push(Stmt::While {
                        cond: c,
                        max_iters: max_val as u32,
                        body: b,
                    });
                }
                RawStmt::For {
                    var,
                    start,
                    end,
                    inclusive,
                    max_iters,
                    body,
                    span,
                } => {
                    let max_val: i64 = match max_iters {
                        Some(m_expr) => {
                            let mv = self.eval_const_i64(m_expr, *span, "'for ... max' bound")?;
                            if mv <= 0 || mv > u32::MAX as i64 {
                                return Err(InfimumDiagnostic::new(
                                    "E017_INVALID_LOOP_BOUND",
                                    "parse",
                                    format!("Loop bound 'max {mv}' must be in range 1..={}", u32::MAX),
                                    *span,
                                    "Provide a positive 32-bit compile-time iteration bound after 'max'",
                                ));
                            }
                            mv
                        }
                        None => {
                            let s_const = self.eval_const_i64(start, *span, "'for' range start");
                            let e_const = self.eval_const_i64(end, *span, "'for' range end");
                            match (s_const, e_const) {
                                (Ok(sv), Ok(ev)) => {
                                    let diff = ev.saturating_sub(sv) + if *inclusive { 1 } else { 0 };
                                    if diff > u32::MAX as i64 {
                                        return Err(InfimumDiagnostic::new(
                                            "E017_INVALID_LOOP_BOUND",
                                            "parse",
                                            format!("'for' range length ({diff}) exceeds u32::MAX"),
                                            *span,
                                            "Use a smaller static range or an explicit 'max N'",
                                        ));
                                    }
                                    diff.max(1)
                                }
                                _ => {
                                    return Err(InfimumDiagnostic::new(
                                        "E017_INVALID_LOOP_BOUND",
                                        "parse",
                                        "'for' loop with non-constant bounds requires an explicit 'max N'",
                                        *span,
                                        "Add a static upper bound: e.g. 'for i in 0..count max 128 { ... }'",
                                    ));
                                }
                            }
                        }
                    };

                    let lid = self.loop_counter;
                    self.loop_counter += 1;
                    let start_var = format!("__ifm_fstart_{lid}");
                    let end_var = format!("__ifm_fend_{lid}");
                    let start_expr = self.lower_expr(start, locals)?;
                    let end_expr = self.lower_expr(end, locals)?;

                    let (has_brk, has_cont) = stmts_have_loop_ctrl(body);
                    let mut scope_stmts = Vec::new();
                    let brk_var = if has_brk {
                        let v = format!("__ifm_brk_{lid}");
                        scope_stmts.push(Stmt::Let {
                            name: v.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                        Some(v)
                    } else {
                        None
                    };
                    let cont_var = if has_cont {
                        Some(format!("__ifm_cont_{lid}"))
                    } else {
                        None
                    };

                    scope_stmts.push(Stmt::Let {
                        name: start_var.clone(),
                        mutable: false,
                        ty: Some(Type::I64),
                        value: start_expr,
                    });
                    scope_stmts.push(Stmt::Let {
                        name: var.clone(),
                        mutable: true,
                        ty: Some(Type::I64),
                        value: Expr::Var(start_var.clone()),
                    });
                    scope_stmts.push(Stmt::Let {
                        name: end_var.clone(),
                        mutable: false,
                        ty: Some(Type::I64),
                        value: end_expr,
                    });

                    let cmp_op = if *inclusive { BinOp::Le } else { BinOp::Lt };
                    let range_cond = Expr::Binary {
                        op: BinOp::And,
                        lhs: Box::new(Expr::Binary {
                            op: BinOp::Ge,
                            lhs: Box::new(Expr::Var(var.clone())),
                            rhs: Box::new(Expr::Var(start_var)),
                        }),
                        rhs: Box::new(Expr::Binary {
                            op: cmp_op,
                            lhs: Box::new(Expr::Var(var.clone())),
                            rhs: Box::new(Expr::Var(end_var)),
                        }),
                    };
                    let while_cond = match &brk_var {
                        Some(bv) => Expr::Binary {
                            op: BinOp::And,
                            lhs: Box::new(Expr::Unary {
                                op: UnaryOp::Not,
                                expr: Box::new(Expr::Var(bv.clone())),
                            }),
                            rhs: Box::new(range_cond),
                        },
                        None => range_cond,
                    };

                    locals.push(BTreeSet::new());
                    if let Some(top) = locals.last_mut() {
                        top.insert(var.clone());
                    }
                    let mut b = Vec::new();
                    if let Some(cv) = &cont_var {
                        b.push(Stmt::Let {
                            name: cv.clone(),
                            mutable: true,
                            ty: Some(Type::Bool),
                            value: Expr::BoolLit(false),
                        });
                    }
                    self.loop_stack.push(LoopCtrlCtx {
                        brk_var: brk_var.clone(),
                        cont_var: cont_var.clone(),
                    });
                    b.extend(self.lower_stmts(body, locals)?);
                    self.loop_stack.pop();
                    locals.pop();

                    let inc_stmt = Stmt::Assign {
                        name: var.clone(),
                        value: Expr::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(Expr::Var(var.clone())),
                            rhs: Box::new(Expr::IntLit(1)),
                        },
                    };
                    match &brk_var {
                        Some(bv) => {
                            b.push(Stmt::Expr(Expr::If {
                                cond: Box::new(Expr::Unary {
                                    op: UnaryOp::Not,
                                    expr: Box::new(Expr::Var(bv.clone())),
                                }),
                                then_branch: Box::new(Expr::Block {
                                    stmts: vec![inc_stmt],
                                    tail: Box::new(Expr::UnitLit),
                                }),
                                else_branch: Box::new(Expr::UnitLit),
                            }));
                        }
                        None => b.push(inc_stmt),
                    }

                    scope_stmts.push(Stmt::While {
                        cond: while_cond,
                        max_iters: max_val as u32,
                        body: b,
                    });
                    out.push(Stmt::Expr(Expr::Block {
                        stmts: scope_stmts,
                        tail: Box::new(Expr::UnitLit),
                    }));
                }
                RawStmt::Break(span) => {
                    let Some(ctx) = self.loop_stack.last() else {
                        return Err(InfimumDiagnostic::new(
                            "E067_BREAK_OUTSIDE_LOOP",
                            "resolve",
                            "'break' can only be used inside a 'while' or 'for' loop",
                            *span,
                            "Move 'break' inside an enclosing loop body",
                        ));
                    };
                    if let Some(bv) = &ctx.brk_var {
                        out.push(Stmt::Assign {
                            name: bv.clone(),
                            value: Expr::BoolLit(true),
                        });
                    }
                }
                RawStmt::Continue(span) => {
                    let Some(ctx) = self.loop_stack.last() else {
                        return Err(InfimumDiagnostic::new(
                            "E067_CONTINUE_OUTSIDE_LOOP",
                            "resolve",
                            "'continue' can only be used inside a 'while' or 'for' loop",
                            *span,
                            "Move 'continue' inside an enclosing loop body",
                        ));
                    };
                    if let Some(cv) = &ctx.cont_var {
                        out.push(Stmt::Assign {
                            name: cv.clone(),
                            value: Expr::BoolLit(true),
                        });
                    }
                }
                RawStmt::Expr(e) => {
                    out.push(Stmt::Expr(self.lower_expr(e, locals)?));
                }
            }

            // If this statement may trigger break/continue and more statements follow, guard the remainder
            let (sb, sc) = stmt_has_loop_ctrl(s);
            if (sb || sc) && idx + 1 < stmts.len() {
                if let Some(guard) = self.active_loop_guard() {
                    let rest = self.lower_stmts(&stmts[idx + 1..], locals)?;
                    out.push(Stmt::Expr(Expr::If {
                        cond: Box::new(guard),
                        then_branch: Box::new(Expr::Block {
                            stmts: rest,
                            tail: Box::new(Expr::UnitLit),
                        }),
                        else_branch: Box::new(Expr::UnitLit),
                    }));
                    break;
                }
            }
            idx += 1;
        }
        Ok(out)
    }

    pub(super) fn lower_expr(
        &mut self,
        expr: &RawExpr,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        match expr {
            RawExpr::IntLit(v) => Ok(Expr::IntLit(*v)),
            RawExpr::FixedLit(v) => Ok(Expr::FixedLit(*v)),
            RawExpr::BoolLit(b) => Ok(Expr::BoolLit(*b)),
            RawExpr::StrLit(s) => Ok(Expr::StrLit(s.clone())),
            RawExpr::UnitLit => Ok(Expr::UnitLit),
            RawExpr::Var(name) => {
                if !Self::is_shadowed(locals, name) {
                    if let Some(sub) = self.forma_subst.get(name) {
                        return Ok(sub.clone());
                    }
                    if let Some(sym) = self.lookup_const_sym(None, name, Span::zero())? {
                        if let Some(cdef) = self.consts.get(&sym.canonical_name) {
                            return Ok(match cdef.value {
                                ConstValue::Int(v) => Expr::IntLit(v),
                                ConstValue::Fixed(v) => Expr::FixedLit(v),
                                ConstValue::Bool(b) => Expr::BoolLit(b),
                            });
                        }
                    }
                }
                Ok(Expr::Var(name.clone()))
            }
            RawExpr::Path { segs, span } => {
                let v = self.resolve_path_const(segs, *span)?;
                Ok(match v {
                    ConstValue::Int(n) => Expr::IntLit(n),
                    ConstValue::Fixed(n) => Expr::FixedLit(n),
                    ConstValue::Bool(b) => Expr::BoolLit(b),
                })
            }
            RawExpr::LayoutLoad(ll) => {
                let RawLayoutLoad {
                    module,
                    layout,
                    field,
                    index,
                    subfield,
                    span,
                } = ll.as_ref();
                // Case A: Local value-struct or slice property read (`v.x`, `s.len`, `s.ptr`)
                if module.is_none() && index.is_none() && subfield.is_none() {
                    if self.lookup_slice_var(layout).is_some() {
                        return match field.as_str() {
                            "len" => Ok(Expr::SliceLen(Box::new(Expr::Var(layout.clone())))),
                            "ptr" => Ok(Expr::SlicePtr(Box::new(Expr::Var(layout.clone())))),
                            _ => Err(InfimumDiagnostic::new(
                                "E063_UNKNOWN_FIELD",
                                "parse",
                                format!("Slice '{layout}' has no property '.{field}'"),
                                *span,
                                "Supported slice properties are '.len' and '.ptr'",
                            )),
                        };
                    }
                    if let Some(sname) = self.lookup_struct_var(layout) {
                        let sdef = self.structs.get(&sname).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E061_UNKNOWN_STRUCT",
                                "parse",
                                format!("Unknown struct '{sname}'"),
                                *span,
                                "Declare the struct before using it",
                            )
                        })?;
                        let fidx = *sdef.fields.get(field).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E063_UNKNOWN_FIELD",
                                "parse",
                                format!("Struct '{sname}' has no field '{field}'"),
                                *span,
                                format!(
                                    "Available fields on '{sname}': {}",
                                    sdef.field_names.join(", ")
                                ),
                            )
                        })? as usize;
                        return Ok(Expr::TupleField {
                            expr: Box::new(Expr::Var(layout.clone())),
                            index: fidx,
                        });
                    }
                }
                let lcanon = self.resolve_layout_canon(module.as_deref(), layout, *span)?;
                // Case B: Whole-struct load from layout (`let h = World.hero;` or `let s = World.flock[i];`)
                if subfield.is_none() {
                    if let Some(meta) = self
                        .layout_field_meta
                        .get(&(lcanon.clone(), field.clone()))
                        .cloned()
                    {
                        if let Some(sname) = meta.struct_type {
                            let sdef = self.structs.get(&sname).cloned().ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E061_UNKNOWN_STRUCT",
                                    "layout",
                                    format!("Unknown struct '{sname}'"),
                                    *span,
                                    "Declare the struct before using it",
                                )
                            })?;
                            if let Some(idx_raw) = index {
                                let idx_e = self.lower_expr(idx_raw, locals)?;
                                let clamped =
                                    self.inline_clamp_nonneg(idx_e, (meta.count - 1).max(0));
                                let base_expr = Expr::Binary {
                                    op: BinOp::Add,
                                    lhs: Box::new(Expr::IntLit(meta.offset)),
                                    rhs: Box::new(Expr::Binary {
                                        op: BinOp::Mul,
                                        lhs: Box::new(clamped),
                                        rhs: Box::new(Expr::IntLit(meta.stride)),
                                    }),
                                };
                                let bvar = format!("__ifm_sladdr_{}", self.addr_counter);
                                self.addr_counter += 1;
                                let mut loads = Vec::with_capacity(sdef.field_names.len());
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    let f_addr = if k == 0 {
                                        Expr::Var(bvar.clone())
                                    } else {
                                        Expr::Binary {
                                            op: BinOp::Add,
                                            lhs: Box::new(Expr::Var(bvar.clone())),
                                            rhs: Box::new(Expr::IntLit(k as i64)),
                                        }
                                    };
                                    loads.push(Expr::MemLoad {
                                        index: Box::new(f_addr),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    });
                                }
                                return Ok(Expr::Block {
                                    stmts: vec![Stmt::Let {
                                        name: bvar,
                                        mutable: false,
                                        ty: Some(Type::I64),
                                        value: base_expr,
                                    }],
                                    tail: Box::new(Expr::TupleLit(loads)),
                                });
                            } else {
                                let mut loads = Vec::with_capacity(sdef.field_names.len());
                                for (k, fname) in sdef.field_names.iter().enumerate() {
                                    loads.push(Expr::MemLoad {
                                        index: Box::new(Expr::IntLit(meta.offset + k as i64)),
                                        inv: sdef.field_invs.get(fname).copied(),
                                        fixed_ty: sdef.field_fixed.contains(fname),
                                    });
                                }
                                return Ok(Expr::TupleLit(loads));
                            }
                        }
                    }
                }
                let addr_expr = self.lower_layout_addr_expr(
                    &lcanon,
                    field,
                    index.as_deref(),
                    subfield.as_deref(),
                    locals,
                    *span,
                )?;
                let field_inv = self.lookup_layout_inv(&lcanon, field, subfield.as_deref());
                let field_fixed = self.lookup_layout_fixed(&lcanon, field, subfield.as_deref());
                Ok(Expr::MemLoad {
                    index: Box::new(addr_expr),
                    inv: field_inv,
                    fixed_ty: field_fixed,
                })
            }
            RawExpr::Unary { op, expr: inner } => Ok(Expr::Unary {
                op: *op,
                expr: Box::new(self.lower_expr(inner, locals)?),
            }),
            RawExpr::Binary { op, lhs, rhs } => Ok(Expr::Binary {
                op: *op,
                lhs: Box::new(self.lower_expr(lhs, locals)?),
                rhs: Box::new(self.lower_expr(rhs, locals)?),
            }),
            RawExpr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                // Flatten `if ... else if ... else ...` iteratively so long chains never overflow the stack
                let mut branches: Vec<(Expr, Expr)> = Vec::new();
                let mut cur_cond = cond.as_ref();
                let mut cur_then = then_branch.as_ref();
                let mut cur_else = else_branch.as_ref();
                loop {
                    let lc = self.lower_expr(cur_cond, locals)?;
                    let lt = self.lower_expr(cur_then, locals)?;
                    branches.push((lc, lt));
                    if let RawExpr::If {
                        cond: nc,
                        then_branch: nt,
                        else_branch: ne,
                    } = cur_else
                    {
                        cur_cond = nc.as_ref();
                        cur_then = nt.as_ref();
                        cur_else = ne.as_ref();
                    } else {
                        break;
                    }
                }
                let mut acc = self.lower_expr(cur_else, locals)?;
                for (lc, lt) in branches.into_iter().rev() {
                    acc = Expr::If {
                        cond: Box::new(lc),
                        then_branch: Box::new(lt),
                        else_branch: Box::new(acc),
                    };
                }
                Ok(acc)
            }
            RawExpr::Match {
                scrutinee,
                arms,
                span,
            } => self.desugar_match(scrutinee, arms, *span, locals),
            RawExpr::Call { target, args, span } => {
                if let TargetRef::Named(n) = target {
                    if args.len() == 2 && (n == "slice_i64" || n == "slice_fixed") {
                        let ptr_e = self.lower_expr(&args[0], locals)?;
                        let len_e = self.lower_expr(&args[1], locals)?;
                        let elem_ty = if n == "slice_fixed" {
                            Type::Fixed
                        } else {
                            Type::I64
                        };
                        return Ok(Expr::SliceLit {
                            ptr: Box::new(ptr_e),
                            len: Box::new(len_e),
                            elem_ty,
                        });
                    }
                    if args.len() == 1 && n == "str_as_slice" {
                        let s_e = self.lower_expr(&args[0], locals)?;
                        let sp_var = format!("__ifm_strp_{}", self.addr_counter);
                        self.addr_counter += 1;
                        return Ok(Expr::Block {
                            stmts: vec![
                                Stmt::Expr(Expr::Perform {
                                    effect: "Mem".to_string(),
                                    op: "store_str".to_string(),
                                    args: vec![Expr::IntLit(17), s_e],
                                }),
                                Stmt::Let {
                                    name: sp_var.clone(),
                                    mutable: false,
                                    ty: Some(Type::I64),
                                    value: Expr::MemLoad {
                                        index: Box::new(Expr::IntLit(17)),
                                        inv: Some((0, 16_777_216)),
                                        fixed_ty: false,
                                    },
                                },
                            ],
                            tail: Box::new(Expr::SliceLit {
                                ptr: Box::new(Expr::Binary {
                                    op: BinOp::Add,
                                    lhs: Box::new(Expr::Var(sp_var.clone())),
                                    rhs: Box::new(Expr::IntLit(1)),
                                }),
                                len: Box::new(Expr::MemLoad {
                                    index: Box::new(Expr::Var(sp_var)),
                                    inv: Some((0, 16_777_216)),
                                    fixed_ty: false,
                                }),
                                elem_ty: Type::I64,
                            }),
                        });
                    }
                }
                if args.len() == 1 {
                    if let TargetRef::Named(n) = target {
                        let fixed_uop = match n.as_str() {
                            "fixed" => Some(UnaryOp::IntToFixed),
                            "fixed_to_int" => Some(UnaryOp::FixedToInt),
                            "fixed_raw" => Some(UnaryOp::FixedToRaw),
                            "fixed_from_raw" => Some(UnaryOp::RawToFixed),
                            _ => None,
                        };
                        if let Some(op) = fixed_uop {
                            let inner = self.lower_expr(&args[0], locals)?;
                            return Ok(Expr::Unary {
                                op,
                                expr: Box::new(inner),
                            });
                        }
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt")
                    && args.len() == 7
                {
                    if let RawExpr::StrLit(fmt_str) = &args[2] {
                        return self.desugar_ui_fmt(
                            fmt_str,
                            &args[0],
                            &args[1],
                            &args[3],
                            &args[4],
                            &args[5],
                            &args[6],
                            locals,
                        );
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt_centered")
                    && args.len() == 7
                {
                    if let RawExpr::StrLit(fmt_str) = &args[2] {
                        return self.desugar_ui_fmt_centered(
                            fmt_str,
                            &args[0],
                            &args[1],
                            &args[3],
                            &args[4],
                            &args[5],
                            &args[6],
                            locals,
                        );
                    }
                }
                if matches!(target, TargetRef::Named(n) if n == "ui_fmt_width")
                    && args.len() == 3
                {
                    if let RawExpr::StrLit(fmt_str) = &args[0] {
                        return self.desugar_ui_fmt_width(fmt_str, &args[1], &args[2], locals);
                    }
                }
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                let resolved_target = match target {
                    TargetRef::Named(fname) => {
                        if let Some(sym) = self.lookup_fn_sym(None, fname, *span)? {
                            TargetRef::Named(sym.canonical_name.clone())
                        } else if fname.starts_with("$forma_") || fname.starts_with("__forma_") {
                            TargetRef::Named(fname.clone())
                        } else if self.tables.len() > 1 {
                            let cur_file = &self.unit_labels[self.cur_uid];
                            return Err(InfimumDiagnostic::new(
                                "E022_UNKNOWN_FUNCTION",
                                "resolve",
                                format!(
                                    "{cur_file}: Unknown or unimported function '{fname}'"
                                ),
                                *span,
                                format!(
                                    "Declare 'fn {fname}(...)' in '{cur_file}' or import a 'pub fn {fname}' with 'use'"
                                ),
                            ));
                        } else {
                            TargetRef::Named(fname.clone())
                        }
                    }
                    other => other.clone(),
                };
                Ok(Expr::Call {
                    target: resolved_target,
                    args: largs,
                })
            }
            RawExpr::ModCall(mc) => {
                let RawModCall {
                    module,
                    func,
                    args,
                    span,
                } = mc.as_ref();
                let Some(sym) = self.lookup_fn_sym(Some(module), func, *span)? else {
                    if self.lookup_namespace(module).is_none() {
                        return Err(InfimumDiagnostic::new(
                            "E065_UNKNOWN_MOD_FN",
                            "resolve",
                            format!("Unknown module namespace '{module}' in '{module}::{func}'"),
                            *span,
                            format!("Import the module with 'use \"file.ifm\" as {module};'"),
                        ));
                    }
                    return Err(InfimumDiagnostic::new(
                        "E065_UNKNOWN_MOD_FN",
                        "resolve",
                        format!("Unknown function '{module}::{func}'"),
                        *span,
                        format!("Declare 'pub fn {func}(...)' in module '{module}'"),
                    ));
                };
                let target_name = sym.canonical_name.clone();
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                Ok(Expr::Call {
                    target: TargetRef::Named(target_name),
                    args: largs,
                })
            }
            RawExpr::Perform { effect, op, args } => {
                let mut largs = Vec::with_capacity(args.len());
                for a in args {
                    largs.push(self.lower_expr(a, locals)?);
                }
                Ok(Expr::Perform {
                    effect: effect.clone(),
                    op: op.clone(),
                    args: largs,
                })
            }
            RawExpr::MemLoad { index } => Ok(Expr::MemLoad {
                index: Box::new(self.lower_expr(index, locals)?),
                inv: None,
                fixed_ty: false,
            }),
            RawExpr::MemStore { index, value } => Ok(Expr::MemStore {
                index: Box::new(self.lower_expr(index, locals)?),
                value: Box::new(self.lower_expr(value, locals)?),
                inv: None,
                fixed_ty: false,
            }),
            RawExpr::SecretWrap(inner) => {
                Ok(Expr::SecretWrap(Box::new(self.lower_expr(inner, locals)?)))
            }
            RawExpr::Reveal { domain, expr: inner } => Ok(Expr::Reveal {
                domain: domain.clone(),
                expr: Box::new(self.lower_expr(inner, locals)?),
            }),
            RawExpr::Block { stmts, tail } => {
                locals.push(BTreeSet::new());
                self.struct_vars.push(BTreeMap::new());
                self.slice_vars.push(BTreeMap::new());
                let lstmts = self.lower_stmts(stmts, locals)?;
                let ltail = self.lower_expr(tail, locals)?;
                self.slice_vars.pop();
                self.struct_vars.pop();
                locals.pop();
                if lstmts.is_empty() {
                    Ok(ltail)
                } else {
                    Ok(Expr::Block {
                        stmts: lstmts,
                        tail: Box::new(ltail),
                    })
                }
            }
            RawExpr::TupleLit(elems) => {
                let mut out = Vec::with_capacity(elems.len());
                for e in elems {
                    out.push(self.lower_expr(e, locals)?);
                }
                Ok(Expr::TupleLit(out))
            }
            RawExpr::TupleField { expr, index } => Ok(Expr::TupleField {
                expr: Box::new(self.lower_expr(expr, locals)?),
                index: *index,
            }),
            RawExpr::StructLit(sl) => {
                let RawStructLit {
                    module,
                    name,
                    fields,
                    span,
                } = sl.as_ref();
                let scanon = self.resolve_struct_canon(module.as_deref(), name, *span)?;
                let sdef = self.structs.get(&scanon).cloned().ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_STRUCT",
                        "parse",
                        format!("Unknown struct '{name}'"),
                        *span,
                        "Declare the struct with 'struct Name { field: i64, ... }' before constructing it",
                    )
                })?;
                let mut provided: BTreeMap<String, Expr> = BTreeMap::new();
                for (fname, fraw, fspan) in fields {
                    if !sdef.fields.contains_key(fname) {
                        return Err(InfimumDiagnostic::new(
                            "E063_UNKNOWN_FIELD",
                            "parse",
                            format!("Struct '{name}' has no field '{fname}'"),
                            *fspan,
                            format!(
                                "Available fields on '{name}': {}",
                                sdef.field_names.join(", ")
                            ),
                        ));
                    }
                    if provided.contains_key(fname) {
                        return Err(InfimumDiagnostic::new(
                            "E063_DUPLICATE_FIELD",
                            "parse",
                            format!("Duplicate field '{fname}' in struct literal '{name}'"),
                            *fspan,
                            "Specify each struct field at most once",
                        ));
                    }
                    let lowered = self.lower_expr(fraw, locals)?;
                    provided.insert(fname.clone(), lowered);
                }
                let mut missing = Vec::new();
                let mut ordered = Vec::with_capacity(sdef.field_names.len());
                for fname in &sdef.field_names {
                    if let Some(val) = provided.remove(fname) {
                        ordered.push(val);
                    } else {
                        missing.push(fname.clone());
                    }
                }
                if !missing.is_empty() {
                    return Err(InfimumDiagnostic::new(
                        "E063_MISSING_FIELD",
                        "parse",
                        format!(
                            "Struct literal '{name}' is missing field(s): {}",
                            missing.join(", ")
                        ),
                        *span,
                        format!(
                            "Provide all fields of '{name}': {}",
                            sdef.field_names.join(", ")
                        ),
                    ));
                }
                Ok(Expr::TupleLit(ordered))
            }
            RawExpr::FieldAccess { expr, field, span } => {
                if self.infer_raw_expr_slice(expr).is_some() {
                    let lowered = self.lower_expr(expr, locals)?;
                    return match field.as_str() {
                        "len" => Ok(Expr::SliceLen(Box::new(lowered))),
                        "ptr" => Ok(Expr::SlicePtr(Box::new(lowered))),
                        _ => Err(InfimumDiagnostic::new(
                            "E063_UNKNOWN_FIELD",
                            "parse",
                            format!("Slice has no property '.{field}'"),
                            *span,
                            "Supported slice properties are '.len' and '.ptr'",
                        )),
                    };
                }
                let sname = self.infer_raw_expr_struct(expr).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E063_UNKNOWN_FIELD",
                        "parse",
                        format!("Cannot access field '.{field}' on non-struct expression"),
                        *span,
                        "Field access '.name' is supported on struct values (e.g. 'let v = Vec3 { ... }; v.x' or 'add(a, b).x')",
                    )
                })?;
                let sdef = self.structs.get(&sname).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E061_UNKNOWN_STRUCT",
                        "parse",
                        format!("Unknown struct '{sname}'"),
                        *span,
                        "Declare the struct before using it",
                    )
                })?;
                let fidx = *sdef.fields.get(field).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E063_UNKNOWN_FIELD",
                        "parse",
                        format!("Struct '{sname}' has no field '{field}'"),
                        *span,
                        format!(
                            "Available fields on '{sname}': {}",
                            sdef.field_names.join(", ")
                        ),
                    )
                })? as usize;
                let lowered = self.lower_expr(expr, locals)?;
                Ok(Expr::TupleField {
                    expr: Box::new(lowered),
                    index: fidx,
                })
            }
            RawExpr::SliceIndex(si) => {
                let RawSliceIndex {
                    base,
                    index,
                    subfield,
                    span,
                } = si.as_ref();
                let smeta = self.infer_raw_expr_slice(base).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E060_TYPE_MISMATCH",
                        "parse",
                        "Cannot index into non-slice expression",
                        *span,
                        "Indexing 's[i]' requires a slice '[T]' or a layout array 'Layout.arr[i]'",
                    )
                })?;
                let base_e = self.lower_expr(base, locals)?;
                let idx_e = self.lower_expr(index, locals)?;
                let sb_var = format!("__ifm_sb_{}", self.addr_counter);
                self.addr_counter += 1;
                let si_var = format!("__ifm_si_{}", self.addr_counter);
                self.addr_counter += 1;
                let pre_stmts = vec![
                    Stmt::Let {
                        name: sb_var.clone(),
                        mutable: false,
                        ty: Some(Type::Slice(Box::new(smeta.elem_ty.clone()))),
                        value: base_e,
                    },
                    Stmt::Let {
                        name: si_var.clone(),
                        mutable: false,
                        ty: Some(Type::I64),
                        value: Expr::BoundsCheck {
                            index: Box::new(idx_e),
                            len: Box::new(Expr::SliceLen(Box::new(Expr::Var(sb_var.clone())))),
                            inclusive_hi: false,
                        },
                    },
                ];
                let elem_offset = if smeta.stride == 1 {
                    Expr::Var(si_var)
                } else {
                    Expr::Binary {
                        op: BinOp::Mul,
                        lhs: Box::new(Expr::Var(si_var)),
                        rhs: Box::new(Expr::IntLit(smeta.stride)),
                    }
                };
                let elem_addr = Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(Expr::SlicePtr(Box::new(Expr::Var(sb_var)))),
                    rhs: Box::new(elem_offset),
                };
                let tail = if let Some(sf) = subfield {
                    let scanon = smeta.struct_canon.as_ref().ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E063_UNKNOWN_FIELD",
                            "parse",
                            format!("Cannot access subfield '.{sf}' on scalar slice element"),
                            *span,
                            "Use 's[i]' directly for scalar slices '[i64]' / '[fixed]'",
                        )
                    })?;
                    let sdef = self.structs.get(scanon).cloned().ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E061_UNKNOWN_STRUCT",
                            "parse",
                            format!("Unknown struct '{scanon}'"),
                            *span,
                            "Declare the struct before using it",
                        )
                    })?;
                    let foff = *sdef.fields.get(sf).ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E063_UNKNOWN_FIELD",
                            "parse",
                            format!("Struct '{scanon}' has no field '{sf}'"),
                            *span,
                            format!("Available fields: {}", sdef.field_names.join(", ")),
                        )
                    })?;
                    let finv = sdef.field_invs.get(sf).copied();
                    let ffixed = sdef.field_fixed.contains(sf);
                    let faddr = if foff == 0 {
                        elem_addr
                    } else {
                        Expr::Binary {
                            op: BinOp::Add,
                            lhs: Box::new(elem_addr),
                            rhs: Box::new(Expr::IntLit(foff)),
                        }
                    };
                    Expr::MemLoad {
                        index: Box::new(faddr),
                        inv: finv,
                        fixed_ty: ffixed,
                    }
                } else if let Some(ref scanon) = smeta.struct_canon {
                    let sdef = self.structs.get(scanon).cloned().ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E061_UNKNOWN_STRUCT",
                            "parse",
                            format!("Unknown struct '{scanon}'"),
                            *span,
                            "Declare the struct before using it",
                        )
                    })?;
                    self.emit_load_struct_at(elem_addr, &sdef)
                } else {
                    Expr::MemLoad {
                        index: Box::new(elem_addr),
                        inv: None,
                        fixed_ty: smeta.is_fixed,
                    }
                };
                Ok(Expr::Block {
                    stmts: pre_stmts,
                    tail: Box::new(tail),
                })
            }
            RawExpr::SliceRange(sr) => {
                let RawSliceRange { base, lo, hi, span } = sr.as_ref();
                // Case 1: Slicing a layout array (`[m::]Layout.arr[lo..hi]` or `Layout.arr[..]`)
                let mut pre_stmts: Vec<Stmt> = Vec::new();
                let (base_ptr, base_len, smeta) = if let RawExpr::LayoutLoad(ll) = base {
                    if ll.index.is_none() && ll.subfield.is_none() {
                        if let Some(sym) = self.lookup_layout_sym(
                            ll.module.as_deref(),
                            &ll.layout,
                            ll.span,
                        )? {
                            if let Some(meta) = self
                                .layout_field_meta
                                .get(&(sym.canonical_name.clone(), ll.field.clone()))
                                .cloned()
                            {
                                if meta.is_array {
                                    let smeta =
                                        self.slice_meta_from_layout_field(&meta).ok_or_else(
                                            || {
                                                InfimumDiagnostic::new(
                                                    "E061_UNKNOWN_STRUCT",
                                                    "parse",
                                                    format!("Unknown element struct on '{}.{}'", ll.layout, ll.field),
                                                    *span,
                                                    "Declare the struct before slicing its layout array",
                                                )
                                            },
                                        )?;
                                    (Expr::IntLit(meta.offset), Expr::IntLit(meta.count), smeta)
                                } else {
                                    return Err(InfimumDiagnostic::new(
                                        "E061_LAYOUT_ARRAY_ACCESS",
                                        "parse",
                                        format!("Layout field '{}.{}' is not an array and cannot be sliced", ll.layout, ll.field),
                                        *span,
                                        "Slicing '[lo..hi]' / '[..]' is supported on layout arrays (words[N], fixed[N], Struct[N]) and slices",
                                    ));
                                }
                            } else {
                                return Err(InfimumDiagnostic::new(
                                    "E061_UNKNOWN_LAYOUT_FIELD",
                                    "parse",
                                    format!("Unknown field '{}' on layout '{}'", ll.field, ll.layout),
                                    *span,
                                    "Check the field name in the layout declaration",
                                ));
                            }
                        } else {
                            // Not a layout: fall through to slice expression
                            let smeta = self.infer_raw_expr_slice(base).ok_or_else(|| {
                                InfimumDiagnostic::new(
                                    "E060_TYPE_MISMATCH",
                                    "parse",
                                    "Cannot slice non-slice expression",
                                    *span,
                                    "Slicing '[lo..hi]' requires a layout array or slice '[T]'",
                                )
                            })?;
                            let base_e = self.lower_expr(base, locals)?;
                            let sb_var = format!("__ifm_sb_{}", self.addr_counter);
                            self.addr_counter += 1;
                            pre_stmts.push(Stmt::Let {
                                name: sb_var.clone(),
                                mutable: false,
                                ty: Some(Type::Slice(Box::new(smeta.elem_ty.clone()))),
                                value: base_e,
                            });
                            (
                                Expr::SlicePtr(Box::new(Expr::Var(sb_var.clone()))),
                                Expr::SliceLen(Box::new(Expr::Var(sb_var))),
                                smeta,
                            )
                        }
                    } else {
                        return Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "parse",
                            "Cannot slice an already-indexed layout element",
                            *span,
                            "Slice the array directly: e.g. 'Layout.arr[lo..hi]'",
                        ));
                    }
                } else {
                    let smeta = self.infer_raw_expr_slice(base).ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "parse",
                            "Cannot slice non-slice expression",
                            *span,
                            "Slicing '[lo..hi]' requires a layout array or slice '[T]'",
                        )
                    })?;
                    let base_e = self.lower_expr(base, locals)?;
                    let sb_var = format!("__ifm_sb_{}", self.addr_counter);
                    self.addr_counter += 1;
                    pre_stmts.push(Stmt::Let {
                        name: sb_var.clone(),
                        mutable: false,
                        ty: Some(Type::Slice(Box::new(smeta.elem_ty.clone()))),
                        value: base_e,
                    });
                    (
                        Expr::SlicePtr(Box::new(Expr::Var(sb_var.clone()))),
                        Expr::SliceLen(Box::new(Expr::Var(sb_var))),
                        smeta,
                    )
                };

                if lo.is_none() && hi.is_none() {
                    let slit = Expr::SliceLit {
                        ptr: Box::new(base_ptr),
                        len: Box::new(base_len),
                        elem_ty: smeta.elem_ty,
                    };
                    if pre_stmts.is_empty() {
                        return Ok(slit);
                    }
                    return Ok(Expr::Block {
                        stmts: pre_stmts,
                        tail: Box::new(slit),
                    });
                }

                let lo_e = match lo {
                    Some(e) => self.lower_expr(e, locals)?,
                    None => Expr::IntLit(0),
                };
                let hi_e = match hi {
                    Some(e) => self.lower_expr(e, locals)?,
                    None => base_len.clone(),
                };
                let lo_var = format!("__ifm_slo_{}", self.addr_counter);
                self.addr_counter += 1;
                let hi_var = format!("__ifm_shi_{}", self.addr_counter);
                self.addr_counter += 1;
                let chk_var = format!("__ifm_schk_{}", self.addr_counter);
                self.addr_counter += 1;
                pre_stmts.push(Stmt::Let {
                    name: lo_var.clone(),
                    mutable: false,
                    ty: Some(Type::I64),
                    value: lo_e,
                });
                pre_stmts.push(Stmt::Let {
                    name: hi_var.clone(),
                    mutable: false,
                    ty: Some(Type::I64),
                    value: hi_e,
                });
                pre_stmts.push(Stmt::Let {
                    name: chk_var.clone(),
                    mutable: false,
                    ty: Some(Type::I64),
                    value: Expr::RangeCheck {
                        lo: Box::new(Expr::Var(lo_var.clone())),
                        hi: Box::new(Expr::Var(hi_var.clone())),
                        len: Box::new(base_len),
                    },
                });
                let ptr_off = if smeta.stride == 1 {
                    Expr::Var(chk_var)
                } else {
                    Expr::Binary {
                        op: BinOp::Mul,
                        lhs: Box::new(Expr::Var(chk_var)),
                        rhs: Box::new(Expr::IntLit(smeta.stride)),
                    }
                };
                let new_ptr = Expr::Binary {
                    op: BinOp::Add,
                    lhs: Box::new(base_ptr),
                    rhs: Box::new(ptr_off),
                };
                let new_len = Expr::Binary {
                    op: BinOp::Sub,
                    lhs: Box::new(Expr::Var(hi_var)),
                    rhs: Box::new(Expr::Var(lo_var)),
                };
                Ok(Expr::Block {
                    stmts: pre_stmts,
                    tail: Box::new(Expr::SliceLit {
                        ptr: Box::new(new_ptr),
                        len: Box::new(new_len),
                        elem_ty: smeta.elem_ty,
                    }),
                })
            }
        }
    }

    fn desugar_match(
        &mut self,
        scrutinee: &RawExpr,
        arms: &[RawMatchArm],
        span: Span,
        locals: &mut Vec<BTreeSet<String>>,
    ) -> Result<Expr, InfimumDiagnostic> {
        if arms.is_empty() {
            return Err(InfimumDiagnostic::new(
                "E063_NON_EXHAUSTIVE_MATCH",
                "resolve",
                "Empty 'match' expression is not exhaustive",
                span,
                "Add at least one arm ending with a wildcard '_ => ...' or covering all enum variants",
            ));
        }

        enum ResolvedPat {
            Wildcard,
            Int(i64),
            Bool(bool),
            Enum {
                enum_name: String,
                variant: String,
                value: i64,
            },
        }

        let mut resolved_arms: Vec<(Vec<ResolvedPat>, Expr)> = Vec::with_capacity(arms.len());
        let mut wildcard_arm_idx: Option<usize> = None;

        for (arm_idx, arm) in arms.iter().enumerate() {
            let mut rpats = Vec::with_capacity(arm.patterns.len());
            for pat in &arm.patterns {
                let rp = match pat {
                    RawMatchPattern::Wildcard(pspan) => {
                        if arm_idx + 1 != arms.len() {
                            return Err(InfimumDiagnostic::new(
                                "E063_UNREACHABLE_MATCH_ARM",
                                "resolve",
                                "Wildcard '_' arm must be the final arm of a 'match' expression",
                                *pspan,
                                "Move the '_ => ...' fallback arm to the end of the match block",
                            ));
                        }
                        wildcard_arm_idx = Some(arm_idx);
                        ResolvedPat::Wildcard
                    }
                    RawMatchPattern::Int(v) => ResolvedPat::Int(*v),
                    RawMatchPattern::Bool(b) => ResolvedPat::Bool(*b),
                    RawMatchPattern::ConstRef(name, pspan) => {
                        let sym = self.lookup_const_sym(None, name, *pspan)?;
                        let cdef = sym.and_then(|s| self.consts.get(&s.canonical_name)).ok_or_else(|| {
                            InfimumDiagnostic::new(
                                "E064_CONST_EVAL",
                                "resolve",
                                format!("Unknown constant '{name}' in match pattern"),
                                *pspan,
                                "Use a declared 'const', literal, Enum::Variant, or '_' wildcard in match patterns",
                            )
                        })?;
                        match cdef.value {
                            ConstValue::Int(v) | ConstValue::Fixed(v) => ResolvedPat::Int(v),
                            ConstValue::Bool(b) => ResolvedPat::Bool(b),
                        }
                    }
                    RawMatchPattern::Path(segs, pspan) => {
                        let enum_hit: Option<(String, String, i64)> = match segs.as_slice() {
                            [head, member] => {
                                if let Some(sym) = self.lookup_enum_sym(None, head, *pspan)? {
                                    if let Some(edef) = self.enums.get(&sym.canonical_name) {
                                        let v = edef.variants.iter().find(|v| v.name == *member).ok_or_else(|| {
                                            InfimumDiagnostic::new(
                                                "E061_UNKNOWN_ENUM_VARIANT",
                                                "resolve",
                                                format!("Enum '{head}' has no variant '{member}'"),
                                                *pspan,
                                                format!("Check the variants declared in 'enum {head}'"),
                                            )
                                        })?;
                                        Some((sym.canonical_name.clone(), member.clone(), v.value))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            [m, ename, member] if self.lookup_namespace(m).is_some() => {
                                if let Some(sym) = self.lookup_enum_sym(Some(m), ename, *pspan)? {
                                    if let Some(edef) = self.enums.get(&sym.canonical_name) {
                                        let v = edef.variants.iter().find(|v| v.name == *member).ok_or_else(|| {
                                            InfimumDiagnostic::new(
                                                "E061_UNKNOWN_ENUM_VARIANT",
                                                "resolve",
                                                format!("Enum '{m}::{ename}' has no variant '{member}'"),
                                                *pspan,
                                                format!("Check the variants declared in 'enum {ename}'"),
                                            )
                                        })?;
                                        Some((sym.canonical_name.clone(), member.clone(), v.value))
                                    } else {
                                        None
                                    }
                                } else {
                                    None
                                }
                            }
                            _ => None,
                        };
                        if let Some((enum_name, variant, value)) = enum_hit {
                            ResolvedPat::Enum { enum_name, variant, value }
                        } else {
                            match self.resolve_path_const(segs, *pspan)? {
                                ConstValue::Int(v) | ConstValue::Fixed(v) => ResolvedPat::Int(v),
                                ConstValue::Bool(b) => ResolvedPat::Bool(b),
                            }
                        }
                    }
                };
                rpats.push(rp);
            }
            let body = self.lower_expr(&arm.body, locals)?;
            resolved_arms.push((rpats, body));
        }

        // Verify exhaustiveness when there is no wildcard arm
        if wildcard_arm_idx.is_none() {
            let mut all_bool = true;
            let mut seen_true = false;
            let mut seen_false = false;
            let mut single_enum: Option<String> = None;
            let mut all_same_enum = true;
            let mut covered_variants: BTreeSet<String> = BTreeSet::new();

            for (pats, _) in &resolved_arms {
                for p in pats {
                    match p {
                        ResolvedPat::Bool(true) => {
                            seen_true = true;
                            all_same_enum = false;
                        }
                        ResolvedPat::Bool(false) => {
                            seen_false = true;
                            all_same_enum = false;
                        }
                        ResolvedPat::Enum {
                            enum_name, variant, ..
                        } => {
                            all_bool = false;
                            match &single_enum {
                                None => single_enum = Some(enum_name.clone()),
                                Some(prev) if prev != enum_name => all_same_enum = false,
                                Some(_) => {}
                            }
                            covered_variants.insert(variant.clone());
                        }
                        ResolvedPat::Int(_) | ResolvedPat::Wildcard => {
                            all_bool = false;
                            all_same_enum = false;
                        }
                    }
                }
            }

            let exhaustive = if all_bool && seen_true && seen_false {
                true
            } else if all_same_enum {
                if let Some(ref ename) = single_enum {
                    if let Some(edef) = self.enums.get(ename) {
                        let missing: Vec<String> = edef
                            .variants
                            .iter()
                            .filter(|v| !covered_variants.contains(&v.name))
                            .map(|v| format!("{ename}::{}", v.name))
                            .collect();
                        if !missing.is_empty() {
                            return Err(InfimumDiagnostic::new(
                                "E063_NON_EXHAUSTIVE_MATCH",
                                "resolve",
                                format!(
                                    "Non-exhaustive 'match' on enum '{ename}': missing variant(s) {}",
                                    missing.join(", ")
                                ),
                                span,
                                "Cover all enum variants or add a wildcard fallback arm '_ => ...'",
                            ));
                        }
                        true
                    } else {
                        false
                    }
                } else {
                    false
                }
            } else {
                false
            };

            if !exhaustive {
                return Err(InfimumDiagnostic::new(
                    "E063_NON_EXHAUSTIVE_MATCH",
                    "resolve",
                    "Non-exhaustive 'match' expression: missing wildcard fallback '_ => ...'",
                    span,
                    "Add a wildcard fallback arm '_ => ...' (or cover all variants of a single enum / both bool values)",
                ));
            }
        }

        let lowered_scrut = self.lower_expr(scrutinee, locals)?;
        let scrut_var = format!("$match_{}", self.match_counter);
        self.match_counter += 1;

        // Build nested if/else chain from the last arm backwards
        let (_, last_body) = resolved_arms.pop().unwrap();
        let mut acc = last_body;

        for (pats, arm_body) in resolved_arms.into_iter().rev() {
            let mut cond_opt: Option<Expr> = None;
            for p in pats {
                let lit = match p {
                    ResolvedPat::Int(v) | ResolvedPat::Enum { value: v, .. } => Expr::IntLit(v),
                    ResolvedPat::Bool(b) => Expr::BoolLit(b),
                    ResolvedPat::Wildcard => continue,
                };
                let eq = Expr::Binary {
                    op: BinOp::Eq,
                    lhs: Box::new(Expr::Var(scrut_var.clone())),
                    rhs: Box::new(lit),
                };
                cond_opt = Some(match cond_opt {
                    None => eq,
                    Some(prev) => Expr::Binary {
                        op: BinOp::Or,
                        lhs: Box::new(prev),
                        rhs: Box::new(eq),
                    },
                });
            }
            if let Some(cond) = cond_opt {
                acc = Expr::If {
                    cond: Box::new(cond),
                    then_branch: Box::new(arm_body),
                    else_branch: Box::new(acc),
                };
            }
        }

        Ok(Expr::Block {
            stmts: vec![Stmt::Let {
                name: scrut_var,
                mutable: false,
                ty: None,
                value: lowered_scrut,
            }],
            tail: Box::new(acc),
        })
    }
}

pub(super) fn const_overflow(op: &str, span: Span) -> InfimumDiagnostic {
    InfimumDiagnostic::new(
        "E064_CONST_EVAL",
        "const-eval",
        format!("Signed 64-bit overflow evaluating '{op}' in constant expression"),
        span,
        "Keep constant arithmetic within signed 64-bit integer bounds",
    )
}


pub(super) fn rewrite_forma_slot_callbacks<E>(
    node: &mut crate::forma::FormaNode<E>,
    resolver: &ModuleResolver,
) -> Result<(), InfimumDiagnostic> {
    let resolve_cb = |raw: &str, span: Span| -> Result<String, InfimumDiagnostic> {
        if let Some((m, f)) = raw.split_once("::") {
            let Some(sym) = resolver.lookup_fn_sym(Some(m), f, span)? else {
                return Err(InfimumDiagnostic::new(
                    "E065_UNKNOWN_MOD_FN",
                    "resolve",
                    format!("Unknown callback function '{raw}' in Forma Slot"),
                    span,
                    format!("Ensure module '{m}' declares 'pub fn {f}'"),
                ));
            };
            Ok(sym.canonical_name.clone())
        } else if let Some(sym) = resolver.lookup_fn_sym(None, raw, span)? {
            Ok(sym.canonical_name.clone())
        } else if raw.starts_with("$forma_") || raw.starts_with("__forma_") {
            Ok(raw.to_string())
        } else if resolver.tables.len() > 1 {
            let cur_file = &resolver.unit_labels[resolver.cur_uid];
            Err(InfimumDiagnostic::new(
                "E022_UNKNOWN_FUNCTION",
                "resolve",
                format!("{cur_file}: Unknown or unimported Slot callback '{raw}'"),
                span,
                format!("Declare 'fn {raw}(...)' in '{cur_file}' or import a 'pub fn {raw}' with 'use'"),
            ))
        } else {
            Ok(raw.to_string())
        }
    };
    if let Some(ref mut rfn) = node.props.render {
        *rfn = resolve_cb(rfn, node.span)?;
    }
    if let Some(ref mut wfn) = node.props.on_wheel {
        *wfn = resolve_cb(wfn, node.span)?;
    }
    for ch in &mut node.children {
        rewrite_forma_slot_callbacks(ch, resolver)?;
    }
    if let Some((_, ref mut else_nodes)) = node.forma_if {
        for ch in else_nodes {
            rewrite_forma_slot_callbacks(ch, resolver)?;
        }
    }
    Ok(())
}


pub(super) fn infer_and_rewrite_fixed_expr(
    expr: &mut Expr,
    env: &mut BTreeMap<String, Type>,
    fn_rets: &BTreeMap<String, Type>,
) -> Type {
    match expr {
        Expr::IntLit(_) => Type::I64,
        Expr::FixedLit(_) => Type::Fixed,
        Expr::BoolLit(_) => Type::Bool,
        Expr::StrLit(_) => Type::Str,
        Expr::UnitLit => Type::Unit,
        Expr::Var(n) => env.get(n).cloned().unwrap_or(Type::I64),
        Expr::MemLoad { index, fixed_ty, .. } => {
            infer_and_rewrite_fixed_expr(index, env, fn_rets);
            if *fixed_ty {
                Type::Fixed
            } else {
                Type::I64
            }
        }
        Expr::MemStore { index, value, .. } => {
            infer_and_rewrite_fixed_expr(index, env, fn_rets);
            infer_and_rewrite_fixed_expr(value, env, fn_rets);
            Type::Unit
        }
        Expr::Unary { op, expr: inner } => {
            let t = infer_and_rewrite_fixed_expr(inner, env, fn_rets);
            match op {
                UnaryOp::Neg => t,
                UnaryOp::Not => Type::Bool,
                UnaryOp::IntToFixed | UnaryOp::RawToFixed => Type::Fixed,
                UnaryOp::FixedToInt | UnaryOp::FixedToRaw => Type::I64,
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = infer_and_rewrite_fixed_expr(lhs, env, fn_rets);
            let rt = infer_and_rewrite_fixed_expr(rhs, env, fn_rets);
            if lt == Type::Fixed && rt == Type::Fixed {
                if *op == BinOp::Mul {
                    *op = BinOp::FixedMul;
                } else if *op == BinOp::Div {
                    *op = BinOp::FixedDiv;
                }
            }
            match op {
                BinOp::Eq | BinOp::Ne | BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge | BinOp::And | BinOp::Or => Type::Bool,
                BinOp::FixedMul | BinOp::FixedDiv => Type::Fixed,
                BinOp::Add | BinOp::Sub | BinOp::Mul | BinOp::Div => {
                    if lt == Type::Fixed || rt == Type::Fixed {
                        Type::Fixed
                    } else {
                        Type::I64
                    }
                }
                _ => Type::I64,
            }
        }
        Expr::If { cond, then_branch, else_branch } => {
            infer_and_rewrite_fixed_expr(cond, env, fn_rets);
            let tt = infer_and_rewrite_fixed_expr(then_branch, env, fn_rets);
            let _ = infer_and_rewrite_fixed_expr(else_branch, env, fn_rets);
            tt
        }
        Expr::Call { target, args } => {
            let mut arg_types = Vec::with_capacity(args.len());
            for a in args.iter_mut() {
                arg_types.push(infer_and_rewrite_fixed_expr(a, env, fn_rets));
            }
            if let TargetRef::Named(n) = target {
                if (n == "ui_int" || n.ends_with("__ui_int"))
                    && args.len() == 7
                    && arg_types.get(2) == Some(&Type::Fixed)
                {
                    let rewritten = format!("{}ui_fixed", &n[..n.len() - "ui_int".len()]);
                    *target = TargetRef::Named(rewritten);
                    args.insert(3, Expr::IntLit(2));
                } else if (n == "ui_int_width" || n.ends_with("__ui_int_width"))
                    && args.len() == 3
                    && arg_types.first() == Some(&Type::Fixed)
                {
                    let rewritten = format!("{}ui_fixed_width", &n[..n.len() - "ui_int_width".len()]);
                    *target = TargetRef::Named(rewritten);
                    args.insert(1, Expr::IntLit(2));
                }
            }
            match target {
                TargetRef::Named(n) => fn_rets.get(n).cloned().unwrap_or(Type::I64),
                _ => Type::I64,
            }
        }
        Expr::Perform { args, .. } => {
            for a in args {
                infer_and_rewrite_fixed_expr(a, env, fn_rets);
            }
            Type::Unit
        }
        Expr::SecretWrap(inner) => Type::Secret(Box::new(infer_and_rewrite_fixed_expr(inner, env, fn_rets))),
        Expr::Reveal { expr: inner, .. } => match infer_and_rewrite_fixed_expr(inner, env, fn_rets) {
            Type::Secret(t) => *t,
            other => other,
        },
        Expr::TupleLit(elems) => Type::Tuple(
            elems
                .iter_mut()
                .map(|e| infer_and_rewrite_fixed_expr(e, env, fn_rets))
                .collect(),
        ),
        Expr::TupleField { expr: inner, index } => match infer_and_rewrite_fixed_expr(inner, env, fn_rets) {
            Type::Tuple(ts) => ts.get(*index).cloned().unwrap_or(Type::I64),
            _ => Type::I64,
        },
        Expr::SliceLit { ptr, len, elem_ty } => {
            infer_and_rewrite_fixed_expr(ptr, env, fn_rets);
            infer_and_rewrite_fixed_expr(len, env, fn_rets);
            Type::Slice(Box::new(elem_ty.clone()))
        }
        Expr::SlicePtr(inner) | Expr::SliceLen(inner) => {
            infer_and_rewrite_fixed_expr(inner, env, fn_rets);
            Type::I64
        }
        Expr::BoundsCheck { index, len, .. } => {
            infer_and_rewrite_fixed_expr(index, env, fn_rets);
            infer_and_rewrite_fixed_expr(len, env, fn_rets);
            Type::I64
        }
        Expr::RangeCheck { lo, hi, len } => {
            infer_and_rewrite_fixed_expr(lo, env, fn_rets);
            infer_and_rewrite_fixed_expr(hi, env, fn_rets);
            infer_and_rewrite_fixed_expr(len, env, fn_rets);
            Type::I64
        }
        Expr::Block { stmts, tail } => {
            let mut inner_env = env.clone();
            infer_and_rewrite_fixed_stmts(stmts, &mut inner_env, fn_rets);
            infer_and_rewrite_fixed_expr(tail, &mut inner_env, fn_rets)
        }
    }
}

pub(super) fn infer_and_rewrite_fixed_stmts(
    stmts: &mut [Stmt],
    env: &mut BTreeMap<String, Type>,
    fn_rets: &BTreeMap<String, Type>,
) {
    for s in stmts {
        match s {
            Stmt::Let { name, ty, value, .. } => {
                let vt = infer_and_rewrite_fixed_expr(value, env, fn_rets);
                env.insert(name.clone(), ty.clone().unwrap_or(vt));
            }
            Stmt::LetTuple { names, value } => {
                let vt = infer_and_rewrite_fixed_expr(value, env, fn_rets);
                if let Type::Tuple(ts) = vt {
                    for (i, (n, _)) in names.iter().enumerate() {
                        env.insert(n.clone(), ts.get(i).cloned().unwrap_or(Type::I64));
                    }
                } else {
                    for (n, _) in names {
                        env.insert(n.clone(), Type::I64);
                    }
                }
            }
            Stmt::Assign { value, .. } | Stmt::Expr(value) => {
                infer_and_rewrite_fixed_expr(value, env, fn_rets);
            }
            Stmt::While { cond, body, .. } => {
                infer_and_rewrite_fixed_expr(cond, env, fn_rets);
                let mut loop_env = env.clone();
                infer_and_rewrite_fixed_stmts(body, &mut loop_env, fn_rets);
            }
        }
    }
}

pub(super) fn resolve_fixed_ops_in_fns(functions: &mut [FnDef]) {
    let mut fn_rets = BTreeMap::new();
    for f in functions.iter() {
        fn_rets.insert(f.name.clone(), f.ret_type.clone());
    }
    for f in functions.iter_mut() {
        let mut env = BTreeMap::new();
        for p in &f.params {
            env.insert(p.name.clone(), p.ty.clone());
        }
        for r in &mut f.requires {
            infer_and_rewrite_fixed_expr(r, &mut env, &fn_rets);
        }
        let mut ens_env = env.clone();
        ens_env.insert("result".to_string(), f.ret_type.clone());
        for e in &mut f.ensures {
            infer_and_rewrite_fixed_expr(e, &mut ens_env, &fn_rets);
        }
        infer_and_rewrite_fixed_expr(&mut f.body, &mut env, &fn_rets);
    }
}

fn attach_forma_visible_expr(
    mut node: crate::forma::FormaNode<Expr>,
    cond: Expr,
) -> crate::forma::FormaNode<Expr> {
    node.props.visible = match node.props.visible {
        Some(prev) => Some(Expr::Binary {
            op: BinOp::And,
            lhs: Box::new(cond),
            rhs: Box::new(prev),
        }),
        None => Some(cond),
    };
    node
}

pub(super) fn expand_and_lower_forma_node(
    mut node: crate::forma::FormaNode<RawExpr>,
    defs: &BTreeMap<String, (UnitId, crate::forma::FormaDef<RawExpr>)>,
    resolver: &mut ModuleResolver,
    visiting: &mut Vec<String>,
    param_locals: &mut Vec<BTreeSet<String>>,
) -> Result<Vec<crate::forma::FormaNode<Expr>>, InfimumDiagnostic> {
    // 1. Conditional UI: `if cond { then... } [else { else... }]`
    if let Some((cond_raw, else_nodes)) = node.forma_if.take() {
        let cond_expr = resolver.lower_expr(&cond_raw, param_locals)?;
        let mut out = Vec::new();
        for ch in node.children {
            for exp in expand_and_lower_forma_node(ch, defs, resolver, visiting, param_locals)? {
                out.push(attach_forma_visible_expr(exp, cond_expr.clone()));
            }
        }
        if !else_nodes.is_empty() {
            let neg_cond = Expr::Unary {
                op: UnaryOp::Not,
                expr: Box::new(cond_expr),
            };
            for ch in else_nodes {
                for exp in
                    expand_and_lower_forma_node(ch, defs, resolver, visiting, param_locals)?
                {
                    out.push(attach_forma_visible_expr(exp, neg_cond.clone()));
                }
            }
        }
        return Ok(out);
    }

    // 2. Static unrolled loop: `for i in start..end { body... }`
    if let Some((var, start_expr, end_expr, inclusive)) = node.forma_for.take() {
        let sv = resolver.eval_const_i64(&start_expr, node.span, "forma 'for' start")?;
        let ev = resolver.eval_const_i64(&end_expr, node.span, "forma 'for' end")?;
        let hi = if inclusive { ev } else { ev.saturating_sub(1) };
        let count = if hi >= sv { hi - sv + 1 } else { 0 };
        if count > 256 {
            return Err(InfimumDiagnostic::new(
                "E094_FORMA_FOR_TOO_LARGE",
                "forma-expand",
                format!("Forma 'for' loop unrolls to {count} iterations (max 256)"),
                node.span,
                "Use a smaller static range (<= 256) or a custom Slot(render: ...) for large dynamic lists",
            ));
        }
        let mut out = Vec::new();
        for k in sv..=hi {
            let prev_sub = resolver.forma_subst.insert(var.clone(), Expr::IntLit(k));
            for ch in &node.children {
                out.extend(expand_and_lower_forma_node(
                    ch.clone(),
                    defs,
                    resolver,
                    visiting,
                    param_locals,
                )?);
            }
            if let Some(old) = prev_sub {
                resolver.forma_subst.insert(var.clone(), old);
            } else {
                resolver.forma_subst.remove(&var);
            }
        }
        return Ok(out);
    }

    // 3. Nested custom `forma` component call: `ScoreCard("Вы", score)` or `m::ScoreCard(...)`
    //    Lexical hygiene: lower arguments in CALLER's unit scope, then switch to TARGET's unit
    //    scope to expand & lower the component body with pre-lowered argument substitutions.
    if let Some((comp_name, args)) = node.component_call.take() {
        let resolved_sym = if let Some((m, fname)) = comp_name.split_once("::") {
            resolver.lookup_forma_sym(Some(m), fname, node.span)?
        } else {
            resolver.lookup_forma_sym(None, &comp_name, node.span)?
        };
        let target_entry = resolved_sym
            .and_then(|s| defs.get(&s.canonical_name).map(|e| (s.canonical_name.clone(), e)));
        let Some((target_canon, (target_uid, target_def))) = target_entry else {
            return Err(InfimumDiagnostic::new(
                "E091_FORMA_SYNTAX",
                "forma-expand",
                format!("Unknown Forma node or component '{comp_name}'"),
                node.span,
                "Use a built-in node (Box, Col, Row, Stack, Scroll, Overlay, Button, Dropdown, Spacer, Divider, Rect, Slot, Text) or declare 'forma Name(...)'",
            ));
        };
        if visiting.iter().any(|v| v == &target_canon) {
            return Err(InfimumDiagnostic::new(
                "E095_FORMA_COMPONENT_CYCLE",
                "forma-expand",
                format!("Recursive Forma component cycle detected involving '{comp_name}'"),
                node.span,
                "Forma component trees must be statically acyclic",
            ));
        }
        if args.len() != target_def.params.len() {
            return Err(InfimumDiagnostic::new(
                "E096_FORMA_COMPONENT_ARITY",
                "forma-expand",
                format!(
                    "Forma component '{comp_name}' expects {} argument(s), got {}",
                    target_def.params.len(),
                    args.len()
                ),
                node.span,
                "Match the arguments passed to the nested Forma component",
            ));
        }
        let mut new_subst = BTreeMap::new();
        for (param, arg_raw) in target_def.params.iter().zip(&args) {
            let lowered_arg = resolver.lower_expr(arg_raw, param_locals)?;
            new_subst.insert(param.name.clone(), lowered_arg);
        }
        let prev_uid = resolver.cur_uid;
        let prev_subst = std::mem::replace(&mut resolver.forma_subst, new_subst);
        let mut target_locals: Vec<BTreeSet<String>> = vec![BTreeSet::new()];
        resolver.cur_uid = *target_uid;
        visiting.push(target_canon);
        let expanded = expand_and_lower_forma_node(
            target_def.root.clone(),
            defs,
            resolver,
            visiting,
            &mut target_locals,
        );
        visiting.pop();
        resolver.cur_uid = prev_uid;
        resolver.forma_subst = prev_subst;
        return expanded;
    }

    // 4. Regular built-in Forma node: lower props in current unit scope & recursively lower children
    let lowered_props = node
        .props
        .try_map(&mut |e| resolver.lower_expr(&e, param_locals))?;
    let mut new_children = Vec::with_capacity(node.children.len());
    for ch in node.children {
        new_children.extend(expand_and_lower_forma_node(
            ch,
            defs,
            resolver,
            visiting,
            param_locals,
        )?);
    }
    Ok(vec![crate::forma::FormaNode {
        kind: node.kind,
        props: lowered_props,
        children: new_children,
        span: node.span,
        component_call: None,
        forma_for: None,
        forma_if: None,
    }])
}
