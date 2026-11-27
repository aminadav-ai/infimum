use crate::ast::{BinOp, Expr, FnDef, Stmt, TargetRef, Type, UnaryOp};
use crate::canon::CanonicalNode;
use crate::diag::{InfimumDiagnostic, Span};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
pub struct DirectSecretSink {
    pub secret_type: String,
    pub domain: String,
    pub operation: String,
}

struct TypeEnv {
    frames: Vec<BTreeMap<String, Type>>,
}

impl TypeEnv {
    fn from_fn(f: &FnDef) -> Self {
        let mut root = BTreeMap::new();
        for p in &f.params {
            root.insert(p.name.clone(), p.ty.clone());
        }
        Self { frames: vec![root] }
    }

    fn push(&mut self) {
        self.frames.push(BTreeMap::new());
    }

    fn pop(&mut self) {
        self.frames.pop();
    }

    fn bind(&mut self, name: String, ty: Type) {
        if let Some(top) = self.frames.last_mut() {
            top.insert(name, ty);
        }
    }

    fn lookup(&self, name: &str) -> Option<Type> {
        for frame in self.frames.iter().rev() {
            if let Some(t) = frame.get(name) {
                return Some(t.clone());
            }
        }
        None
    }
}

pub fn typecheck_node<F>(
    raw_name: &str,
    canon_node: &CanonicalNode,
    load_callee: &F,
) -> Result<Vec<DirectSecretSink>, InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    let f = &canon_node.ast;
    if f.exported {
        for p in &f.params {
            if matches!(p.ty, Type::Slice(_)) {
                return Err(InfimumDiagnostic::new(
                    "E082_NON_SCALAR_EXPORT",
                    "typecheck",
                    format!(
                        "Exported function '{}' cannot take slice parameter '{}: {}'",
                        raw_name, p.name, p.ty
                    ),
                    Span::zero(),
                    "Pass scalar '(ptr: i64, len: i64)' across the Wasm export boundary and wrap with 'slice_i64(ptr, len)' inside",
                ));
            }
        }
        if matches!(f.ret_type, Type::Slice(_)) {
            return Err(InfimumDiagnostic::new(
                "E082_NON_SCALAR_EXPORT",
                "typecheck",
                format!(
                    "Exported function '{}' cannot return slice type '{}'",
                    raw_name, f.ret_type
                ),
                Span::zero(),
                "Return a scalar i64/bool/unit from 'export fn' and use slices internally",
            ));
        }
    }
    let mut env = TypeEnv::from_fn(f);
    let mut sinks = Vec::new();

    // 1. Typecheck preconditions (`req`) — must be bool and free of Secret<T> inspection
    for req in &f.requires {
        let ty = check_expr(req, raw_name, canon_node, &mut env, load_callee, &mut sinks, true)?;
        if ty != Type::Bool {
            return Err(InfimumDiagnostic::new(
                "E060_TYPE_MISMATCH",
                "typecheck",
                format!(
                    "Precondition ('req') in '{raw_name}' must have type 'bool', found '{ty}'"
                ),
                Span::zero(),
                "Write boolean expressions in 'req' clauses (e.g. req amount > 0)",
            ));
        }
    }

    // 2. Typecheck postconditions (`ens`) — bind $result, must be bool and free of Secret<T>
    env.push();
    env.bind("$result".to_string(), f.ret_type.clone());
    for ens in &f.ensures {
        let ty = check_expr(ens, raw_name, canon_node, &mut env, load_callee, &mut sinks, true)?;
        if ty != Type::Bool {
            return Err(InfimumDiagnostic::new(
                "E060_TYPE_MISMATCH",
                "typecheck",
                format!(
                    "Postcondition ('ens') in '{raw_name}' must have type 'bool', found '{ty}'"
                ),
                Span::zero(),
                "Write boolean expressions in 'ens' clauses (e.g. ens result >= 0)",
            ));
        }
    }
    env.pop();

    // 3. Typecheck function body and match return type
    let body_ty = check_expr(
        &f.body,
        raw_name,
        canon_node,
        &mut env,
        load_callee,
        &mut sinks,
        false,
    )?;
    if !types_compatible(&f.ret_type, &body_ty) {
        return Err(InfimumDiagnostic::new(
            "E060_TYPE_MISMATCH",
            "typecheck",
            format!(
                "Function '{raw_name}' declares return type '{}', but body evaluates to '{body_ty}'",
                f.ret_type
            ),
            Span::zero(),
            format!(
                "Change return expression or update signature of '{raw_name}' to '-> {body_ty}'"
            ),
        ));
    }

    sinks.sort();
    sinks.dedup();
    Ok(sinks)
}

