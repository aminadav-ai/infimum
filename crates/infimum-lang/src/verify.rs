use crate::ast::{BinOp, Expr, FnDef, Param, Stmt, TargetRef, Type, UnaryOp};
use crate::canon::CanonicalNode;
use crate::diag::{InfimumDiagnostic, Span};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, BTreeSet};
use std::env;
use std::io::Write;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::sync::OnceLock;
#[cfg(not(target_arch = "wasm32"))]
use std::time::Instant;

pub const PROOF_CACHE_SCHEMA_VERSION: &str = "infimum-z3-v3";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct VerificationReport {
    pub alias: String,
    pub short_hash: String,
    pub obligations_checked: usize,
    pub elapsed_ms: u128,
    pub status: String,
    #[serde(default)]
    pub cached: bool,
}

#[derive(Debug, Clone)]
enum ObligationKind {
    Postcondition { index: usize, display: String },
    CallPrecondition { callee_desc: String, req_index: usize },
    NonZeroDivisor,
    FieldInvariant { lo: i64, hi: i64 },
}

#[derive(Debug, Clone)]
struct ProofObligation {
    guard_smt: String,
    goal_smt: String,
    kind: ObligationKind,
}

struct SmtBuilder<'a, F>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    self_node: &'a CanonicalNode,
    load_callee: &'a F,
    decls: Vec<String>,
    assumptions: Vec<String>,
    obligations: Vec<ProofObligation>,
    fresh_counter: usize,
}

