// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

use std::collections::{BTreeMap, BTreeSet};

use crate::ast::{BinOp, EffectCap, TargetRef, Type, UnaryOp};
use crate::diag::Span;

#[derive(Clone)]
pub(super) struct RawUseItem {
    pub(super) name: String,
    pub(super) alias: Option<String>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(super) enum RawUseKind {
    All,
    Alias(String, Span),
    Selective(Vec<RawUseItem>),
}

#[derive(Clone)]
pub(super) struct RawUse {
    pub(super) is_pub: bool,
    pub(super) path: String,
    pub(super) kind: RawUseKind,
    pub(super) span: Span,
}

pub(super) struct RawMod {
    pub(super) name: String,
    pub(super) is_pub: bool,
    pub(super) span: Span,
    pub(super) uses: Vec<RawUse>,
    pub(super) items: Vec<RawItem>,
    pub(super) formas: Vec<crate::forma::FormaDef<RawExpr>>,
    pub(super) functions: Vec<RawFnDef>,
}

pub(super) struct RawConst {
    pub(super) name: String,
    pub(super) is_pub: bool,
    pub(super) ty: Type,
    pub(super) expr: RawExpr,
    pub(super) span: Span,
}

pub(super) struct RawEnumVariant {
    pub(super) name: String,
    pub(super) value: Option<RawExpr>,
    pub(super) span: Span,
}

pub(super) struct RawEnum {
    pub(super) name: String,
    pub(super) is_pub: bool,
    pub(super) variants: Vec<RawEnumVariant>,
    pub(super) span: Span,
}

pub(super) struct RawInv {
    pub(super) lo: RawExpr,
    pub(super) hi: RawExpr,
    pub(super) inclusive: bool,
    pub(super) span: Span,
}

pub(super) struct RawStruct {
    pub(super) name: String,
    pub(super) is_pub: bool,
    pub(super) fields: Vec<(String, bool, Option<RawInv>, Span)>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(super) struct StructDef {
    pub(super) fields: BTreeMap<String, i64>,
    pub(super) field_invs: BTreeMap<String, (i64, i64)>,
    pub(super) field_fixed: BTreeSet<String>,
    pub(super) field_names: Vec<String>,
    pub(super) size: i64,
}

#[derive(Clone)]
pub(super) struct LayoutFieldMeta {
    pub(super) offset: i64,
    pub(super) is_array: bool,
    pub(super) is_vec: bool,
    pub(super) is_fixed: bool,
    pub(super) struct_type: Option<String>,
    pub(super) stride: i64,
    pub(super) count: i64,
    pub(super) inv: Option<(i64, i64)>,
}

pub(super) struct RawLayoutField {
    pub(super) name: String,
    pub(super) struct_module: Option<String>,
    pub(super) struct_type: Option<String>,
    pub(super) len_expr: Option<RawExpr>,
    pub(super) is_array: bool,
    pub(super) is_vec: bool,
    pub(super) is_fixed: bool,
    pub(super) at_expr: Option<RawExpr>,
    pub(super) inv: Option<RawInv>,
    pub(super) span: Span,
}

pub(super) struct RawLayout {
    pub(super) name: String,
    pub(super) is_pub: bool,
    pub(super) base_expr: Option<RawExpr>,
    pub(super) fields: Vec<RawLayoutField>,
    pub(super) span: Span,
}

pub(super) enum RawItem {
    Const(RawConst),
    Enum(RawEnum),
    Struct(RawStruct),
    Layout(RawLayout),
    Mod(RawMod),
}

#[derive(Clone)]
pub(crate) enum RawMatchPattern {
    Wildcard(Span),
    Int(i64),
    Bool(bool),
    ConstRef(String, Span),
    Path(Vec<String>, Span),
}

#[derive(Clone)]
pub(crate) struct RawMatchArm {
    pub(super) patterns: Vec<RawMatchPattern>,
    pub(super) body: RawExpr,
}

#[derive(Clone)]
pub(crate) enum RawType {
    Builtin(Type),
    Named {
        module: Option<String>,
        name: String,
        span: Span,
    },
    Secret(Box<RawType>),
    Tuple(Vec<RawType>),
    Slice(Box<RawType>),
}

#[derive(Clone)]
pub(crate) struct RawSliceStore {
    pub(super) base: RawExpr,
    pub(super) index: RawExpr,
    pub(super) subfield: Option<String>,
    pub(super) op: Option<BinOp>,
    pub(super) value: RawExpr,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) enum RawStmt {
    Let {
        name: String,
        mutable: bool,
        ty: Option<RawType>,
        value: RawExpr,
    },
    LetTuple {
        names: Vec<(String, bool)>,
        value: RawExpr,
    },
    Assign {
        name: String,
        op: Option<BinOp>,
        value: RawExpr,
    },
    LayoutStore {
        module: Option<String>,
        layout: String,
        field: String,
        index: Option<RawExpr>,
        subfield: Option<String>,
        op: Option<BinOp>,
        value: RawExpr,
        span: Span,
    },
    SliceStore(Box<RawSliceStore>),
    While {
        cond: RawExpr,
        max_iters: RawExpr,
        body: Vec<RawStmt>,
        span: Span,
    },
    For {
        var: String,
        start: RawExpr,
        end: RawExpr,
        inclusive: bool,
        max_iters: Option<RawExpr>,
        body: Vec<RawStmt>,
        span: Span,
    },
    Break(Span),
    Continue(Span),
    Expr(RawExpr),
}

#[derive(Clone)]
pub(crate) struct RawLayoutLoad {
    pub(super) module: Option<String>,
    pub(super) layout: String,
    pub(super) field: String,
    pub(super) index: Option<Box<RawExpr>>,
    pub(super) subfield: Option<String>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) struct RawModCall {
    pub(super) module: String,
    pub(super) func: String,
    pub(super) args: Vec<RawExpr>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) struct RawStructLit {
    pub(super) module: Option<String>,
    pub(super) name: String,
    pub(super) fields: Vec<(String, RawExpr, Span)>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) struct RawSliceIndex {
    pub(super) base: RawExpr,
    pub(super) index: RawExpr,
    pub(super) subfield: Option<String>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) struct RawSliceRange {
    pub(super) base: RawExpr,
    pub(super) lo: Option<RawExpr>,
    pub(super) hi: Option<RawExpr>,
    pub(super) span: Span,
}

#[derive(Clone)]
pub(crate) enum RawExpr {
    IntLit(i64),
    FixedLit(i64),
    BoolLit(bool),
    StrLit(String),
    UnitLit,
    Var(String),
    Path {
        segs: Vec<String>,
        span: Span,
    },
    LayoutLoad(Box<RawLayoutLoad>),
    Unary {
        op: UnaryOp,
        expr: Box<RawExpr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<RawExpr>,
        rhs: Box<RawExpr>,
    },
    If {
        cond: Box<RawExpr>,
        then_branch: Box<RawExpr>,
        else_branch: Box<RawExpr>,
    },
    Match {
        scrutinee: Box<RawExpr>,
        arms: Vec<RawMatchArm>,
        span: Span,
    },
    Call {
        target: TargetRef,
        args: Vec<RawExpr>,
        span: Span,
    },
    ModCall(Box<RawModCall>),
    Perform {
        effect: String,
        op: String,
        args: Vec<RawExpr>,
    },
    MemLoad {
        index: Box<RawExpr>,
    },
    MemStore {
        index: Box<RawExpr>,
        value: Box<RawExpr>,
    },
    SecretWrap(Box<RawExpr>),
    Reveal {
        domain: String,
        expr: Box<RawExpr>,
    },
    Block {
        stmts: Vec<RawStmt>,
        tail: Box<RawExpr>,
    },
    TupleLit(Vec<RawExpr>),
    TupleField {
        expr: Box<RawExpr>,
        index: usize,
    },
    StructLit(Box<RawStructLit>),
    FieldAccess {
        expr: Box<RawExpr>,
        field: String,
        span: Span,
    },
    SliceIndex(Box<RawSliceIndex>),
    SliceRange(Box<RawSliceRange>),
}

pub(super) struct RawFnDef {
    pub(super) name: String,
    pub(super) exported: bool,
    pub(super) is_pub: bool,
    pub(super) test_label: Option<String>,
    pub(super) params: Vec<(String, RawType)>,
    pub(super) effects: Vec<EffectCap>,
    pub(super) ret_type: RawType,
    pub(super) requires: Vec<RawExpr>,
    pub(super) ensures: Vec<RawExpr>,
    pub(super) body: RawExpr,
    pub(super) source_file: Option<String>,
    pub(super) span: Span,
}

pub(super) struct RawFile {
    pub(super) uses: Vec<RawUse>,
    pub(super) items: Vec<RawItem>,
    pub(super) formas: Vec<crate::forma::FormaDef<RawExpr>>,
    pub(super) functions: Vec<RawFnDef>,
}

// ---------------------------------------------------------------------------
// Public Entry Points
// ---------------------------------------------------------------------------


pub(super) type UnitId = usize;

pub(super) struct RawUnit {
    pub(super) id: UnitId,
    pub(super) display_label: String,
    pub(super) parent: Option<UnitId>,
    pub(super) inline_mod: Option<(String, bool, Span)>,
    pub(super) is_entry: bool,
    pub(super) is_forma_rt: bool,
    pub(super) uses: Vec<(RawUse, UnitId)>,
    pub(super) items: Vec<RawItem>,
    pub(super) formas: Vec<crate::forma::FormaDef<RawExpr>>,
    pub(super) functions: Vec<RawFnDef>,
    pub(super) inline_children: Vec<UnitId>,
}

pub(super) struct UnitGraph {
    pub(super) units: Vec<RawUnit>,
    pub(super) by_key: BTreeMap<String, UnitId>,
    pub(super) topo_order: Vec<UnitId>,
    pub(super) entry_uid: UnitId,
}

impl UnitGraph {
    pub(super) fn find_unit(&self, canonical_key: &str) -> Option<UnitId> {
        self.by_key.get(canonical_key).copied()
    }
}

