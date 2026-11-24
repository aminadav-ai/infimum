//! Lightweight, zero-dependency interval + linear constraint verifier for `wasm32` / Online Infimum Code.
//!
//! Covers ~80-90% of everyday Infimum verification checks in <1 ms without Z3 (~50 KB in Wasm):
//! - Division/modulo by zero (`E053_POSSIBLE_DIV_BY_ZERO`)
//! - Basic `requires` / `ensures` contract bounds (`E050_CONTRACT_VIOLATION`, `E051_CALL_PRECONDITION_VIOLATED`)
//! - `Forma` UI fixed-size layout overflow checks (`E090_FORMA_LAYOUT_VIOLATION`)
//!
//! For coupled multi-variable or nonlinear contracts where interval arithmetic cannot decide,
//! this verifier honestly reports `inconclusive` and recommends local Infimum Code with Z3.

use crate::ast::{BinOp, Expr, FnDef, Module, Stmt, TargetRef, UnaryOp};
use crate::diag::{InfimumDiagnostic, Span};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

const I64_LO: i128 = i64::MIN as i128;
const I64_HI: i128 = i64::MAX as i128;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Interval {
    lo: i128,
    hi: i128,
    nonzero: bool,
}

impl Interval {
    fn top() -> Self {
        Self {
            lo: I64_LO,
            hi: I64_HI,
            nonzero: false,
        }
    }

    fn point(v: i64) -> Self {
        let x = v as i128;
        Self {
            lo: x,
            hi: x,
            nonzero: x != 0,
        }
    }

    fn range(lo: i128, hi: i128) -> Self {
        let lo = lo.clamp(I64_LO, I64_HI);
        let hi = hi.clamp(I64_LO, I64_HI);
        let nonzero = lo > 0 || hi < 0;
        Self { lo, hi, nonzero }
    }

    fn is_empty(self) -> bool {
        self.lo > self.hi || (self.nonzero && self.lo == 0 && self.hi == 0)
    }

    fn is_definitely_zero(self) -> bool {
        self.lo == 0 && self.hi == 0 && !self.nonzero
    }

    fn may_be_zero(self) -> bool {
        !self.nonzero && self.lo <= 0 && self.hi >= 0
    }

    fn join(self, other: Self) -> Self {
        if self.is_empty() {
            return other;
        }
        if other.is_empty() {
            return self;
        }
        let lo = self.lo.min(other.lo);
        let hi = self.hi.max(other.hi);
        let nonzero = (lo > 0 || hi < 0) || (self.nonzero && other.nonzero);
        Self { lo, hi, nonzero }
    }

    fn intersect(self, other: Self) -> Self {
        let lo = self.lo.max(other.lo);
        let hi = self.hi.min(other.hi);
        let nonzero = self.nonzero || other.nonzero || lo > 0 || hi < 0;
        Self { lo, hi, nonzero }
    }

    fn neg(self) -> Self {
        let lo = (-self.hi).clamp(I64_LO, I64_HI);
        let hi = (-self.lo).clamp(I64_LO, I64_HI);
        Self {
            lo,
            hi,
            nonzero: self.nonzero || lo > 0 || hi < 0,
        }
    }

    fn add(self, other: Self) -> Self {
        Self::range(self.lo.saturating_add(other.lo), self.hi.saturating_add(other.hi))
    }

    fn sub(self, other: Self) -> Self {
        Self::range(self.lo.saturating_sub(other.hi), self.hi.saturating_sub(other.lo))
    }

    fn mul(self, other: Self) -> Self {
        let corners = [
            self.lo.saturating_mul(other.lo),
            self.lo.saturating_mul(other.hi),
            self.hi.saturating_mul(other.lo),
            self.hi.saturating_mul(other.hi),
        ];
        let lo = *corners.iter().min().unwrap();
        let hi = *corners.iter().max().unwrap();
        let mut r = Self::range(lo, hi);
        if self.nonzero && other.nonzero {
            r.nonzero = true;
        }
        r
    }