impl<'a, F> SmtBuilder<'a, F>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    fn new(self_node: &'a CanonicalNode, load_callee: &'a F) -> Self {
        Self {
            self_node,
            load_callee,
            decls: Vec::new(),
            assumptions: Vec::new(),
            obligations: Vec::new(),
            fresh_counter: 0,
        }
    }

    fn fresh_sym(&mut self, prefix: &str, ty: &Type) -> String {
        let sym = format!("{prefix}_{}", self.fresh_counter);
        self.fresh_counter += 1;
        self.decls
            .push(format!("(declare-const {sym} {})", smt_sort(ty)));
        sym
    }

    fn infer_expr_type(&self, expr: &Expr, types: &BTreeMap<String, Type>) -> Type {
        match expr {
            Expr::IntLit(_) | Expr::FixedLit(_) | Expr::MemLoad { .. } => Type::I64,
            Expr::BoolLit(_) => Type::Bool,
            Expr::StrLit(_) => Type::Str,
            Expr::UnitLit | Expr::Perform { .. } | Expr::MemStore { .. } => Type::Unit,
            Expr::Var(name) => types.get(name).cloned().unwrap_or(Type::I64),
            Expr::SecretWrap(inner) | Expr::Reveal { expr: inner, .. } => {
                self.infer_expr_type(inner, types)
            }
            Expr::Unary { op, .. } => match op {
                UnaryOp::Not => Type::Bool,
                _ => Type::I64,
            },
            Expr::Binary { op, .. } => match op {
                BinOp::Eq
                | BinOp::Ne
                | BinOp::Lt
                | BinOp::Le
                | BinOp::Gt
                | BinOp::Ge
                | BinOp::And
                | BinOp::Or => Type::Bool,
                _ => Type::I64,
            },
            Expr::If { then_branch, .. } => self.infer_expr_type(then_branch, types),
            Expr::Block { tail, .. } => self.infer_expr_type(tail, types),
            Expr::Call { target, .. } => match target {
                TargetRef::SelfRec => self.self_node.ast.ret_type.clone(),
                TargetRef::Hash(h) => (self.load_callee)(h)
                    .map(|n| n.ast.ret_type)
                    .unwrap_or(Type::I64),
                TargetRef::Named(_) => Type::I64,
            },
            Expr::TupleLit(_) | Expr::TupleField { .. } => Type::I64,
        }
    }

    fn merge_branch_envs(
        &mut self,
        cond_smt: &str,
        env: &mut BTreeMap<String, String>,
        types: &BTreeMap<String, Type>,
        then_env: &BTreeMap<String, String>,
        else_env: &BTreeMap<String, String>,
    ) {
        let outer_keys: Vec<String> = env.keys().cloned().collect();
        for k in outer_keys {
            match (then_env.get(&k), else_env.get(&k)) {
                (Some(ts), Some(es)) if ts != es => {
                    let ty = types.get(&k).cloned().unwrap_or(Type::I64);
                    let phi = self.fresh_sym("phi", &ty);
                    self.assumptions
                        .push(format!("(assert (= {phi} (ite {cond_smt} {ts} {es})))"));
                    env.insert(k, phi);
                }
                (Some(ts), Some(_)) => {
                    env.insert(k, ts.clone());
                }
                _ => {
                    env.remove(&k);
                }
            }
        }
    }

    fn encode_expr(
        &mut self,
        expr: &Expr,
        env: &mut BTreeMap<String, String>,
        types: &mut BTreeMap<String, Type>,
        guard: &str,
        emit_checks: bool,
    ) -> Result<String, InfimumDiagnostic> {
        match expr {
            Expr::IntLit(v) | Expr::FixedLit(v) => {
                if *v < 0 {
                    Ok(format!("(- {})", v.unsigned_abs()))
                } else {
                    Ok(v.to_string())
                }
            }
            Expr::BoolLit(b) => Ok(if *b { "true" } else { "false" }.to_string()),
            Expr::StrLit(s) => Ok(format!("\"{}\"", smt_escape_str(s))),
            Expr::UnitLit => Ok("true".to_string()),
            Expr::Var(name) => env.get(name).cloned().ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E054_SMT_UNBOUND_VAR",
                    "verify",
                    format!("Unbound canonical symbol '{name}' during Z3 encoding"),
                    Span::zero(),
                    "Ensure all variables in contracts and body are bound parameters or locals",
                )
            }),
            Expr::SecretWrap(inner) | Expr::Reveal { expr: inner, .. } => {
                self.encode_expr(inner, env, types, guard, emit_checks)
            }
            Expr::Unary { op, expr: inner } => {
                let sub = self.encode_expr(inner, env, types, guard, emit_checks)?;
                match op {
                    UnaryOp::Neg => Ok(format!("(- {sub})")),
                    UnaryOp::Not => Ok(format!("(not {sub})")),
                    UnaryOp::IntToFixed => Ok(format!("(* {sub} 65536)")),
                    UnaryOp::FixedToInt => Ok(format!("(ifm_div {sub} 65536)")),
                    UnaryOp::FixedToRaw | UnaryOp::RawToFixed => Ok(sub),
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.encode_expr(lhs, env, types, guard, emit_checks)?;
                match op {
                    BinOp::And => {
                        let rhs_guard = conjoin_guard(guard, &l);
                        let pre_env = env.clone();
                        let mut rhs_env = env.clone();
                        let mut rhs_types = types.clone();
                        let r = self.encode_expr(
                            rhs,
                            &mut rhs_env,
                            &mut rhs_types,
                            &rhs_guard,
                            emit_checks,
                        )?;
                        self.merge_branch_envs(&l, env, types, &rhs_env, &pre_env);
                        Ok(format!("(and {l} {r})"))
                    }
                    BinOp::Or => {
                        let not_l = format!("(not {l})");
                        let rhs_guard = conjoin_guard(guard, &not_l);
                        let pre_env = env.clone();
                        let mut rhs_env = env.clone();
                        let mut rhs_types = types.clone();
                        let r = self.encode_expr(
                            rhs,
                            &mut rhs_env,
                            &mut rhs_types,
                            &rhs_guard,
                            emit_checks,
                        )?;
                        self.merge_branch_envs(&l, env, types, &pre_env, &rhs_env);
                        Ok(format!("(or {l} {r})"))
                    }
                    BinOp::FixedMul => {
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        Ok(format!("(ifm_div (* {l} {r}) 65536)"))
                    }
                    BinOp::FixedDiv => {
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        if emit_checks {
                            self.obligations.push(ProofObligation {
                                guard_smt: guard.to_string(),
                                goal_smt: format!("(distinct {r} 0)"),
                                kind: ObligationKind::NonZeroDivisor,
                            });
                        }
                        Ok(format!("(ifm_div (* {l} 65536) {r})"))
                    }
                    BinOp::Div | BinOp::Mod => {
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        if emit_checks {
                            self.obligations.push(ProofObligation {
                                guard_smt: guard.to_string(),
                                goal_smt: format!("(distinct {r} 0)"),
                                kind: ObligationKind::NonZeroDivisor,
                            });
                        }
                        let fn_name = if matches!(op, BinOp::Div) {
                            "ifm_div"
                        } else {
                            "ifm_mod"
                        };
                        Ok(format!("({fn_name} {l} {r})"))
                    }
                    BinOp::Shl => {
                        if let Expr::IntLit(k) = rhs.as_ref() {
                            if (0..=30).contains(k) {
                                let factor = 1_i64 << (*k as u32);
                                return Ok(format!("(* {l} {factor})"));
                            }
                        }
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        Ok(format!("(ifm_shl {l} {r})"))
                    }
                    BinOp::Shr => {
                        if let Expr::IntLit(k) = rhs.as_ref() {
                            if (0..=30).contains(k) {
                                let factor = 1_i64 << (*k as u32);
                                return Ok(format!("(div {l} {factor})"));
                            }
                        }
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        Ok(format!("(ifm_shr {l} {r})"))
                    }
                    BinOp::BitAnd | BinOp::BitOr | BinOp::BitXor => {
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        let fn_name = match op {
                            BinOp::BitAnd => "ifm_band",
                            BinOp::BitOr => "ifm_bor",
                            BinOp::BitXor => "ifm_bxor",
                            _ => unreachable!(),
                        };
                        Ok(format!("({fn_name} {l} {r})"))
                    }
                    other => {
                        let r = self.encode_expr(rhs, env, types, guard, emit_checks)?;
                        let smt_op = match other {
                            BinOp::Add => "+",
                            BinOp::Sub => "-",
                            BinOp::Mul => "*",
                            BinOp::Eq => "=",
                            BinOp::Ne => "distinct",
                            BinOp::Lt => "<",
                            BinOp::Le => "<=",
                            BinOp::Gt => ">",
                            BinOp::Ge => ">=",
                            BinOp::Div
                            | BinOp::FixedMul
                            | BinOp::FixedDiv
                            | BinOp::Mod
                            | BinOp::And
                            | BinOp::Or
                            | BinOp::BitAnd
                            | BinOp::BitOr
                            | BinOp::BitXor
                            | BinOp::Shl
                            | BinOp::Shr => unreachable!(),
                        };
                        Ok(format!("({smt_op} {l} {r})"))
                    }
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.encode_expr(cond, env, types, guard, emit_checks)?;
                let then_guard = conjoin_guard(guard, &c);
                let not_c = format!("(not {c})");
                let else_guard = conjoin_guard(guard, &not_c);
                let mut then_env = env.clone();
                let mut then_types = types.clone();
                let t = self.encode_expr(
                    then_branch,
                    &mut then_env,
                    &mut then_types,
                    &then_guard,
                    emit_checks,
                )?;
                let mut else_env = env.clone();
                let mut else_types = types.clone();
                let e = self.encode_expr(
                    else_branch,
                    &mut else_env,
                    &mut else_types,
                    &else_guard,
                    emit_checks,
                )?;
                self.merge_branch_envs(&c, env, types, &then_env, &else_env);
                Ok(format!("(ite {c} {t} {e})"))
            }
            Expr::Perform { args, .. } => {
                for a in args {
                    let _ = self.encode_expr(a, env, types, guard, emit_checks)?;
                }
                Ok("true".to_string())
            }
            Expr::MemLoad { index, inv, .. } => {
                let _ = self.encode_expr(index, env, types, guard, emit_checks)?;
                let sym = self.fresh_sym("mem_rd", &Type::I64);
                if let Some((lo, hi)) = inv {
                    let lo_s = if *lo < 0 { format!("(- {})", -(*lo as i128)) } else { lo.to_string() };
                    let hi_s = if *hi < 0 { format!("(- {})", -(*hi as i128)) } else { hi.to_string() };
                    self.assumptions.push(format!("(assert (and (>= {sym} {lo_s}) (<= {sym} {hi_s})))"));
                }
                Ok(sym)
            }
            Expr::MemStore { index, value, inv, .. } => {
                let _ = self.encode_expr(index, env, types, guard, emit_checks)?;
                let v = self.encode_expr(value, env, types, guard, emit_checks)?;
                if emit_checks {
                    if let Some((lo, hi)) = inv {
                        let lo_s = if *lo < 0 { format!("(- {})", -(*lo as i128)) } else { lo.to_string() };
                        let hi_s = if *hi < 0 { format!("(- {})", -(*hi as i128)) } else { hi.to_string() };
                        let goal = format!("(and (>= {v} {lo_s}) (<= {v} {hi_s}))");
                        self.obligations.push(ProofObligation {
                            guard_smt: guard.to_string(),
                            goal_smt: goal,
                            kind: ObligationKind::FieldInvariant { lo: *lo, hi: *hi },
                        });
                    }
                }
                Ok("true".to_string())
            }
            Expr::Call { target, args } => {
                let mut encoded_args = Vec::with_capacity(args.len());
                for a in args {
                    encoded_args.push(self.encode_expr(a, env, types, guard, emit_checks)?);
                }

                let callee_node: CanonicalNode;
                let (callee_ast, callee_label): (&FnDef, String) = match target {
                    TargetRef::SelfRec => (&self.self_node.ast, "$self".to_string()),
                    TargetRef::Hash(h) => {
                        callee_node = (self.load_callee)(h)?;
                        let short = callee_node.short_hash.clone();
                        (&callee_node.ast, format!("#{short}"))
                    }
                    TargetRef::Named(n) => {
                        return Err(InfimumDiagnostic::new(
                            "E055_UNRESOLVED_CALL",
                            "verify",
                            format!("Unresolved symbolic call '{n}' reached Z3 verifier"),
                            Span::zero(),
                            "Canonicalize function references to hashes before verification",
                        ));
                    }
                };

                if encoded_args.len() != callee_ast.params.len() {
                    return Err(InfimumDiagnostic::new(
                        "E056_ARITY_MISMATCH",
                        "verify",
                        format!(
                            "Call to {callee_label} passed {} args, expected {}",
                            encoded_args.len(),
                            callee_ast.params.len()
                        ),
                        Span::zero(),
                        "Pass the exact number of arguments declared by the callee signature",
                    ));
                }

                let mut callee_env = BTreeMap::new();
                let mut callee_types = BTreeMap::new();
                for (i, arg_smt) in encoded_args.iter().enumerate() {
                    callee_env.insert(format!("p{i}"), arg_smt.clone());
                    callee_types.insert(format!("p{i}"), callee_ast.params[i].ty.clone());
                    if let Expr::TupleLit(elems) = &args[i] {
                        for (fi, elem) in elems.iter().enumerate() {
                            let fty = self.infer_expr_type(elem, types);
                            let mut tmp_env = env.clone();
                            let mut tmp_types = types.clone();
                            let esmt = self.encode_expr(elem, &mut tmp_env, &mut tmp_types, guard, false)?;
                            callee_env.insert(format!("p{i}#{fi}"), esmt);
                            callee_types.insert(format!("p{i}#{fi}"), fty);
                        }
                    } else if let Expr::Var(src) = &args[i] {
                        let prefix = format!("{src}#");
                        for (k, v) in env.iter() {
                            if let Some(fi) = k.strip_prefix(&prefix) {
                                callee_env.insert(format!("p{i}#{fi}"), v.clone());
                                if let Some(fty) = types.get(k) {
                                    callee_types.insert(format!("p{i}#{fi}"), fty.clone());
                                }
                            }
                        }
                    }
                }

                // 1. Modular call-site obligation: prove callee's preconditions hold under `guard`!
                if emit_checks {
                    for (req_idx, req_expr) in callee_ast.requires.iter().enumerate() {
                        let mut req_env = callee_env.clone();
                        let mut req_types = callee_types.clone();
                        let req_smt = self.encode_expr(req_expr, &mut req_env, &mut req_types, guard, false)?;
                        self.obligations.push(ProofObligation {
                            guard_smt: guard.to_string(),
                            goal_smt: req_smt,
                            kind: ObligationKind::CallPrecondition {
                                callee_desc: callee_label.clone(),
                                req_index: req_idx + 1,
                            },
                        });
                    }
                }

                // 2. Introduce fresh result symbol & assume callee's postconditions (or pure body if no ens)
                let call_sym = self.fresh_sym("call_ret", &callee_ast.ret_type);
                if !callee_ast.ensures.is_empty() {
                    callee_env.insert("$result".to_string(), call_sym.clone());
                    callee_types.insert("$result".to_string(), callee_ast.ret_type.clone());
                    for ens_expr in &callee_ast.ensures {
                        let mut ens_env = callee_env.clone();
                        let mut ens_types = callee_types.clone();
                        let ens_smt = self.encode_expr(ens_expr, &mut ens_env, &mut ens_types, guard, false)?;
                        self.assumptions
                            .push(format!("(assert (=> {guard} {ens_smt}))"));
                    }
                } else if !matches!(target, TargetRef::SelfRec) && callee_ast.effects.is_empty() {
                    // Pure helper without explicit `ens`: inline its mathematical summary transparently
                    let mut body_env = callee_env.clone();
                    let mut body_types = callee_types.clone();
                    let body_smt = self.encode_expr(&callee_ast.body, &mut body_env, &mut body_types, guard, false)?;
                    self.assumptions
                        .push(format!("(assert (=> {guard} (= {call_sym} {body_smt})))"));
                }

                Ok(call_sym)
            }
            Expr::Block { stmts, tail } => {
                let outer_keys: Vec<String> = env.keys().cloned().collect();
                let mut local_env = env.clone();
                let mut local_types = types.clone();
                let mut declared = BTreeSet::new();
                self.encode_stmt_slice(
                    stmts,
                    &mut local_env,
                    &mut local_types,
                    Some(&mut declared),
                    guard,
                    emit_checks,
                )?;
                let res = self.encode_expr(tail, &mut local_env, &mut local_types, guard, emit_checks)?;
                for k in outer_keys {
                    let base = k.split('#').next().unwrap_or(&k);
                    if declared.contains(base) {
                        continue;
                    }
                    if let Some(v) = local_env.get(&k) {
                        env.insert(k, v.clone());
                    } else {
                        env.remove(&k);
                    }
                }
                Ok(res)
            }
            Expr::TupleLit(elems) => {
                for e in elems {
                    let _ = self.encode_expr(e, env, types, guard, emit_checks)?;
                }
                Ok(self.fresh_sym("tup", &Type::I64))
            }
            Expr::TupleField { expr, index } => {
                if let Expr::Var(vname) = expr.as_ref() {
                    if let Some(fsym) = env.get(&format!("{vname}#{index}")) {
                        return Ok(fsym.clone());
                    }
                }
                if let Expr::TupleLit(elems) = expr.as_ref() {
                    if let Some(target) = elems.get(*index) {
                        for (i, e) in elems.iter().enumerate() {
                            if i != *index {
                                let _ = self.encode_expr(e, env, types, guard, emit_checks)?;
                            }
                        }
                        return self.encode_expr(target, env, types, guard, emit_checks);
                    }
                }
                let _ = self.encode_expr(expr, env, types, guard, emit_checks)?;
                Ok(self.fresh_sym("tup_fld", &Type::I64))
            }
        }
    }

    fn encode_stmt_slice(
        &mut self,
        stmts: &[Stmt],
        local_env: &mut BTreeMap<String, String>,
        local_types: &mut BTreeMap<String, Type>,
        mut declared: Option<&mut BTreeSet<String>>,
        guard: &str,
        emit_checks: bool,
    ) -> Result<(), InfimumDiagnostic> {
        for stmt in stmts {
            match stmt {
                Stmt::Let {
                    name, ty, value, ..
                } => {
                    if let Some(d) = declared.as_deref_mut() {
                        d.insert(name.clone());
                    }
                    let inferred_ty = ty
                        .clone()
                        .unwrap_or_else(|| self.infer_expr_type(value, local_types));
                    if let Expr::TupleLit(elems) = value {
                        let mut fsyms = Vec::with_capacity(elems.len());
                        let mut ftys = Vec::with_capacity(elems.len());
                        for e in elems {
                            ftys.push(self.infer_expr_type(e, local_types));
                            fsyms.push(self.encode_expr(e, local_env, local_types, guard, emit_checks)?);
                        }
                        for (i, (fsym, fty)) in fsyms.into_iter().zip(ftys.into_iter()).enumerate() {
                            local_env.insert(format!("{name}#{i}"), fsym);
                            local_types.insert(format!("{name}#{i}"), fty);
                        }
                        let v_smt = self.fresh_sym("tup", &Type::I64);
                        local_env.insert(name.clone(), v_smt);
                    } else {
                        let v_smt = self.encode_expr(value, local_env, local_types, guard, emit_checks)?;
                        local_env.insert(name.clone(), v_smt);
                        if let Expr::Var(src) = value {
                            let prefix = format!("{src}#");
                            let copied: Vec<(String, String, Option<Type>)> = local_env
                                .iter()
                                .filter_map(|(k, v)| {
                                    k.strip_prefix(&prefix).map(|idx| {
                                        (format!("{name}#{idx}"), v.clone(), local_types.get(k).cloned())
                                    })
                                })
                                .collect();
                            for (k, v, fty) in copied {
                                local_env.insert(k.clone(), v);
                                if let Some(t) = fty {
                                    local_types.insert(k, t);
                                }
                            }
                        }
                    }
                    local_types.insert(name.clone(), inferred_ty);
                }
                Stmt::LetTuple { names, value } => {
                    if let Some(d) = declared.as_deref_mut() {
                        for (n, _) in names {
                            d.insert(n.clone());
                        }
                    }
                    if let Expr::TupleLit(elems) = value {
                        for ((n, _), e) in names.iter().zip(elems.iter()) {
                            let inferred = self.infer_expr_type(e, local_types);
                            let v_smt = self.encode_expr(e, local_env, local_types, guard, emit_checks)?;
                            local_env.insert(n.clone(), v_smt);
                            local_types.insert(n.clone(), inferred);
                        }
                    } else {
                        let _ = self.encode_expr(value, local_env, local_types, guard, emit_checks)?;
                        for (n, _) in names {
                            let fresh = self.fresh_sym("tup_el", &Type::I64);
                            local_env.insert(n.clone(), fresh);
                            local_types.insert(n.clone(), Type::I64);
                        }
                    }
                }
                Stmt::Assign { name, value } => {
                    if let Expr::TupleLit(elems) = value {
                        let mut fsyms = Vec::with_capacity(elems.len());
                        let mut ftys = Vec::with_capacity(elems.len());
                        for e in elems {
                            ftys.push(self.infer_expr_type(e, local_types));
                            fsyms.push(self.encode_expr(e, local_env, local_types, guard, emit_checks)?);
                        }
                        for (i, (fsym, fty)) in fsyms.into_iter().zip(ftys.into_iter()).enumerate() {
                            local_env.insert(format!("{name}#{i}"), fsym);
                            local_types.insert(format!("{name}#{i}"), fty);
                        }
                        let v_smt = self.fresh_sym("tup", &Type::I64);
                        local_env.insert(name.clone(), v_smt);
                    } else {
                        let v_smt = self.encode_expr(value, local_env, local_types, guard, emit_checks)?;
                        local_env.insert(name.clone(), v_smt);
                        let prefix = format!("{name}#");
                        let stale: Vec<String> = local_env
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();
                        for k in stale {
                            local_env.remove(&k);
                            local_types.remove(&k);
                        }
                    }
                }
                Stmt::While { cond, body, .. } => {
                    // Sound loop abstraction: havoc all variables mutated anywhere inside the loop (including inside if/block)
                    let mut mutated = BTreeSet::new();
                    collect_mutated_vars_expr(cond, &mut mutated);
                    collect_mutated_vars(body, &mut mutated);
                    for mvar in mutated {
                        let mty = local_types.get(&mvar).cloned().unwrap_or(Type::I64);
                        let fresh = self.fresh_sym("loop_havoc", &mty);
                        local_env.insert(mvar.clone(), fresh);
                        let prefix = format!("{mvar}#");
                        let fkeys: Vec<String> = local_env
                            .keys()
                            .filter(|k| k.starts_with(&prefix))
                            .cloned()
                            .collect();
                        for fk in fkeys {
                            let ffresh = self.fresh_sym("loop_havoc_f", &Type::I64);
                            local_env.insert(fk, ffresh);
                        }
                    }
                    let c_smt = self.encode_expr(cond, local_env, local_types, guard, emit_checks)?;
                    let body_guard = conjoin_guard(guard, &c_smt);
                    let mut inner_env = local_env.clone();
                    let mut inner_types = local_types.clone();
                    self.encode_stmt_slice(
                        body,
                        &mut inner_env,
                        &mut inner_types,
                        None,
                        &body_guard,
                        emit_checks,
                    )?;
                }
                Stmt::Expr(e) => {
                    let _ = self.encode_expr(e, local_env, local_types, guard, emit_checks)?;
                }
            }
        }
        Ok(())
    }
}