fn check_expr<F>(
    expr: &Expr,
    fn_name: &str,
    self_node: &CanonicalNode,
    env: &mut TypeEnv,
    load_callee: &F,
    sinks: &mut Vec<DirectSecretSink>,
    in_contract: bool,
) -> Result<Type, InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    match expr {
        Expr::IntLit(_) => Ok(Type::I64),
        Expr::FixedLit(_) => Ok(Type::Fixed),
        Expr::BoolLit(_) => Ok(Type::Bool),
        Expr::StrLit(_) => Ok(Type::Str),
        Expr::UnitLit => Ok(Type::Unit),
        Expr::Var(name) => {
            let ty = env.lookup(name).ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E020_UNKNOWN_VAR",
                    "typecheck",
                    format!("Unbound symbol '{name}' in '{fn_name}'"),
                    Span::zero(),
                    "Ensure variable is declared in parameters or a preceding 'let'",
                )
            })?;
            if in_contract && type_contains_secret(&ty) {
                return Err(InfimumDiagnostic::new(
                    "E062_SECRET_IN_CONTRACT",
                    "typecheck",
                    format!(
                        "Contract of '{fn_name}' references '{ty}'; contracts cannot inspect Secret<T> values"
                    ),
                    Span::zero(),
                    "Keep Secret<T> opaque in public contracts to prevent specification side channels",
                ));
            }
            Ok(ty)
        }
        Expr::SecretWrap(inner) => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E062_SECRET_IN_CONTRACT",
                    "typecheck",
                    format!("'secret(...)' cannot appear inside a contract in '{fn_name}'"),
                    Span::zero(),
                    "Wrap secrets inside executable function bodies, not contracts",
                ));
            }
            let inner_ty = check_expr(
                inner,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            if type_contains_secret(&inner_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Cannot double-wrap '{inner_ty}' inside Secret<...> in '{fn_name}'"),
                    Span::zero(),
                    "Value is already wrapped as Secret<T>",
                ));
            }
            Ok(Type::Secret(Box::new(inner_ty)))
        }
        Expr::Reveal { domain, .. } => Err(InfimumDiagnostic::new(
            "E064_REVEAL_OUTSIDE_NET_SINK",
            "typecheck",
            format!(
                "Illegal 'reveal(\"{domain}\", ...)' in '{fn_name}': revealed secrets cannot be stored in variables, returned, or used in expressions"
            ),
            Span::zero(),
            format!(
                "Pass 'reveal(\"{domain}\", secret_val)' directly into 'do Net.<op>(...)' inside a function pinned to Net(\"{domain}\")"
            ),
        )),
        Expr::Unary { op, expr: inner } => {
            let t = check_expr(
                inner,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            if type_contains_secret(&t) {
                return Err(InfimumDiagnostic::new(
                    "E062_SECRET_OPACITY_VIOLATION",
                    "typecheck",
                    format!(
                        "Cannot apply unary operator '{}' to opaque '{t}' in '{fn_name}'",
                        op.as_str()
                    ),
                    Span::zero(),
                    "Secret<T> values are opaque to prevent side-channel leaks; pass them intact or reveal at an authorized Net(\"domain\") sink",
                ));
            }
            match (op, &t) {
                (UnaryOp::Neg, Type::I64 | Type::U8) => Ok(Type::I64),
                (UnaryOp::Neg, Type::Fixed) => Ok(Type::Fixed),
                (UnaryOp::Not, Type::Bool) => Ok(Type::Bool),
                (UnaryOp::IntToFixed, Type::I64 | Type::U8) => Ok(Type::Fixed),
                (UnaryOp::FixedToInt, Type::Fixed) => Ok(Type::I64),
                (UnaryOp::FixedToRaw, Type::Fixed) => Ok(Type::I64),
                (UnaryOp::RawToFixed, Type::I64 | Type::U8) => Ok(Type::Fixed),
                (UnaryOp::ToU8, Type::I64 | Type::U8) => Ok(Type::U8),
                _ => Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!(
                        "Unary operator '{}' cannot be applied to type '{t}' in '{fn_name}'",
                        op.as_str()
                    ),
                    Span::zero(),
                    "Use '-' on i64/fixed, '!' on bool, 'fixed(i64)' / 'fixed_to_int(fixed)' for conversions",
                )),
            }
        }
        Expr::Binary { op, lhs, rhs } => {
            let lt = check_expr(
                lhs,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            let rt = check_expr(
                rhs,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            if type_contains_secret(&lt) || type_contains_secret(&rt) {
                return Err(InfimumDiagnostic::new(
                    "E062_SECRET_OPACITY_VIOLATION",
                    "typecheck",
                    format!(
                        "Cannot apply operator '{}' to Secret<T> ('{lt}' vs '{rt}') in '{fn_name}'",
                        op.as_str()
                    ),
                    Span::zero(),
                    "Comparing or computing directly on Secret<T> is forbidden to block timing and control-flow side-channel attacks",
                ));
            }
            match op {
                BinOp::Add | BinOp::Sub => {
                    if is_int_like(&lt) && is_int_like(&rt) {
                        Ok(Type::I64)
                    } else if lt == Type::Fixed && rt == Type::Fixed {
                        Ok(Type::Fixed)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Arithmetic operator '{}' expects (i64, i64) or (fixed, fixed), found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Convert integers explicitly with fixed(x) before combining with fixed values",
                        ))
                    }
                }
                BinOp::Mul => {
                    if is_int_like(&lt) && is_int_like(&rt) {
                        Ok(Type::I64)
                    } else if (lt == Type::Fixed && is_int_like(&rt))
                        || (is_int_like(&lt) && rt == Type::Fixed)
                    {
                        Ok(Type::Fixed)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Operator '*' expects (i64, i64), (fixed, fixed), or (fixed, i64), found ({lt}, {rt}) in '{fn_name}'"
                            ),
                            Span::zero(),
                            "Use matching numeric types or fixed(x)",
                        ))
                    }
                }
                BinOp::Div => {
                    if is_int_like(&lt) && is_int_like(&rt) {
                        Ok(Type::I64)
                    } else if lt == Type::Fixed && is_int_like(&rt) {
                        Ok(Type::Fixed)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Operator '/' expects (i64, i64), (fixed, fixed), or (fixed, i64), found ({lt}, {rt}) in '{fn_name}'"
                            ),
                            Span::zero(),
                            "Use matching numeric types or fixed(x)",
                        ))
                    }
                }
                BinOp::FixedMul | BinOp::FixedDiv => {
                    if lt == Type::Fixed && rt == Type::Fixed {
                        Ok(Type::Fixed)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Fixed-point operator '{}' expects (fixed, fixed), found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Ensure both operands have type fixed",
                        ))
                    }
                }
                BinOp::Mod
                | BinOp::BitAnd
                | BinOp::BitOr
                | BinOp::BitXor
                | BinOp::Shl
                | BinOp::Shr => {
                    if is_int_like(&lt) && is_int_like(&rt) {
                        Ok(Type::I64)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Integer/bitwise operator '{}' expects (i64, i64), found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Ensure both operands have type i64",
                        ))
                    }
                }
                BinOp::Lt | BinOp::Le | BinOp::Gt | BinOp::Ge => {
                    if (is_int_like(&lt) && is_int_like(&rt))
                        || (lt == Type::Fixed && rt == Type::Fixed)
                    {
                        Ok(Type::Bool)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Comparison operator '{}' expects (i64, i64) or (fixed, fixed), found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Ensure both operands of ordering comparisons have the same numeric type",
                        ))
                    }
                }
                BinOp::And | BinOp::Or => {
                    if lt == Type::Bool && rt == Type::Bool {
                        Ok(Type::Bool)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Logical operator '{}' expects (bool, bool), found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Ensure both operands of '&&' / '||' have type bool",
                        ))
                    }
                }
                BinOp::Eq | BinOp::Ne => {
                    if types_compatible(&lt, &rt) {
                        Ok(Type::Bool)
                    } else {
                        Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Equality operator '{}' requires identical types, found ({lt}, {rt}) in '{fn_name}'",
                                op.as_str()
                            ),
                            Span::zero(),
                            "Compare values of the same non-secret type",
                        ))
                    }
                }
            }
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            let ct = check_expr(
                cond,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            if ct != Type::Bool {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'if' condition in '{fn_name}' must be 'bool', found '{ct}'"),
                    Span::zero(),
                    "Provide a boolean condition for 'if'",
                ));
            }
            let tt = check_expr(
                then_branch,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            let et = check_expr(
                else_branch,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            if !types_compatible(&tt, &et) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!(
                        "'if' branches in '{fn_name}' have incompatible types: '{tt}' vs '{et}'"
                    ),
                    Span::zero(),
                    "Ensure both 'if' and 'else' branches return the exact same type",
                ));
            }
            if tt == Type::U8 && et == Type::I64 {
                Ok(Type::I64)
            } else {
                Ok(tt)
            }
        }
        Expr::Perform { effect, op, args } => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E066_EFFECT_IN_CONTRACT",
                    "typecheck",
                    format!(
                        "Effectful operation 'do {effect}.{op}' is forbidden inside contracts of '{fn_name}'"
                    ),
                    Span::zero(),
                    "Contracts ('req'/'ens') must be pure mathematical specifications",
                ));
            }

            for arg in args {
                if let Expr::Reveal {
                    domain,
                    expr: inner_secret,
                } = arg
                {
                    if effect != "Net" {
                        return Err(InfimumDiagnostic::new(
                            "E064_REVEAL_OUTSIDE_NET_SINK",
                            "typecheck",
                            format!(
                                "Function '{fn_name}' attempts to pass revealed secret for '{domain}' into non-network sink '{effect}.{op}'"
                            ),
                            Span::zero(),
                            "Secrets can never be written to Fs, Audit, or Clock sinks; only domain-pinned Net(\"...\") sinks are allowed",
                        ));
                    }

                    let sec_ty = check_expr(
                        inner_secret,
                        fn_name,
                        self_node,
                        env,
                        load_callee,
                        sinks,
                        false,
                    )?;
                    let Type::Secret(inner_t) = sec_ty else {
                        return Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "'reveal(\"{domain}\", ...)' in '{fn_name}' expects a Secret<T> argument, found '{sec_ty}'"
                            ),
                            Span::zero(),
                            "Pass a value of type Secret<T> to reveal(...)",
                        ));
                    };

                    // Verify strict single-domain Net("domain") pinning on enclosing function!
                    verify_net_domain_pinning(fn_name, &self_node.ast, domain)?;

                    sinks.push(DirectSecretSink {
                        secret_type: format!("Secret<{inner_t}>"),
                        domain: domain.clone(),
                        operation: format!("Net.{op}"),
                    });
                } else {
                    let arg_ty = check_expr(
                        arg,
                        fn_name,
                        self_node,
                        env,
                        load_callee,
                        sinks,
                        false,
                    )?;
                    if type_contains_secret(&arg_ty) {
                        return Err(InfimumDiagnostic::new(
                            "E063_RAW_SECRET_SINK_LEAK",
                            "typecheck",
                            format!(
                                "Security violation in '{fn_name}': raw '{arg_ty}' passed directly to sink '{effect}.{op}'"
                            ),
                            Span::zero(),
                            "Raw Secret<T> values cannot be written to logs, files, or network sockets; use explicit domain-pinned reveal(\"domain\", secret_val) inside a pinned Net(\"domain\") node",
                        ));
                    }
                }
            }
            Ok(Type::Unit)
        }
        Expr::MemLoad { index, fixed_ty, .. } => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E066_EFFECT_IN_CONTRACT",
                    "typecheck",
                    format!("'mem_load' is forbidden inside contracts of '{fn_name}'"),
                    Span::zero(),
                    "Contracts ('req'/'ens') must be pure mathematical specifications",
                ));
            }
            let idx_ty = check_expr(index, fn_name, self_node, env, load_callee, sinks, false)?;
            if !is_int_like(&idx_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'mem_load' index in '{fn_name}' must be 'i64', found '{idx_ty}'"),
                    Span::zero(),
                    "Pass an i64 word index to mem_load(idx)",
                ));
            }
            Ok(if *fixed_ty { Type::Fixed } else { Type::I64 })
        }
        Expr::MemStore { index, value, fixed_ty, .. } => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E066_EFFECT_IN_CONTRACT",
                    "typecheck",
                    format!("'mem_store' is forbidden inside contracts of '{fn_name}'"),
                    Span::zero(),
                    "Contracts ('req'/'ens') must be pure mathematical specifications",
                ));
            }
            let idx_ty = check_expr(index, fn_name, self_node, env, load_callee, sinks, false)?;
            if !is_int_like(&idx_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'mem_store' index in '{fn_name}' must be 'i64', found '{idx_ty}'"),
                    Span::zero(),
                    "Pass an i64 word index to mem_store(idx, val)",
                ));
            }
            let val_ty = check_expr(value, fn_name, self_node, env, load_callee, sinks, false)?;
            if type_contains_secret(&val_ty) {
                return Err(InfimumDiagnostic::new(
                    "E063_RAW_SECRET_SINK_LEAK",
                    "typecheck",
                    format!(
                        "Security violation in '{fn_name}': raw '{val_ty}' cannot be written to shared Strata linear memory"
                    ),
                    Span::zero(),
                    "Never store raw Secret<T> values in shared framebuffer/linear memory",
                ));
            }
            let expected_ty = if *fixed_ty { Type::Fixed } else { Type::I64 };
            if !types_compatible(&expected_ty, &val_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Memory store in '{fn_name}' expects '{expected_ty}', found '{val_ty}'"),
                    Span::zero(),
                    format!("Provide a '{expected_ty}' value (use fixed(x) / fixed_to_int(x) if converting)"),
                ));
            }
            Ok(Type::Unit)
        }
        Expr::ByteLoad { index, .. } => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E066_EFFECT_IN_CONTRACT",
                    "typecheck",
                    format!("'byte_load' is forbidden inside contracts of '{fn_name}'"),
                    Span::zero(),
                    "Contracts ('req'/'ens') must be pure mathematical specifications",
                ));
            }
            let idx_ty = check_expr(index, fn_name, self_node, env, load_callee, sinks, false)?;
            if !is_int_like(&idx_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'byte_load' index in '{fn_name}' must be 'i64', found '{idx_ty}'"),
                    Span::zero(),
                    "Pass an i64 byte address to byte_load(addr)",
                ));
            }
            Ok(Type::U8)
        }
        Expr::ByteStore { index, value, .. } => {
            if in_contract {
                return Err(InfimumDiagnostic::new(
                    "E066_EFFECT_IN_CONTRACT",
                    "typecheck",
                    format!("'byte_store' is forbidden inside contracts of '{fn_name}'"),
                    Span::zero(),
                    "Contracts ('req'/'ens') must be pure mathematical specifications",
                ));
            }
            let idx_ty = check_expr(index, fn_name, self_node, env, load_callee, sinks, false)?;
            if !is_int_like(&idx_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'byte_store' index in '{fn_name}' must be 'i64', found '{idx_ty}'"),
                    Span::zero(),
                    "Pass an i64 byte address to byte_store(addr, val)",
                ));
            }
            let val_ty = check_expr(value, fn_name, self_node, env, load_callee, sinks, false)?;
            if type_contains_secret(&val_ty) {
                return Err(InfimumDiagnostic::new(
                    "E063_RAW_SECRET_SINK_LEAK",
                    "typecheck",
                    format!(
                        "Security violation in '{fn_name}': raw '{val_ty}' cannot be written to shared Strata linear memory"
                    ),
                    Span::zero(),
                    "Never store raw Secret<T> values in shared framebuffer/linear memory",
                ));
            }
            if !is_int_like(&val_ty) {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Byte store in '{fn_name}' expects 'u8' or 'i64', found '{val_ty}'"),
                    Span::zero(),
                    "Provide a u8 or i64 value in 0..=255",
                ));
            }
            Ok(Type::Unit)
        }
        Expr::Call { target, args } => {
            let callee_node: CanonicalNode;
            let (callee_ast, callee_label): (&FnDef, String) = match target {
                TargetRef::SelfRec => (&self_node.ast, fn_name.to_string()),
                TargetRef::Hash(h) => {
                    callee_node = load_callee(h)?;
                    let short = callee_node.short_hash.clone();
                    (&callee_node.ast, format!("#{short}"))
                }
                TargetRef::Named(n) => {
                    return Err(InfimumDiagnostic::new(
                        "E055_UNRESOLVED_CALL",
                        "typecheck",
                        format!("Unresolved symbolic call '{n}' in '{fn_name}'"),
                        Span::zero(),
                        "Canonicalize function calls before typechecking",
                    ));
                }
            };

            if args.len() != callee_ast.params.len() {
                return Err(InfimumDiagnostic::new(
                    "E056_ARITY_MISMATCH",
                    "typecheck",
                    format!(
                        "Call to {callee_label} in '{fn_name}' passed {} argument(s), expected {}",
                        args.len(),
                        callee_ast.params.len()
                    ),
                    Span::zero(),
                    "Match the exact parameter count of the called function",
                ));
            }

            for (i, (arg_expr, param)) in args.iter().zip(callee_ast.params.iter()).enumerate() {
                let arg_ty = check_expr(
                    arg_expr,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
                if !types_compatible(&param.ty, &arg_ty) {
                    return Err(InfimumDiagnostic::new(
                        "E061_ARG_TYPE_MISMATCH",
                        "typecheck",
                        format!(
                            "Argument #{} in call to {callee_label} inside '{fn_name}' has type '{arg_ty}', expected '{}'",
                            i + 1,
                            param.ty
                        ),
                        Span::zero(),
                        format!(
                            "Pass a value of type '{}' for parameter #{}",
                            param.ty,
                            i + 1
                        ),
                    ));
                }
            }

            // Transitive Flux effect check: caller must declare every capability required by callee
            // (test harness blocks `__ifm_test_*` are exempt since they are never emitted to .ifw).
            if !in_contract
                && !matches!(target, TargetRef::SelfRec)
                && !fn_name.starts_with("__ifm_test_")
            {
                for req_cap in &callee_ast.effects {
                    if !caller_covers_effect(&self_node.ast.effects, req_cap) {
                        return Err(InfimumDiagnostic::new(
                            "E021_UNDECLARED_EFFECT",
                            "typecheck",
                            format!(
                                "Function '{fn_name}' calls {callee_label} which requires effect '{req_cap}', but '{fn_name}' does not declare it"
                            ),
                            Span::zero(),
                            format!(
                                "Add '{req_cap}' to the effect signature of '{fn_name}': '-[..., {req_cap}]-> {}'",
                                self_node.ast.ret_type
                            ),
                        ));
                    }
                }
            }

            Ok(callee_ast.ret_type.clone())
        }
        Expr::Block { stmts, tail } => {
            env.push();
            check_stmt_slice(
                stmts,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            let tail_ty = check_expr(
                tail,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            env.pop();
            Ok(tail_ty)
        }
        Expr::TupleLit(elems) => {
            let mut tys = Vec::with_capacity(elems.len());
            for e in elems {
                tys.push(check_expr(
                    e,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?);
            }
            Ok(Type::Tuple(tys))
        }
        Expr::TupleField { expr, index } => {
            let ty = check_expr(
                expr,
                fn_name,
                self_node,
                env,
                load_callee,
                sinks,
                in_contract,
            )?;
            match ty {
                Type::Tuple(elems) => elems.get(*index).cloned().ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E030_TYPE_MISMATCH",
                        "typecheck",
                        format!(
                            "Tuple index .{index} out of bounds for tuple of length {} in '{fn_name}'",
                            elems.len()
                        ),
                        Span::zero(),
                        format!("Use an index in 0..{}", elems.len()),
                    )
                }),
                other => Err(InfimumDiagnostic::new(
                    "E030_TYPE_MISMATCH",
                    "typecheck",
                    format!("Cannot access tuple field .{index} on non-tuple type '{other}' in '{fn_name}'"),
                    Span::zero(),
                    "Use '.0', '.1', ... only on tuple values like (a, b)",
                )),
            }
        }
        Expr::SliceLit { ptr, len, elem_ty } => {
            if type_contains_secret(elem_ty) {
                return Err(InfimumDiagnostic::new(
                    "E062_SECRET_IN_SLICE",
                    "typecheck",
                    format!("Slice element type '{elem_ty}' cannot contain Secret<T> in '{fn_name}'"),
                    Span::zero(),
                    "Store only non-secret scalars or structs in linear-memory slices",
                ));
            }
            let ptr_ty = check_expr(ptr, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if ptr_ty != Type::I64 {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Slice ptr must be 'i64', found '{ptr_ty}' in '{fn_name}'"),
                    Span::zero(),
                    "Pass an i64 word address as slice ptr",
                ));
            }
            let len_ty = check_expr(len, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if len_ty != Type::I64 {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Slice len must be 'i64', found '{len_ty}' in '{fn_name}'"),
                    Span::zero(),
                    "Pass an i64 element count as slice len",
                ));
            }
            Ok(Type::Slice(Box::new(elem_ty.clone())))
        }
        Expr::SlicePtr(inner) => {
            let ty = check_expr(inner, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if matches!(ty, Type::Slice(_)) {
                Ok(Type::I64)
            } else {
                Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'.ptr' requires a slice type '[T]', found '{ty}' in '{fn_name}'"),
                    Span::zero(),
                    "Access '.ptr' only on slice values",
                ))
            }
        }
        Expr::StrPtr(inner) => {
            let ty = check_expr(inner, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if matches!(ty, Type::Str | Type::I64) {
                Ok(Type::I64)
            } else {
                Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'str_as_slice' requires 'str', found '{ty}' in '{fn_name}'"),
                    Span::zero(),
                    "Pass a 'str' literal or parameter to str_as_slice(s)",
                ))
            }
        }
        Expr::SliceLen(inner) => {
            let ty = check_expr(inner, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if matches!(ty, Type::Slice(_)) {
                Ok(Type::I64)
            } else {
                Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("'.len' requires a slice type '[T]', found '{ty}' in '{fn_name}'"),
                    Span::zero(),
                    "Access '.len' only on slice values",
                ))
            }
        }
        Expr::BoundsCheck { index, len, .. } => {
            let it = check_expr(index, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            let lt = check_expr(len, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if it != Type::I64 || lt != Type::I64 {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Slice index and length must be 'i64', found '{it}' and '{lt}' in '{fn_name}'"),
                    Span::zero(),
                    "Use i64 expressions for slice indexing",
                ));
            }
            Ok(Type::I64)
        }
        Expr::RangeCheck { lo, hi, len } => {
            let lot = check_expr(lo, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            let hit = check_expr(hi, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            let lent = check_expr(len, fn_name, self_node, env, load_callee, sinks, in_contract)?;
            if lot != Type::I64 || hit != Type::I64 || lent != Type::I64 {
                return Err(InfimumDiagnostic::new(
                    "E060_TYPE_MISMATCH",
                    "typecheck",
                    format!("Slice range bounds must be 'i64', found '{lot}', '{hit}', '{lent}' in '{fn_name}'"),
                    Span::zero(),
                    "Use i64 expressions for slice ranges [lo..hi]",
                ));
            }
            Ok(Type::I64)
        }
    }
}

