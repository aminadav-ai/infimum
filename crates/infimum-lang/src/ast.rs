use serde::{Deserialize, Serialize};
use std::fmt;

pub const REGION_RAW: u32 = 0;
pub const REGION_SLICE_DATA: u32 = 1;

fn is_zero_u32(v: &u32) -> bool {
    *v == 0
}

fn default_elem_bytes() -> i64 {
    4
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub enum Type {
    I64,
    U8,
    Fixed,
    Dec64,
    Bool,
    Str,
    Unit,
    Secret(Box<Type>),
    Tuple(Vec<Type>),
    Slice(Box<Type>),
}

impl fmt::Display for Type {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Type::I64 => write!(f, "i64"),
            Type::U8 => write!(f, "u8"),
            Type::Fixed => write!(f, "fixed"),
            Type::Dec64 => write!(f, "dec64"),
            Type::Bool => write!(f, "bool"),
            Type::Str => write!(f, "str"),
            Type::Unit => write!(f, "unit"),
            Type::Secret(inner) => write!(f, "Secret<{inner}>"),
            Type::Tuple(elems) => {
                write!(f, "(")?;
                for (i, t) in elems.iter().enumerate() {
                    if i > 0 {
                        write!(f, ", ")?;
                    }
                    write!(f, "{t}")?;
                }
                write!(f, ")")
            }
            Type::Slice(inner) => write!(f, "[{inner}]"),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct EffectCap {
    pub name: String,
    pub scope: Option<String>,
}

impl fmt::Display for EffectCap {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match &self.scope {
            Some(s) => write!(f, "{}(\"{}\")", self.name, s.escape_default()),
            None => write!(f, "{}", self.name),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Param {
    pub name: String,
    pub ty: Type,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum UnaryOp {
    Neg,
    Not,
    IntToFixed,
    FixedToInt,
    FixedToRaw,
    RawToFixed,
    IntToDec64,
    Dec64ToInt,
    Dec64ToRaw,
    RawToDec64,
    ToU8,
}

impl UnaryOp {
    pub fn as_str(self) -> &'static str {
        match self {
            UnaryOp::Neg => "-",
            UnaryOp::Not => "!",
            UnaryOp::IntToFixed => "fixed",
            UnaryOp::FixedToInt => "fixed_to_int",
            UnaryOp::FixedToRaw => "fixed_raw",
            UnaryOp::RawToFixed => "fixed_from_raw",
            UnaryOp::IntToDec64 => "dec64",
            UnaryOp::Dec64ToInt => "dec64_to_int",
            UnaryOp::Dec64ToRaw => "dec64_raw",
            UnaryOp::RawToDec64 => "dec64_from_raw",
            UnaryOp::ToU8 => "u8",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum BinOp {
    Add,
    Sub,
    Mul,
    Div,
    FixedMul,
    FixedDiv,
    DecMul,
    DecDiv,
    Mod,
    BitAnd,
    BitOr,
    BitXor,
    Shl,
    Shr,
    Eq,
    Ne,
    Lt,
    Le,
    Gt,
    Ge,
    And,
    Or,
}

impl BinOp {
    pub fn as_str(self) -> &'static str {
        match self {
            BinOp::Add => "+",
            BinOp::Sub => "-",
            BinOp::Mul => "*",
            BinOp::Div => "/",
            BinOp::FixedMul => "*.",
            BinOp::FixedDiv => "/.",
            BinOp::DecMul => "*d",
            BinOp::DecDiv => "/d",
            BinOp::Mod => "%",
            BinOp::BitAnd => "&",
            BinOp::BitOr => "|",
            BinOp::BitXor => "^",
            BinOp::Shl => "<<",
            BinOp::Shr => ">>",
            BinOp::Eq => "==",
            BinOp::Ne => "!=",
            BinOp::Lt => "<",
            BinOp::Le => "<=",
            BinOp::Gt => ">",
            BinOp::Ge => ">=",
            BinOp::And => "&&",
            BinOp::Or => "||",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TargetRef {
    Named(String),
    Hash(String),
    SelfRec,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Stmt {
    Let {
        name: String,
        #[serde(default)]
        mutable: bool,
        ty: Option<Type>,
        value: Expr,
    },
    LetTuple {
        names: Vec<(String, bool)>,
        value: Expr,
    },
    Assign {
        name: String,
        value: Expr,
    },
    While {
        cond: Expr,
        max_iters: u32,
        body: Vec<Stmt>,
    },
    Expr(Expr),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Expr {
    IntLit(i64),
    FixedLit(i64),
    DecLit(i64),
    BoolLit(bool),
    StrLit(String),
    UnitLit,
    Var(String),
    Unary {
        op: UnaryOp,
        expr: Box<Expr>,
    },
    Binary {
        op: BinOp,
        lhs: Box<Expr>,
        rhs: Box<Expr>,
    },
    If {
        cond: Box<Expr>,
        then_branch: Box<Expr>,
        else_branch: Box<Expr>,
    },
    Call {
        target: TargetRef,
        args: Vec<Expr>,
    },
    Perform {
        effect: String,
        op: String,
        args: Vec<Expr>,
    },
    MemLoad {
        index: Box<Expr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inv: Option<(i64, i64)>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fixed_ty: bool,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    MemStore {
        index: Box<Expr>,
        value: Box<Expr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inv: Option<(i64, i64)>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        fixed_ty: bool,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    ByteLoad {
        index: Box<Expr>,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    ByteStore {
        index: Box<Expr>,
        value: Box<Expr>,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    /// 64-bit signed word-aligned load (`i64.load align=2`), used for `dec64` in `layout` and `[dec64]`.
    MemLoad64 {
        index: Box<Expr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inv: Option<(i64, i64)>,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    /// 64-bit signed word-aligned store (`i64.store align=2`), used for `dec64` in `layout` and `[dec64]`.
    MemStore64 {
        index: Box<Expr>,
        value: Box<Expr>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        inv: Option<(i64, i64)>,
        #[serde(default, skip_serializing_if = "is_zero_u32")]
        region: u32,
    },
    SecretWrap(Box<Expr>),
    Reveal {
        domain: String,
        expr: Box<Expr>,
    },
    Block {
        stmts: Vec<Stmt>,
        tail: Box<Expr>,
    },
    TupleLit(Vec<Expr>),
    TupleField {
        expr: Box<Expr>,
        index: usize,
    },
    SliceLit {
        ptr: Box<Expr>,
        len: Box<Expr>,
        elem_ty: Type,
    },
    SlicePtr(Box<Expr>),
    SliceLen(Box<Expr>),
    /// Zero-cost view of a UTF-8 `str` as a read-only byte slice `[u8]` (`(byte_ptr, byte_len)`).
    StrToBytes(Box<Expr>),
    /// Zero-cost wrap of a byte slice `[u8]` as a UTF-8 `str` (`(byte_ptr, byte_len)`).
    StrFromBytes(Box<Expr>),
    BoundsCheck {
        index: Box<Expr>,
        len: Box<Expr>,
        #[serde(default)]
        inclusive_hi: bool,
    },
    RangeCheck {
        lo: Box<Expr>,
        hi: Box<Expr>,
        len: Box<Expr>,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FnDef {
    pub name: String,
    #[serde(default)]
    pub exported: bool,
    pub params: Vec<Param>,
    pub effects: Vec<EffectCap>,
    pub ret_type: Type,
    pub requires: Vec<Expr>,
    pub ensures: Vec<Expr>,
    pub body: Expr,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_file: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub span: Option<crate::diag::Span>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "value")]
pub enum ConstValue {
    Int(i64),
    Fixed(i64),
    Dec64(i64),
    Bool(bool),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ConstDef {
    pub name: String,
    pub ty: Type,
    pub value: ConstValue,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumVariant {
    pub name: String,
    pub value: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EnumDef {
    pub name: String,
    pub variants: Vec<EnumVariant>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutField {
    pub name: String,
    pub offset: i64,
    pub len: i64,
    pub is_array: bool,
    #[serde(default)]
    pub is_vec: bool,
    #[serde(default)]
    pub cap: i64,
    #[serde(default)]
    pub stride: i64,
    #[serde(default = "default_elem_bytes")]
    pub elem_bytes: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct LayoutDef {
    pub name: String,
    pub base: i64,
    pub fields: Vec<LayoutField>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Module {
    #[serde(default)]
    pub consts: Vec<ConstDef>,
    #[serde(default)]
    pub enums: Vec<EnumDef>,
    #[serde(default)]
    pub layouts: Vec<LayoutDef>,
    #[serde(default)]
    pub formas: Vec<crate::forma::FormaDef>,
    pub functions: Vec<FnDef>,
    #[serde(default)]
    pub tests: Vec<(String, String)>,
}