fn collect_mutated_vars_expr(expr: &Expr, out: &mut BTreeSet<String>) {
    match expr {
        Expr::IntLit(_)
        | Expr::FixedLit(_)
        | Expr::BoolLit(_)
        | Expr::StrLit(_)
        | Expr::UnitLit
        | Expr::Var(_) => {}
        Expr::Unary { expr: inner, .. }
        | Expr::SecretWrap(inner)
        | Expr::Reveal { expr: inner, .. }
        | Expr::MemLoad { index: inner, .. }
        | Expr::TupleField { expr: inner, .. } => collect_mutated_vars_expr(inner, out),
        Expr::Binary { lhs, rhs, .. } => {
            collect_mutated_vars_expr(lhs, out);
            collect_mutated_vars_expr(rhs, out);
        }
        Expr::MemStore { index, value, .. } => {
            collect_mutated_vars_expr(index, out);
            collect_mutated_vars_expr(value, out);
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            collect_mutated_vars_expr(cond, out);
            collect_mutated_vars_expr(then_branch, out);
            collect_mutated_vars_expr(else_branch, out);
        }
        Expr::Call { args, .. } | Expr::Perform { args, .. } | Expr::TupleLit(args) => {
            for a in args {
                collect_mutated_vars_expr(a, out);
            }
        }
        Expr::Block { stmts, tail } => {
            collect_mutated_vars(stmts, out);
            collect_mutated_vars_expr(tail, out);
        }
    }
}