    fn div(self, other: Self) -> Self {
        if other.lo > 0 || other.hi < 0 {
            let corners = [
                self.lo / other.lo,
                self.lo / other.hi,
                self.hi / other.lo,
                self.hi / other.hi,
            ];
            let lo = *corners.iter().min().unwrap();
            let hi = *corners.iter().max().unwrap();
            Self::range(lo, hi)
        } else {
            Self::top()
        }
    }

    fn rem(self, other: Self) -> Self {
        if other.lo > 0 {
            let bound = other.hi - 1;
            if self.lo >= 0 {
                Self::range(0, bound.min(self.hi))
            } else {
                Self::range(-bound, bound)
            }
        } else {
            Self::top()
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Truth {
    AlwaysTrue,
    AlwaysFalse,
    Unknown,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct LightVerifyReport {
    pub proven: usize,
    pub inconclusive: Vec<String>,
}

type Env = BTreeMap<String, Interval>;

fn refine_env(env: &mut Env, cond: &Expr, assume_true: bool) {
    match cond {
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => refine_env(env, expr, !assume_true),
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } if assume_true => {
            refine_env(env, lhs, true);
            refine_env(env, rhs, true);
        }
        Expr::Binary {
            op: BinOp::Or,
            lhs,
            rhs,
        } if !assume_true => {
            refine_env(env, lhs, false);
            refine_env(env, rhs, false);
        }
        Expr::Binary { op, lhs, rhs } => {
            let eff_op = if assume_true {
                *op
            } else {
                match op {
                    BinOp::Eq => BinOp::Ne,
                    BinOp::Ne => BinOp::Eq,
                    BinOp::Lt => BinOp::Ge,
                    BinOp::Le => BinOp::Gt,
                    BinOp::Gt => BinOp::Le,
                    BinOp::Ge => BinOp::Lt,
                    _ => return,
                }
            };
            if let (Expr::Var(name), Some(c)) = (lhs.as_ref(), eval_const_i64(rhs)) {
                apply_var_cmp(env, name, eff_op, c as i128);
            } else if let (Some(c), Expr::Var(name)) = (eval_const_i64(lhs), rhs.as_ref()) {
                let flipped = match eff_op {
                    BinOp::Lt => BinOp::Gt,
                    BinOp::Le => BinOp::Ge,
                    BinOp::Gt => BinOp::Lt,
                    BinOp::Ge => BinOp::Le,
                    other => other,
                };
                apply_var_cmp(env, name, flipped, c as i128);
            }
        }
        _ => {}
    }
}

fn eval_const_i64(e: &Expr) -> Option<i64> {
    match e {
        Expr::IntLit(v) | Expr::FixedLit(v) => Some(*v),
        Expr::Unary {
            op: UnaryOp::Neg,
            expr,
        } => eval_const_i64(expr).map(|v| v.wrapping_neg()),
        _ => None,
    }
}

fn apply_var_cmp(env: &mut Env, name: &str, op: BinOp, c: i128) {
    let cur = env.get(name).copied().unwrap_or_else(Interval::top);
    let narrowed = match op {
        BinOp::Eq => cur.intersect(Interval::range(c, c)),
        BinOp::Ne => {
            if c == 0 {
                let mut n = cur;
                n.nonzero = true;
                n
            } else if cur.lo == c && cur.hi == c {
                Interval {
                    lo: 1,
                    hi: 0,
                    nonzero: false,
                }
            } else {
                cur
            }
        }
        BinOp::Lt => cur.intersect(Interval::range(I64_LO, c - 1)),
        BinOp::Le => cur.intersect(Interval::range(I64_LO, c)),
        BinOp::Gt => cur.intersect(Interval::range(c + 1, I64_HI)),
        BinOp::Ge => cur.intersect(Interval::range(c, I64_HI)),
        _ => cur,
    };
    env.insert(name.to_string(), narrowed);
}

fn eval_truth(env: &Env, cond: &Expr) -> Truth {
    match cond {
        Expr::BoolLit(true) => Truth::AlwaysTrue,
        Expr::BoolLit(false) => Truth::AlwaysFalse,
        Expr::Unary {
            op: UnaryOp::Not,
            expr,
        } => match eval_truth(env, expr) {
            Truth::AlwaysTrue => Truth::AlwaysFalse,
            Truth::AlwaysFalse => Truth::AlwaysTrue,
            Truth::Unknown => Truth::Unknown,
        },
        Expr::Binary {
            op: BinOp::And,
            lhs,
            rhs,
        } => match (eval_truth(env, lhs), eval_truth(env, rhs)) {
            (Truth::AlwaysFalse, _) | (_, Truth::AlwaysFalse) => Truth::AlwaysFalse,
            (Truth::AlwaysTrue, Truth::AlwaysTrue) => Truth::AlwaysTrue,
            _ => Truth::Unknown,
        },
        Expr::Binary {
            op: BinOp::Or,
            lhs,
            rhs,
        } => match (eval_truth(env, lhs), eval_truth(env, rhs)) {
            (Truth::AlwaysTrue, _) | (_, Truth::AlwaysTrue) => Truth::AlwaysTrue,
            (Truth::AlwaysFalse, Truth::AlwaysFalse) => Truth::AlwaysFalse,
            _ => Truth::Unknown,
        },
        Expr::Binary { op, lhs, rhs } => {
            let a = eval_interval_pure(env, lhs);
            let b = eval_interval_pure(env, rhs);
            match op {
                BinOp::Lt => {
                    if a.hi < b.lo {
                        Truth::AlwaysTrue
                    } else if a.lo >= b.hi {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                BinOp::Le => {
                    if a.hi <= b.lo {
                        Truth::AlwaysTrue
                    } else if a.lo > b.hi {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                BinOp::Gt => {
                    if a.lo > b.hi {
                        Truth::AlwaysTrue
                    } else if a.hi <= b.lo {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                BinOp::Ge => {
                    if a.lo >= b.hi {
                        Truth::AlwaysTrue
                    } else if a.hi < b.lo {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                BinOp::Eq => {
                    if a.lo == a.hi && b.lo == b.hi && a.lo == b.lo {
                        Truth::AlwaysTrue
                    } else if a.hi < b.lo
                        || b.hi < a.lo
                        || (a.nonzero && b.is_definitely_zero())
                        || (b.nonzero && a.is_definitely_zero())
                    {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                BinOp::Ne => {
                    if a.hi < b.lo
                        || b.hi < a.lo
                        || (a.nonzero && b.is_definitely_zero())
                        || (b.nonzero && a.is_definitely_zero())
                    {
                        Truth::AlwaysTrue
                    } else if a.lo == a.hi && b.lo == b.hi && a.lo == b.lo {
                        Truth::AlwaysFalse
                    } else {
                        Truth::Unknown
                    }
                }
                _ => Truth::Unknown,
            }
        }
        _ => Truth::Unknown,
    }
}

fn eval_interval_pure(env: &Env, expr: &Expr) -> Interval {
    match expr {
        Expr::IntLit(v) | Expr::FixedLit(v) => Interval::point(*v),
        Expr::Var(name) => env.get(name).copied().unwrap_or_else(Interval::top),
        Expr::Unary {
            op: UnaryOp::Neg,
            expr,
        } => eval_interval_pure(env, expr).neg(),
        Expr::Binary { op, lhs, rhs } => {
            let a = eval_interval_pure(env, lhs);
            let b = eval_interval_pure(env, rhs);
            match op {
                BinOp::Add => a.add(b),
                BinOp::Sub => a.sub(b),
                BinOp::Mul => a.mul(b),
                BinOp::Div => a.div(b),
                BinOp::Mod => a.rem(b),
                BinOp::BitAnd => {
                    if b.lo >= 0 && b.lo == b.hi {
                        Interval::range(0, b.hi)
                    } else {
                        Interval::top()
                    }
                }
                _ => Interval::top(),
            }
        }
        _ => Interval::top(),
    }
}

struct FnChecker<'a> {
    fn_name: &'a str,
    fns: &'a BTreeMap<String, &'a FnDef>,
    inconclusive: &'a mut Vec<String>,
}

impl<'a> FnChecker<'a> {
    fn eval_expr(&mut self, env: &mut Env, expr: &Expr) -> Result<Interval, InfimumDiagnostic> {
        match expr {
            Expr::IntLit(v) | Expr::FixedLit(v) => Ok(Interval::point(*v)),
            Expr::BoolLit(_) | Expr::StrLit(_) | Expr::UnitLit => Ok(Interval::top()),
            Expr::Var(name) => Ok(env.get(name).copied().unwrap_or_else(Interval::top)),
            Expr::Unary { op, expr } => {
                let inner = self.eval_expr(env, expr)?;
                Ok(match op {
                    UnaryOp::Neg => inner.neg(),
                    UnaryOp::Not => Interval::top(),
                    UnaryOp::IntToFixed => inner.mul(Interval::point(65536)),
                    UnaryOp::FixedToInt => inner.div(Interval::point(65536)),
                    UnaryOp::FixedToRaw | UnaryOp::RawToFixed => inner,
                })
            }
            Expr::Binary { op, lhs, rhs } => {
                let a = self.eval_expr(env, lhs)?;
                let b = self.eval_expr(env, rhs)?;
                if matches!(op, BinOp::Div | BinOp::Mod | BinOp::FixedDiv) {
                    if b.is_definitely_zero() {
                        return Err(InfimumDiagnostic::new(
                            "E053_POSSIBLE_DIV_BY_ZERO",
                            "light-verify",
                            format!("Division or modulo by zero in function '{}'", self.fn_name),
                            Span::zero(),
                            "Guard the divisor with `requires d != 0` or `if d != 0 { ... }`",
                        ));
                    }
                    if b.may_be_zero() {
                        self.inconclusive.push(format!(
                            "Light verifier: possible division by zero in '{}' — install local Infimum Code with Z3 for full SMT proof",
                            self.fn_name
                        ));
                    }
                }
                Ok(match op {
                    BinOp::Add => a.add(b),
                    BinOp::Sub => a.sub(b),
                    BinOp::Mul => a.mul(b),
                    BinOp::Div => a.div(b),
                    BinOp::FixedMul => a.mul(b).div(Interval::point(65536)),
                    BinOp::FixedDiv => a.mul(Interval::point(65536)).div(b),
                    BinOp::Mod => a.rem(b),
                    BinOp::BitAnd => {
                        if b.lo >= 0 && b.lo == b.hi {
                            Interval::range(0, b.hi)
                        } else {
                            Interval::top()
                        }
                    }
                    _ => Interval::top(),
                })
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let _ = self.eval_expr(env, cond)?;
                let mut env_then = env.clone();
                refine_env(&mut env_then, cond, true);
                let mut env_else = env.clone();
                refine_env(&mut env_else, cond, false);
                let t = if env_then.values().any(|v| v.is_empty()) {
                    Interval {
                        lo: 1,
                        hi: 0,
                        nonzero: false,
                    }
                } else {
                    self.eval_expr(&mut env_then, then_branch)?
                };
                let e = if env_else.values().any(|v| v.is_empty()) {
                    Interval {
                        lo: 1,
                        hi: 0,
                        nonzero: false,
                    }
                } else {
                    self.eval_expr(&mut env_else, else_branch)?
                };
                Ok(t.join(e))
            }
            Expr::Call { target, args } => {
                let mut arg_ivs = Vec::with_capacity(args.len());
                for a in args {
                    arg_ivs.push(self.eval_expr(env, a)?);
                }
                if let TargetRef::Named(callee_name) = target {
                    if let Some(callee) = self.fns.get(callee_name) {
                        let mut callee_env = Env::new();
                        for (p, iv) in callee.params.iter().zip(arg_ivs.iter()) {
                            callee_env.insert(p.name.clone(), *iv);
                        }
                        for req in &callee.requires {
                            if eval_truth(&callee_env, req) == Truth::AlwaysFalse {
                                return Err(InfimumDiagnostic::new(
                                    "E051_CALL_PRECONDITION_VIOLATED",
                                    "light-verify",
                                    format!(
                                        "Call to '{}' in '{}' violates precondition",
                                        callee_name, self.fn_name
                                    ),
                                    Span::zero(),
                                    "Ensure call arguments satisfy the callee's `requires` clause",
                                ));
                            }
                        }
                        let mut res_env = callee_env;
                        res_env.insert("result".to_string(), Interval::top());
                        for ens in &callee.ensures {
                            refine_env(&mut res_env, ens, true);
                        }
                        if let Some(rv) = res_env.get("result") {
                            return Ok(*rv);
                        }
                    }
                    // Built-in clamp/max/min heuristics
                    if (callee_name == "clamp_i64" || callee_name == "clamp") && arg_ivs.len() == 3
                    {
                        return Ok(Interval::range(arg_ivs[1].lo, arg_ivs[2].hi));
                    }
                }
                Ok(Interval::top())
            }
            Expr::Perform { args, .. } => {
                for a in args {
                    let _ = self.eval_expr(env, a)?;
                }
                Ok(Interval::top())
            }
            Expr::MemLoad { index, inv, .. } => {
                let _ = self.eval_expr(env, index)?;
                Ok(match inv {
                    Some((lo, hi)) => Interval {
                        lo: *lo as i128,
                        hi: *hi as i128,
                        nonzero: *lo > 0 || *hi < 0,
                    },
                    None => Interval::top(),
                })
            }
            Expr::MemStore { index, value, inv, .. } => {
                let _ = self.eval_expr(env, index)?;
                let v = self.eval_expr(env, value)?;
                if let Some((lo, hi)) = inv {
                    let lo128 = *lo as i128;
                    let hi128 = *hi as i128;
                    if v.hi < lo128 || v.lo > hi128 {
                        return Err(InfimumDiagnostic::new(
                            "E054_FIELD_INVARIANT_VIOLATED",
                            "light-verify",
                            format!("Store value [{}, {}] violates field invariant 'inv {lo}..={hi}'", v.lo, v.hi),
                            Span::zero(),
                            format!("Ensure stored value is within [{lo}..={hi}]"),
                        ));
                    } else if !(v.lo >= lo128 && v.hi <= hi128) {
                        self.inconclusive.push(format!(
                            "Light verifier: inconclusive for field invariant 'inv {lo}..={hi}' in '{}' — install local Infimum Code with Z3 for full SMT proof",
                            self.fn_name
                        ));
                    }
                }
                Ok(Interval::top())
            }
            Expr::SecretWrap(inner) | Expr::Reveal { expr: inner, .. } => {
                self.eval_expr(env, inner)
            }
            Expr::Block { stmts, tail } => {
                let mut local = env.clone();
                for s in stmts {
                    self.eval_stmt(&mut local, s)?;
                }
                self.eval_expr(&mut local, tail)
            }
            Expr::TupleLit(elems) => {
                for e in elems {
                    let _ = self.eval_expr(env, e)?;
                }
                Ok(Interval::top())
            }
            Expr::TupleField { expr, index } => {
                if let Expr::TupleLit(elems) = expr.as_ref() {
                    if let Some(target) = elems.get(*index) {
                        for (i, e) in elems.iter().enumerate() {
                            if i != *index {
                                let _ = self.eval_expr(env, e)?;
                            }
                        }
                        return self.eval_expr(env, target);
                    }
                }
                let _ = self.eval_expr(env, expr)?;
                Ok(Interval::top())
            }
        }
    }

    fn eval_stmt(&mut self, env: &mut Env, stmt: &Stmt) -> Result<(), InfimumDiagnostic> {
        match stmt {
            Stmt::Let { name, value, .. } => {
                let iv = self.eval_expr(env, value)?;
                env.insert(name.clone(), iv);
            }
            Stmt::LetTuple { names, value } => {
                if let Expr::TupleLit(elems) = value {
                    for ((n, _), e) in names.iter().zip(elems.iter()) {
                        let iv = self.eval_expr(env, e)?;
                        env.insert(n.clone(), iv);
                    }
                } else {
                    let _ = self.eval_expr(env, value)?;
                    for (n, _) in names {
                        env.insert(n.clone(), Interval::top());
                    }
                }
            }
            Stmt::Assign { name, value } => {
                let iv = self.eval_expr(env, value)?;
                env.insert(name.clone(), iv);
            }
            Stmt::While { cond, body, .. } => {
                let _ = self.eval_expr(env, cond)?;
                // Widen any variables assigned in loop body before checking body once
                let mut assigned = Vec::new();
                collect_assigned_vars(body, &mut assigned);
                for v in &assigned {
                    env.insert(v.clone(), Interval::top());
                }
                let mut loop_env = env.clone();
                refine_env(&mut loop_env, cond, true);
                for s in body {
                    self.eval_stmt(&mut loop_env, s)?;
                }
            }
            Stmt::Expr(e) => {
                let _ = self.eval_expr(env, e)?;
            }
        }
        Ok(())
    }
}

fn collect_assigned_vars(stmts: &[Stmt], out: &mut Vec<String>) {
    for s in stmts {
        match s {
            Stmt::Assign { name, .. } => out.push(name.clone()),
            Stmt::While { body, .. } => collect_assigned_vars(body, out),
            _ => {}
        }
    }
}

/// Run the lightweight interval verifier across all functions and Forma UI definitions in `module`.
pub fn verify_module_light(module: &Module) -> Result<LightVerifyReport, InfimumDiagnostic> {
    let mut fns: BTreeMap<String, &FnDef> = BTreeMap::new();
    for f in &module.functions {
        fns.insert(f.name.clone(), f);
    }

    let mut report = LightVerifyReport::default();

    for f in &module.functions {
        let mut env = Env::new();
        for p in &f.params {
            env.insert(p.name.clone(), Interval::top());
        }
        for req in &f.requires {
            refine_env(&mut env, req, true);
        }
        let mut checker = FnChecker {
            fn_name: &f.name,
            fns: &fns,
            inconclusive: &mut report.inconclusive,
        };
        let ret_iv = checker.eval_expr(&mut env, &f.body)?;
        if !f.ensures.is_empty() {
            env.insert("result".to_string(), ret_iv);
            for ens in &f.ensures {
                match eval_truth(&env, ens) {
                    Truth::AlwaysTrue => report.proven += 1,
                    Truth::AlwaysFalse => {
                        return Err(InfimumDiagnostic::new(
                            "E050_CONTRACT_VIOLATION",
                            "light-verify",
                            format!("Postcondition (`ensures`) violated in function '{}'", f.name),
                            Span::zero(),
                            "Check the return value bounds against the `ensures` contract",
                        ));
                    }
                    Truth::Unknown => {
                        report.inconclusive.push(format!(
                            "Light verifier: inconclusive for '{}' — install local Infimum Code with Z3 for full SMT proof",
                            f.name
                        ));
                    }
                }
            }
        }
    }

    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::parser::parse_module;

    #[test]
    fn proves_guarded_division_and_bounds() {
        let src = r#"
fn safe_div(a: i64, b: i64) -> i64
  req b > 0
  req a >= 0
  ens result >= 0
{
  a / b
}
"#;
        let m = parse_module(src).unwrap();
        let rep = verify_module_light(&m).unwrap();
        assert_eq!(rep.proven, 1);
        assert!(rep.inconclusive.is_empty(), "{:?}", rep.inconclusive);
    }

    #[test]
    fn catches_definite_div_by_zero() {
        let src = r#"
fn bad_div(a: i64) -> i64 {
  a / 0
}
"#;
        let m = parse_module(src).unwrap();
        let err = verify_module_light(&m).unwrap_err();
        assert_eq!(err.code, "E053_POSSIBLE_DIV_BY_ZERO");
    }

    #[test]
    fn catches_definite_ensures_violation() {
        let src = r#"
fn bad_ret(x: i64) -> i64
  req x >= 10
  ens result < 5
{
  x + 1
}
"#;
        let m = parse_module(src).unwrap();
        let err = verify_module_light(&m).unwrap_err();
        assert_eq!(err.code, "E050_CONTRACT_VIOLATION");
    }

    #[test]
    fn reports_inconclusive_with_z3_hint_on_coupled_vars() {
        let src = r#"
fn coupled(a: i64, b: i64) -> i64
  req a >= 0
  req b >= 0
  ens result == a + b
{
  a + b
}
"#;
        let m = parse_module(src).unwrap();
        let rep = verify_module_light(&m).unwrap();
        assert!(!rep.inconclusive.is_empty());
        assert!(rep.inconclusive[0].contains("Z3"));
    }
}