fn check_stmt_slice<F>(
    stmts: &[Stmt],
    fn_name: &str,
    self_node: &CanonicalNode,
    env: &mut TypeEnv,
    load_callee: &F,
    sinks: &mut Vec<DirectSecretSink>,
    in_contract: bool,
) -> Result<(), InfimumDiagnostic>
where
    F: Fn(&str) -> Result<CanonicalNode, InfimumDiagnostic>,
{
    for stmt in stmts {
        match stmt {
            Stmt::Let {
                name, ty, value, ..
            } => {
                let val_ty = check_expr(
                    value,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
                if let Some(decl_ty) = ty {
                    if !types_compatible(decl_ty, &val_ty) {
                        return Err(InfimumDiagnostic::new(
                            "E060_TYPE_MISMATCH",
                            "typecheck",
                            format!(
                                "Let binding '{name}' in '{fn_name}' declared type '{decl_ty}', but initializer has type '{val_ty}'"
                            ),
                            Span::zero(),
                            format!("Change annotation to '{val_ty}' or fix initializer expression"),
                        ));
                    }
                    env.bind(name.clone(), decl_ty.clone());
                } else {
                    env.bind(name.clone(), val_ty);
                }
            }
            Stmt::LetTuple { names, value } => {
                let val_ty = check_expr(
                    value,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
                let Type::Tuple(elem_tys) = val_ty else {
                    return Err(InfimumDiagnostic::new(
                        "E030_TYPE_MISMATCH",
                        "typecheck",
                        format!(
                            "Tuple destructuring 'let (...)' requires a tuple value, found '{val_ty}' in '{fn_name}'"
                        ),
                        Span::zero(),
                        "Ensure the right-hand side evaluates to a tuple such as (a, b)",
                    ));
                };
                if elem_tys.len() != names.len() {
                    return Err(InfimumDiagnostic::new(
                        "E030_TYPE_MISMATCH",
                        "typecheck",
                        format!(
                            "Tuple destructuring arity mismatch in '{fn_name}': pattern has {} bindings, value has {} elements",
                            names.len(),
                            elem_tys.len()
                        ),
                        Span::zero(),
                        "Match the number of variables in 'let (a, b) = ...' to the tuple size",
                    ));
                }
                for ((n, _), et) in names.iter().zip(elem_tys) {
                    env.bind(n.clone(), et);
                }
            }
            Stmt::Assign { name, value } => {
                let expected = env.lookup(name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E020_UNKNOWN_VAR",
                        "typecheck",
                        format!("Assignment to undeclared variable '{name}' in '{fn_name}'"),
                        Span::zero(),
                        "Declare mutable variable first with 'let mut'",
                    )
                })?;
                let val_ty = check_expr(
                    value,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
                if !types_compatible(&expected, &val_ty) {
                    return Err(InfimumDiagnostic::new(
                        "E060_TYPE_MISMATCH",
                        "typecheck",
                        format!(
                            "Assignment to '{name}' in '{fn_name}' expects type '{expected}', found '{val_ty}'"
                        ),
                        Span::zero(),
                        format!("Assign a value of type '{expected}'"),
                    ));
                }
            }
            Stmt::While { cond, body, .. } => {
                let c_ty = check_expr(
                    cond,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
                if c_ty != Type::Bool {
                    return Err(InfimumDiagnostic::new(
                        "E060_TYPE_MISMATCH",
                        "typecheck",
                        format!("'while' condition in '{fn_name}' must be 'bool', found '{c_ty}'"),
                        Span::zero(),
                        "Provide a boolean condition for 'while'",
                    ));
                }
                env.push();
                check_stmt_slice(body, fn_name, self_node, env, load_callee, sinks, in_contract)?;
                env.pop();
            }
            Stmt::Expr(e) => {
                let _ = check_expr(
                    e,
                    fn_name,
                    self_node,
                    env,
                    load_callee,
                    sinks,
                    in_contract,
                )?;
            }
        }
    }
    Ok(())
}

fn verify_net_domain_pinning(
    fn_name: &str,
    ast: &FnDef,
    expected_domain: &str,
) -> Result<(), InfimumDiagnostic> {
    let net_caps: Vec<Option<&str>> = ast
        .effects
        .iter()
        .filter(|c| c.name == "Net")
        .map(|c| c.scope.as_deref())
        .collect();

    if net_caps.is_empty() {
        return Err(InfimumDiagnostic::new(
            "E021_UNDECLARED_EFFECT",
            "typecheck",
            format!(
                "Function '{fn_name}' reveals a secret for '{expected_domain}' without declaring Net(\"{expected_domain}\")"
            ),
            Span::zero(),
            format!("Add Net(\"{expected_domain}\") to '{fn_name}' capability signature"),
        ));
    }

    if net_caps.contains(&None) {
        return Err(InfimumDiagnostic::new(
            "E065_REVEAL_DOMAIN_MISMATCH",
            "typecheck",
            format!(
                "Function '{fn_name}' reveals Secret for '{expected_domain}', but holds an unpinned wildcard 'Net' capability"
            ),
            Span::zero(),
            format!(
                "Replace wildcard 'Net' with strictly pinned 'Net(\"{expected_domain}\")' so secrets cannot be routed to arbitrary hosts"
            ),
        ));
    }

    let scopes: Vec<&str> = net_caps.into_iter().flatten().collect();
    if scopes != [expected_domain] {
        return Err(InfimumDiagnostic::new(
            "E065_REVEAL_DOMAIN_MISMATCH",
            "typecheck",
            format!(
                "Function '{fn_name}' reveals Secret for '{expected_domain}', but holds Net capabilities for {scopes:?}"
            ),
            Span::zero(),
            format!(
                "Isolate secret transmission inside a dedicated node whose sole Net capability is Net(\"{expected_domain}\")"
            ),
        ));
    }

    Ok(())
}

fn caller_covers_effect(caller_effects: &[crate::ast::EffectCap], req: &crate::ast::EffectCap) -> bool {
    caller_effects
        .iter()
        .any(|c| c.name == req.name && (c.scope.is_none() || c.scope == req.scope))
}

fn is_int_like(ty: &Type) -> bool {
    matches!(ty, Type::I64 | Type::U8)
}

fn types_compatible(expected: &Type, actual: &Type) -> bool {
    if expected == actual {
        return true;
    }
    match (expected, actual) {
        (Type::I64, Type::U8) | (Type::U8, Type::I64) => true,
        (Type::Tuple(es), Type::Tuple(as_)) => {
            es.len() == as_.len() && es.iter().zip(as_).all(|(e, a)| types_compatible(e, a))
        }
        _ => false,
    }
}

pub fn type_contains_secret(ty: &Type) -> bool {
    match ty {
        Type::Secret(_) => true,
        Type::Tuple(ts) => ts.iter().any(type_contains_secret),
        Type::Slice(inner) => type_contains_secret(inner),
        Type::I64 | Type::U8 | Type::Fixed | Type::Bool | Type::Str | Type::Unit => false,
    }
}