fn collect_mutated_vars(stmts: &[Stmt], out: &mut BTreeSet<String>) {
    for s in stmts {
        match s {
            Stmt::Assign { name, value } => {
                out.insert(name.clone());
                collect_mutated_vars_expr(value, out);
            }
            Stmt::Let { value, .. } | Stmt::LetTuple { value, .. } => {
                collect_mutated_vars_expr(value, out);
            }
            Stmt::While { cond, body, .. } => {
                collect_mutated_vars_expr(cond, out);
                collect_mutated_vars(body, out);
            }
            Stmt::Expr(e) => {
                collect_mutated_vars_expr(e, out);
            }
        }
    }
}

pub fn verify_node<F>(
    raw_fn: &FnDef,
    canon_node: &CanonicalNode,
    load_callee: &F,
) -> Result<VerificationReport, InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    let z3_bin = find_z3_binary()?;
    verify_node_with_runner(raw_fn, canon_node, load_callee, &mut |script| {
        run_z3(&z3_bin, script)
    })
}

/// Platform-independent Z3 verifier entry point (used by both native CLI and `wasm32` `z3.wasm`).
pub fn verify_node_with_runner<F, R>(
    raw_fn: &FnDef,
    canon_node: &CanonicalNode,
    load_callee: &F,
    run_smt: &mut R,
) -> Result<VerificationReport, InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
    R: FnMut(&str) -> Result<String, InfimumDiagnostic>,
{
    #[cfg(not(target_arch = "wasm32"))]
    let start = Instant::now();
    let canon_fn = &canon_node.ast;

    let mut base_env = BTreeMap::new();
    let mut base_types = BTreeMap::new();
    let mut param_decls = Vec::with_capacity(canon_fn.params.len());
    for (i, p) in canon_fn.params.iter().enumerate() {
        let cname = format!("p{i}");
        param_decls.push(format!("(declare-const {cname} {})", smt_sort(&p.ty)));
        base_env.insert(cname.clone(), cname.clone());
        base_types.insert(cname.clone(), p.ty.clone());
        if let Type::Tuple(ts) = &p.ty {
            for (fi, fty) in ts.iter().enumerate() {
                let fsym = format!("p{i}_f{fi}");
                param_decls.push(format!("(declare-const {fsym} {})", smt_sort(fty)));
                base_env.insert(format!("{cname}#{fi}"), fsym);
                base_types.insert(format!("{cname}#{fi}"), fty.clone());
            }
        }
    }

    let mut builder = SmtBuilder::new(canon_node, load_callee);

    // Encode preconditions
    let mut req_smts = Vec::with_capacity(canon_fn.requires.len());
    for req in &canon_fn.requires {
        let mut req_env = base_env.clone();
        let mut req_types = base_types.clone();
        req_smts.push(builder.encode_expr(req, &mut req_env, &mut req_types, "true", false)?);
    }

    // 1. Vacuous precondition guard: ensure `req` clauses are mutually satisfiable!
    if !req_smts.is_empty() {
        let mut vac_script = smt_prelude(&param_decls, &builder.decls);
        for r in &req_smts {
            vac_script.push_str(&format!("(assert {r})\n"));
        }
        vac_script.push_str("(check-sat)\n");
        let vac_out = run_smt(&vac_script)?;
        if vac_out.lines().next().map(str::trim) == Some("unsat") {
            return Err(InfimumDiagnostic::new(
                "E052_VACUOUS_PRECONDITION",
                "verify",
                format!(
                    "Preconditions ('req') of function '{}' are contradictory (never satisfiable)",
                    raw_fn.name
                ),
                Span::zero(),
                "Remove contradictory 'req' constraints; vacuous contracts are rejected to prevent trivial proof cheating",
            ));
        }
    }

    // Encode function body -> $result
    let mut body_env = base_env.clone();
    let mut body_types = base_types.clone();
    let body_smt = builder.encode_expr(&canon_fn.body, &mut body_env, &mut body_types, "true", true)?;
    let result_sym = "$result";

    // Encode postconditions (`ens`) as proof obligations
    let mut ens_env = base_env.clone();
    let mut ens_types = base_types.clone();
    ens_env.insert("$result".to_string(), result_sym.to_string());
    ens_types.insert("$result".to_string(), canon_fn.ret_type.clone());
    for (i, (canon_ens, raw_ens)) in canon_fn
        .ensures
        .iter()
        .zip(raw_fn.ensures.iter())
        .enumerate()
    {
        let mut cur_ens_env = ens_env.clone();
        let mut cur_ens_types = ens_types.clone();
        let ens_smt = builder.encode_expr(canon_ens, &mut cur_ens_env, &mut cur_ens_types, "true", false)?;
        let pretty_raw = crate::canon::format_expr_human(raw_ens);
        builder.obligations.push(ProofObligation {
            guard_smt: "true".to_string(),
            goal_smt: ens_smt,
            kind: ObligationKind::Postcondition {
                index: i + 1,
                display: pretty_raw,
            },
        });
    }

    let total_obligations = builder.obligations.len();
    if total_obligations == 0 {
        #[cfg(not(target_arch = "wasm32"))]
        let elapsed_ms = start.elapsed().as_millis();
        #[cfg(target_arch = "wasm32")]
        let elapsed_ms = 0u128;
        return Ok(VerificationReport {
            alias: raw_fn.name.clone(),
            short_hash: canon_node.short_hash.clone(),
            obligations_checked: 0,
            elapsed_ms,
            status: "verified (0 obligations)".to_string(),
            cached: false,
        });
    }

    // Build base SMT-LIB2 script shared across obligations
    let mut base_script = smt_prelude(&param_decls, &builder.decls);
    base_script.push_str(&format!(
        "(declare-const {result_sym} {})\n",
        smt_sort(&canon_fn.ret_type)
    ));
    for r in &req_smts {
        base_script.push_str(&format!("(assert {r})\n"));
    }
    for a in &builder.assumptions {
        base_script.push_str(a);
        base_script.push('\n');
    }
    base_script.push_str(&format!("(assert (= {result_sym} {body_smt}))\n"));

    let mut query_vars: Vec<String> = (0..canon_fn.params.len())
        .map(|i| format!("p{i}"))
        .collect();
    query_vars.push(result_sym.to_string());
    let get_val_cmd = format!("(get-value ({}))\n", query_vars.join(" "));

    for ob in &builder.obligations {
        let mut script = base_script.clone();
        script.push_str(&format!(
            "(assert (and {} (not {})))\n(check-sat)\n",
            ob.guard_smt, ob.goal_smt
        ));

        let out = run_smt(&script)?;
        let first_line = out.lines().next().unwrap_or("").trim();

        match first_line {
            "unsat" => {
                // Obligation mathematically proved for ALL inputs!
            }
            "sat" => {
                script.push_str(&get_val_cmd);
                let cex_out = run_smt(&script).unwrap_or(out);
                let cex = extract_counterexample(&cex_out, &raw_fn.params);
                return Err(build_violation_diagnostic(&raw_fn.name, &ob.kind, cex));
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E057_SOLVER_INCONCLUSIVE",
                    "verify",
                    format!(
                        "Z3 returned '{other}' while verifying '{}' (non-linear or timeout)",
                        raw_fn.name
                    ),
                    Span::zero(),
                    "Simplify non-linear arithmetic in contract or split into smaller verified lemmas",
                ));
            }
        }
    }

    #[cfg(not(target_arch = "wasm32"))]
    let elapsed_ms = start.elapsed().as_millis();
    #[cfg(target_arch = "wasm32")]
    let elapsed_ms = 0u128;
    Ok(VerificationReport {
        alias: raw_fn.name.clone(),
        short_hash: canon_node.short_hash.clone(),
        obligations_checked: total_obligations,
        elapsed_ms,
        status: format!("proved ({total_obligations} obligations)"),
        cached: false,
    })
}

fn build_violation_diagnostic(
    fn_name: &str,
    kind: &ObligationKind,
    cex: BTreeMap<String, String>,
) -> InfimumDiagnostic {
    match kind {
        ObligationKind::Postcondition { index, display } => InfimumDiagnostic::new(
            "E050_CONTRACT_VIOLATION",
            "verify",
            format!(
                "Postcondition #{index} failed for function '{fn_name}': ens {display}"
            ),
            Span::zero(),
            format!(
                "Stubs and partial implementations are rejected. Update '{fn_name}' body so 'ens {display}' holds for the counterexample inputs"
            ),
        )
        .with_counterexample(cex),
        ObligationKind::CallPrecondition {
            callee_desc,
            req_index,
        } => InfimumDiagnostic::new(
            "E051_CALL_PRECONDITION_VIOLATED",
            "verify",
            format!(
                "Function '{fn_name}' may call {callee_desc} violating its precondition #{req_index}"
            ),
            Span::zero(),
            format!(
                "Guard the call to {callee_desc} with an 'if' check or 'clamp_i64(...)', or strengthen '{fn_name}' preconditions ('req'). Note: if a value comes from linear memory (e.g. AppMem.x), bind it to a local 'let v = AppMem.x;' before checking its range because Z3 treats separate memory loads as independent"
            ),
        )
        .with_counterexample(cex),
        ObligationKind::NonZeroDivisor => InfimumDiagnostic::new(
            "E053_POSSIBLE_DIV_BY_ZERO",
            "verify",
            format!("Possible division or modulo by zero in function '{fn_name}'"),
            Span::zero(),
            "Ensure divisor is provably non-zero via 'req divisor != 0', 'clamp_i64(d, 1, ...)', or an explicit 'if d != 0' branch (bind layout fields to a local 'let d = AppMem.x;' before checking)",
        )
        .with_counterexample(cex),
        ObligationKind::FieldInvariant { lo, hi } => InfimumDiagnostic::new(
            "E054_FIELD_INVARIANT_VIOLATED",
            "verify",
            format!("Write in function '{fn_name}' may violate field invariant 'inv {lo}..={hi}'"),
            Span::zero(),
            format!("Ensure the stored value is provably in [{lo}..={hi}] (e.g. via 'clamp_i64(v, {lo}, {hi})' or an 'if' guard)"),
        )
        .with_counterexample(cex),
    }
}

fn smt_prelude(param_decls: &[String], extra_decls: &[String]) -> String {
    let mut s = String::new();
    s.push_str("(set-option :produce-models true)\n");
    // Truncated-toward-zero integer division & remainder matching Rust `i64` `/` and `%`
    s.push_str(
        "(define-fun ifm_div ((a Int) (b Int)) Int (ite (>= a 0) (ite (>= b 0) (div a b) (- (div a (- b)))) (ite (>= b 0) (- (div (- a) b)) (div (- a) (- b)))))\n",
    );
    s.push_str("(define-fun ifm_mod ((a Int) (b Int)) Int (- a (* (ifm_div a b) b)))\n");
    s.push_str("(declare-fun ifm_band (Int Int) Int)\n");
    s.push_str("(declare-fun ifm_bor (Int Int) Int)\n");
    s.push_str("(declare-fun ifm_bxor (Int Int) Int)\n");
    s.push_str("(declare-fun ifm_shl (Int Int) Int)\n");
    s.push_str("(declare-fun ifm_shr (Int Int) Int)\n");
    for d in param_decls {
        s.push_str(d);
        s.push('\n');
    }
    for d in extra_decls {
        s.push_str(d);
        s.push('\n');
    }
    s
}

fn smt_sort(ty: &Type) -> &'static str {
    match ty {
        Type::I64 | Type::Fixed => "Int",
        Type::Bool | Type::Unit => "Bool",
        Type::Str => "String",
        Type::Secret(inner) => smt_sort(inner),
        Type::Tuple(_) => "Int",
    }
}

fn smt_escape_str(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for ch in s.chars() {
        if ch == '"' {
            out.push_str("\"\"");
        } else {
            out.push(ch);
        }
    }
    out
}

fn conjoin_guard(old_guard: &str, cond: &str) -> String {
    if old_guard == "true" {
        cond.to_string()
    } else {
        format!("(and {old_guard} {cond})")
    }
}

/// Return the Z3 solver binary version string, computed once per process via `z3 --version`.
pub fn z3_version_id() -> Result<String, InfimumDiagnostic> {
    static Z3_VERSION: OnceLock<String> = OnceLock::new();
    if let Some(cached) = Z3_VERSION.get() {
        return Ok(cached.clone());
    }
    let z3_bin = find_z3_binary()?;
    let output = Command::new(&z3_bin)
        .arg("--version")
        .output()
        .map_err(|e| {
            InfimumDiagnostic::new(
                "E058_Z3_EXEC_ERROR",
                "verify",
                format!("Failed to query Z3 version: {e}"),
                Span::zero(),
                "Verify Z3 binary permissions",
            )
        })?;
    let ver = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let ver = if ver.is_empty() {
        z3_bin.display().to_string()
    } else {
        ver
    };
    let _ = Z3_VERSION.set(ver.clone());
    Ok(ver)
}

/// Compute a deterministic SHA-256 hex digest (FIPS 180-4, zero external dependencies).
pub fn sha256_hex(data: &[u8]) -> String {
    const K: [u32; 64] = [
        0x428a2f98, 0x71374491, 0xb5c0fbcf, 0xe9b5dba5, 0x3956c25b, 0x59f111f1, 0x923f82a4,
        0xab1c5ed5, 0xd807aa98, 0x12835b01, 0x243185be, 0x550c7dc3, 0x72be5d74, 0x80deb1fe,
        0x9bdc06a7, 0xc19bf174, 0xe49b69c1, 0xefbe4786, 0x0fc19dc6, 0x240ca1cc, 0x2de92c6f,
        0x4a7484aa, 0x5cb0a9dc, 0x76f988da, 0x983e5152, 0xa831c66d, 0xb00327c8, 0xbf597fc7,
        0xc6e00bf3, 0xd5a79147, 0x06ca6351, 0x14292967, 0x27b70a85, 0x2e1b2138, 0x4d2c6dfc,
        0x53380d13, 0x650a7354, 0x766a0abb, 0x81c2c92e, 0x92722c85, 0xa2bfe8a1, 0xa81a664b,
        0xc24b8b70, 0xc76c51a3, 0xd192e819, 0xd6990624, 0xf40e3585, 0x106aa070, 0x19a4c116,
        0x1e376c08, 0x2748774c, 0x34b0bcb5, 0x391c0cb3, 0x4ed8aa4a, 0x5b9cca4f, 0x682e6ff3,
        0x748f82ee, 0x78a5636f, 0x84c87814, 0x8cc70208, 0x90befffa, 0xa4506ceb, 0xbef9a3f7,
        0xc67178f2,
    ];

    let mut h: [u32; 8] = [
        0x6a09e667, 0xbb67ae85, 0x3c6ef372, 0xa54ff53a, 0x510e527f, 0x9b05688c, 0x1f83d9ab,
        0x5be0cd19,
    ];

    let bit_len = (data.len() as u64).wrapping_mul(8);
    let mut padded = Vec::with_capacity((data.len() + 9).div_ceil(64) * 64);
    padded.extend_from_slice(data);
    padded.push(0x80);
    while (padded.len() % 64) != 56 {
        padded.push(0);
    }
    padded.extend_from_slice(&bit_len.to_be_bytes());

    let (chunks, _) = padded.as_chunks::<64>();
    for chunk in chunks {
        let mut w = [0u32; 64];
        for (i, word) in w.iter_mut().take(16).enumerate() {
            let off = i * 4;
            *word = u32::from_be_bytes([
                chunk[off],
                chunk[off + 1],
                chunk[off + 2],
                chunk[off + 3],
            ]);
        }
        for i in 16..64 {
            let s0 = w[i - 15].rotate_right(7) ^ w[i - 15].rotate_right(18) ^ (w[i - 15] >> 3);
            let s1 = w[i - 2].rotate_right(17) ^ w[i - 2].rotate_right(19) ^ (w[i - 2] >> 10);
            w[i] = w[i - 16]
                .wrapping_add(s0)
                .wrapping_add(w[i - 7])
                .wrapping_add(s1);
        }

        let [mut a, mut b, mut c, mut d, mut e, mut f, mut g, mut hh] = h;
        for i in 0..64 {
            let s1 = e.rotate_right(6) ^ e.rotate_right(11) ^ e.rotate_right(25);
            let ch = (e & f) ^ ((!e) & g);
            let temp1 = hh
                .wrapping_add(s1)
                .wrapping_add(ch)
                .wrapping_add(K[i])
                .wrapping_add(w[i]);
            let s0 = a.rotate_right(2) ^ a.rotate_right(13) ^ a.rotate_right(22);
            let maj = (a & b) ^ (a & c) ^ (b & c);
            let temp2 = s0.wrapping_add(maj);

            hh = g;
            g = f;
            f = e;
            e = d.wrapping_add(temp1);
            d = c;
            c = b;
            b = a;
            a = temp1.wrapping_add(temp2);
        }

        h[0] = h[0].wrapping_add(a);
        h[1] = h[1].wrapping_add(b);
        h[2] = h[2].wrapping_add(c);
        h[3] = h[3].wrapping_add(d);
        h[4] = h[4].wrapping_add(e);
        h[5] = h[5].wrapping_add(f);
        h[6] = h[6].wrapping_add(g);
        h[7] = h[7].wrapping_add(hh);
    }

    let mut hex = String::with_capacity(64);
    for word in h {
        hex.push_str(&format!("{word:08x}"));
    }
    hex
}

/// Digest of a callee's verification contract (`params`, `ret_type`, `effects`, `requires`, `ensures`).
///
/// Soundness: when a callee is pure (`effects.is_empty()`) and has no explicit `ens`
/// (`ensures.is_empty()`), the verifier inlines its body into callers (`Expr::Call`),
/// so its content-addressed `callee.hash` (which transitively covers its body and any
/// nested pure helpers) MUST be included in the digest. Otherwise, editing the helper's
/// body would produce a stale cache hit and accept an unsound caller.
pub fn callee_contract_digest(callee: &CanonicalNode) -> String {
    let inlined_body_hash: Option<&str> =
        if callee.ast.ensures.is_empty() && callee.ast.effects.is_empty() {
            Some(callee.hash.as_str())
        } else {
            None
        };
    let payload = serde_json::to_string(&(
        &callee.ast.params,
        &callee.ast.ret_type,
        &callee.ast.effects,
        &callee.ast.requires,
        &callee.ast.ensures,
        inlined_body_hash,
    ))
    .unwrap_or_default();
    sha256_hex(payload.as_bytes())
}

/// Compute the cryptographically sound, contract-invariant Z3 proof cache key:
/// replaces callee implementation hashes in `node.ast` with each callee's
/// contract digest (`params`, `ret_type`, `effects`, `requires`, `ensures`).
/// Thus editing a callee's body without changing its contract yields a 100% cache hit
/// for all callers, while any contract or caller-body change triggers Z3 re-verification.
pub fn compute_proof_key<F>(
    node: &CanonicalNode,
    load_callee: &F,
    z3_version: &str,
) -> Result<String, InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    let mut ast_json = serde_json::to_string(&node.ast).unwrap_or_default();
    let mut callee_contracts: Vec<String> = Vec::with_capacity(node.direct_deps.len());
    for dep_hash in &node.direct_deps {
        let callee = load_callee(dep_hash)?;
        let cdig = callee_contract_digest(&callee);
        let replacement = format!("contract:{cdig}");
        ast_json = ast_json.replace(dep_hash.as_str(), &replacement);
        callee_contracts.push(cdig);
    }
    callee_contracts.sort();

    let key_material = serde_json::to_string(&(
        PROOF_CACHE_SCHEMA_VERSION,
        "fn-contract-v2",
        &ast_json,
        &callee_contracts,
        z3_version,
    ))
    .unwrap_or_default();
    Ok(sha256_hex(key_material.as_bytes()))
}

fn strip_spans_json(v: &mut serde_json::Value) {
    match v {
        serde_json::Value::Object(map) => {
            map.remove("span");
            for val in map.values_mut() {
                strip_spans_json(val);
            }
        }
        serde_json::Value::Array(arr) => {
            for val in arr.iter_mut() {
                strip_spans_json(val);
            }
        }
        _ => {}
    }
}

/// Compute a span-insensitive Z3 proof cache key for an `Infimum.Forma` layout block.
pub fn compute_forma_proof_key(forma: &crate::forma::ast::FormaDef, z3_version: &str) -> String {
    let mut val = serde_json::to_value(forma).unwrap_or(serde_json::Value::Null);
    strip_spans_json(&mut val);
    let key_material = serde_json::to_string(&(
        PROOF_CACHE_SCHEMA_VERSION,
        "forma-v1",
        &val,
        z3_version,
    ))
    .unwrap_or_default();
    sha256_hex(key_material.as_bytes())
}

pub(crate) fn find_z3_binary() -> Result<PathBuf, InfimumDiagnostic> {
    if let Ok(custom) = env::var("INFIMUM_Z3_BIN") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return Ok(p);
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let local = PathBuf::from(home).join(".local/bin/z3");
        if local.is_file() {
            return Ok(local);
        }
    }
    for candidate in ["/usr/bin/z3", "/usr/local/bin/z3"] {
        let p = PathBuf::from(candidate);
        if p.is_file() {
            return Ok(p);
        }
    }
    Err(InfimumDiagnostic::new(
        "E058_Z3_NOT_FOUND",
        "verify",
        "Z3 SMT solver binary not found",
        Span::zero(),
        "Install Z3 in ~/.local/bin/z3 or /usr/bin/z3, or set INFIMUM_Z3_BIN",
    ))
}

pub(crate) fn run_z3(z3_bin: &PathBuf, script: &str) -> Result<String, InfimumDiagnostic> {
    let mut child = Command::new(z3_bin)
        .args(["-smt2", "-in", "-t:3000"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| {
            InfimumDiagnostic::new(
                "E058_Z3_EXEC_ERROR",
                "verify",
                format!("Failed to launch Z3 solver: {e}"),
                Span::zero(),
                "Verify Z3 binary permissions",
            )
        })?;

    if let Some(mut stdin) = child.stdin.take() {
        let _ = stdin.write_all(script.as_bytes());
    }

    let output = child.wait_with_output().map_err(|e| {
        InfimumDiagnostic::new(
            "E058_Z3_EXEC_ERROR",
            "verify",
            format!("Failed waiting on Z3 solver: {e}"),
            Span::zero(),
            "Verify Z3 process health",
        )
    })?;

    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

fn extract_counterexample(z3_out: &str, raw_params: &[Param]) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    let rest: String = z3_out.lines().skip(1).collect::<Vec<_>>().join(" ");
    let pairs = parse_sexp_pairs(&rest);

    for (k, v) in pairs {
        let clean_val = normalize_smt_value(&v);
        if let Some(idx_str) = k.strip_prefix('p') {
            if let Ok(idx) = idx_str.parse::<usize>() {
                if let Some(p) = raw_params.get(idx) {
                    map.insert(p.name.clone(), clean_val);
                    continue;
                }
            }
        }
        if k == "$result" {
            map.insert("result".to_string(), clean_val);
        } else {
            map.insert(k, clean_val);
        }
    }
    map
}

pub(crate) fn normalize_smt_value(val: &str) -> String {
    let trimmed = val.trim();
    if let Some(inner) = trimmed.strip_prefix("(- ").and_then(|s| s.strip_suffix(')')) {
        return format!("-{}", inner.trim());
    }
    trimmed.to_string()
}

pub(crate) fn parse_sexp_pairs(input: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let bytes = input.as_bytes();
    let n = bytes.len();
    let mut i = 0;

    // Skip outer '('
    while i < n && bytes[i] != b'(' {
        i += 1;
    }
    i += 1;

    while i < n {
        while i < n && bytes[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= n || bytes[i] == b')' {
            break;
        }
        if bytes[i] == b'(' {
            i += 1;
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let key_start = i;
            while i < n && !bytes[i].is_ascii_whitespace() && bytes[i] != b')' {
                i += 1;
            }
            let key = input[key_start..i].to_string();
            while i < n && bytes[i].is_ascii_whitespace() {
                i += 1;
            }
            let val_start = i;
            let mut depth = 0usize;
            let mut in_str = false;
            while i < n {
                let b = bytes[i];
                if b == b'"' {
                    in_str = !in_str;
                } else if !in_str {
                    if b == b'(' {
                        depth += 1;
                    } else if b == b')' {
                        if depth == 0 {
                            break;
                        }
                        depth -= 1;
                    }
                }
                i += 1;
            }
            let val = input[val_start..i].trim().to_string();
            if i < n && bytes[i] == b')' {
                i += 1;
            }
            if !key.is_empty() {
                out.push((key, val));
            }
        } else {
            i += 1;
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sha256_matches_nist_vectors() {
        assert_eq!(
            sha256_hex(b""),
            "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855"
        );
        assert_eq!(
            sha256_hex(b"abc"),
            "ba7816bf8f01cfea414140de5dae2223b00361a396177a9cb410ff61f20015ad"
        );
    }

    fn verify_single_fn_src(src: &str) -> Result<VerificationReport, InfimumDiagnostic> {
        let m = crate::parser::parse_module(src)?;
        let f = &m.functions[0];
        let node = crate::canon::canonicalize_fn(
            f,
            &|_| panic!("unexpected callee"),
            &|_| panic!("unexpected hash"),
        )?;
        verify_node(f, &node, &|_| panic!("unexpected callee load"))
    }

    #[test]
    fn z3_rejects_false_ensures_after_if_mutation() {
        let src = r#"
fn unsound(flag: bool) -> i64
  ens result == 1
{
  let mut x = 1;
  if flag { x = 999; }
  x
}
"#;
        let err = verify_single_fn_src(src).unwrap_err();
        assert_eq!(err.code, "E050_CONTRACT_VIOLATION");
    }

    #[test]
    fn z3_rejects_div_by_zero_after_if_mutation() {
        let src = r#"
fn div_after_if(flag: bool) -> i64 {
  let mut x = 1;
  if flag { x = 0; }
  10 / x
}
"#;
        let err = verify_single_fn_src(src).unwrap_err();
        assert_eq!(err.code, "E053_POSSIBLE_DIV_BY_ZERO");
    }

    #[test]
    fn z3_havocs_var_mutated_inside_if_within_while() {
        let src = r#"
fn div_after_while_if(flag: bool) -> i64 {
  let mut x = 1;
  let mut i = 0;
  while i < 5 max 10 {
    if flag { x = 0; }
    i = i + 1;
  }
  10 / x
}
"#;
        let err = verify_single_fn_src(src).unwrap_err();
        assert_eq!(err.code, "E053_POSSIBLE_DIV_BY_ZERO");
    }

    #[test]
    fn z3_proves_valid_ensures_after_if_mutation() {
        let src = r#"
fn sound_branch(flag: bool) -> i64
  ens result >= 1 && result <= 2
{
  let mut x = 1;
  if flag { x = 2; }
  x
}
"#;
        verify_single_fn_src(src).unwrap();
    }

    #[test]
    fn z3_merges_bool_tuple_field_across_if_without_sort_mismatch() {
        let src = r#"
fn bool_tuple_branch(flag: bool) -> i64
  ens result == 1
{
  let mut st = (false, 0);
  if flag {
    st = (true, 1);
  } else {
    st = (true, 1);
  }
  if st.0 { st.1 } else { 0 }
}
"#;
        verify_single_fn_src(src).unwrap();
    }
}
