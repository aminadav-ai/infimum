use crate::ast::{EffectCap, Expr, FnDef, Param, Stmt, TargetRef, Type};
use crate::diag::{InfimumDiagnostic, Span};
use crate::typecheck::DirectSecretSink;
use serde::{Deserialize, Serialize};
use std::collections::BTreeSet;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CanonicalNode {
    pub hash: String,
    pub short_hash: String,
    pub canonical_source: String,
    pub ast: FnDef,
    pub direct_deps: Vec<String>,
    pub direct_effects: Vec<EffectCap>,
    #[serde(default)]
    pub direct_secret_sinks: Vec<DirectSecretSink>,
}

struct ScopeEnv {
    frames: Vec<Vec<(String, String, bool)>>,
    next_local: usize,
    allow_result: bool,
}

impl ScopeEnv {
    fn new(params: &[Param]) -> Self {
        let mut root = Vec::with_capacity(params.len());
        for (i, p) in params.iter().enumerate() {
            root.push((p.name.clone(), format!("p{i}"), false));
        }
        Self {
            frames: vec![root],
            next_local: 0,
            allow_result: false,
        }
    }

    fn push_frame(&mut self) {
        self.frames.push(Vec::new());
    }

    fn pop_frame(&mut self) {
        self.frames.pop();
    }

    fn bind_local(&mut self, raw_name: &str, mutable: bool) -> String {
        let cname = format!("l{}", self.next_local);
        self.next_local += 1;
        if let Some(frame) = self.frames.last_mut() {
            frame.push((raw_name.to_string(), cname.clone(), mutable));
        }
        cname
    }

    fn lookup(&self, name: &str) -> Option<(String, bool)> {
        if self.allow_result && (name == "result" || name == "$result") {
            return Some(("$result".to_string(), false));
        }
        for frame in self.frames.iter().rev() {
            for (raw, canon, mutable) in frame.iter().rev() {
                if raw == name {
                    return Some((canon.clone(), *mutable));
                }
            }
        }
        None
    }
}

pub fn canonicalize_fn<FN, FH>(
    raw_fn: &FnDef,
    resolve_name: &FN,
    resolve_hash: &FH,
) -> Result<CanonicalNode, InfimumDiagnostic>
where
    FN: Fn(&str) -> Result<String, InfimumDiagnostic>,
    FH: Fn(&str) -> Result<String, InfimumDiagnostic>,
{
    let mut env = ScopeEnv::new(&raw_fn.params);
    let mut deps_set = BTreeSet::new();
    let mut perform_effects = BTreeSet::new();

    let canon_params: Vec<Param> = raw_fn
        .params
        .iter()
        .enumerate()
        .map(|(i, p)| Param {
            name: format!("p{i}"),
            ty: p.ty.clone(),
        })
        .collect();

    let effects_set: BTreeSet<EffectCap> = raw_fn.effects.iter().cloned().collect();

    let mut canon_requires = Vec::with_capacity(raw_fn.requires.len());
    for req in &raw_fn.requires {
        canon_requires.push(canon_expr(
            req,
            &raw_fn.name,
            &mut env,
            &mut deps_set,
            &mut perform_effects,
            resolve_name,
            resolve_hash,
        )?);
    }

    env.allow_result = true;
    let mut canon_ensures = Vec::with_capacity(raw_fn.ensures.len());
    for ens in &raw_fn.ensures {
        canon_ensures.push(canon_expr(
            ens,
            &raw_fn.name,
            &mut env,
            &mut deps_set,
            &mut perform_effects,
            resolve_name,
            resolve_hash,
        )?);
    }
    env.allow_result = false;

    let canon_body = canon_expr(
        &raw_fn.body,
        &raw_fn.name,
        &mut env,
        &mut deps_set,
        &mut perform_effects,
        resolve_name,
        resolve_hash,
    )?;

    for used_eff in &perform_effects {
        let covered = effects_set.iter().any(|decl| decl.name == *used_eff);
        if !covered {
            return Err(InfimumDiagnostic::new(
                "E021_UNDECLARED_EFFECT",
                "resolve",
                format!(
                    "Function '{}' performs effect '{}' without declaring it in '-[...]->'",
                    raw_fn.name, used_eff
                ),
                Span::zero(),
                format!(
                    "Add '{}' to the effect signature of '{}': '-[{}]-> {}'",
                    used_eff, raw_fn.name, used_eff, raw_fn.ret_type
                ),
            ));
        }
    }

    let canon_effects: Vec<EffectCap> = effects_set.into_iter().collect();

    // Note: `exported` is interface metadata (kept in `ast` for module emitters, excluded from semantic hash text)
    let canon_ast = FnDef {
        name: "_".to_string(),
        exported: raw_fn.exported,
        params: canon_params,
        effects: canon_effects.clone(),
        ret_type: raw_fn.ret_type.clone(),
        requires: canon_requires,
        ensures: canon_ensures,
        body: canon_body,
        source_file: None,
        span: None,
    };

    let canonical_source = format_canonical_fn(&canon_ast);
    let full_hash = blake3::hash(canonical_source.as_bytes())
        .to_hex()
        .to_string();
    let short_hash = full_hash[..12].to_string();

    Ok(CanonicalNode {
        hash: full_hash,
        short_hash,
        canonical_source,
        ast: canon_ast,
        direct_deps: deps_set.into_iter().collect(),
        direct_effects: canon_effects,
        direct_secret_sinks: Vec::new(),
    })
}

fn canon_stmts<FN, FH>(
    stmts: &[Stmt],
    self_name: &str,
    env: &mut ScopeEnv,
    deps: &mut BTreeSet<String>,
    perform_effects: &mut BTreeSet<String>,
    resolve_name: &FN,
    resolve_hash: &FH,
) -> Result<Vec<Stmt>, InfimumDiagnostic>
where
    FN: Fn(&str) -> Result<String, InfimumDiagnostic>,
    FH: Fn(&str) -> Result<String, InfimumDiagnostic>,
{
    let mut cstmts = Vec::with_capacity(stmts.len());
    for s in stmts {
        match s {
            Stmt::Let {
                name,
                mutable,
                ty,
                value,
            } => {
                let cval = canon_expr(
                    value,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?;
                let cname = env.bind_local(name, *mutable);
                cstmts.push(Stmt::Let {
                    name: cname,
                    mutable: *mutable,
                    ty: ty.clone(),
                    value: cval,
                });
            }
            Stmt::LetTuple { names, value } => {
                let cval = canon_expr(
                    value,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?;
                let cnames = names
                    .iter()
                    .map(|(n, m)| (env.bind_local(n, *m), *m))
                    .collect();
                cstmts.push(Stmt::LetTuple {
                    names: cnames,
                    value: cval,
                });
            }
            Stmt::Assign { name, value } => {
                let (cname, is_mut) = env.lookup(name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E020_UNKNOWN_VAR",
                        "resolve",
                        format!("Assignment to unknown variable '{name}' in '{self_name}'"),
                        Span::zero(),
                        "Declare the variable first with 'let mut <name> = ...;'",
                    )
                })?;
                if !is_mut {
                    return Err(InfimumDiagnostic::new(
                        "E067_ASSIGN_IMMUTABLE",
                        "resolve",
                        format!(
                            "Cannot assign to immutable variable '{name}' in function '{self_name}'"
                        ),
                        Span::zero(),
                        format!("Declare '{name}' as mutable with 'let mut {name} = ...;'"),
                    ));
                }
                let cval = canon_expr(
                    value,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?;
                cstmts.push(Stmt::Assign {
                    name: cname,
                    value: cval,
                });
            }
            Stmt::While {
                cond,
                max_iters,
                body,
            } => {
                let ccond = canon_expr(
                    cond,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?;
                env.push_frame();
                let cbody = canon_stmts(
                    body,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?;
                env.pop_frame();
                cstmts.push(Stmt::While {
                    cond: ccond,
                    max_iters: *max_iters,
                    body: cbody,
                });
            }
            Stmt::Expr(e) => {
                cstmts.push(Stmt::Expr(canon_expr(
                    e,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?));
            }
        }
    }
    Ok(cstmts)
}

fn canon_expr<FN, FH>(
    expr: &Expr,
    self_name: &str,
    env: &mut ScopeEnv,
    deps: &mut BTreeSet<String>,
    perform_effects: &mut BTreeSet<String>,
    resolve_name: &FN,
    resolve_hash: &FH,
) -> Result<Expr, InfimumDiagnostic>
where
    FN: Fn(&str) -> Result<String, InfimumDiagnostic>,
    FH: Fn(&str) -> Result<String, InfimumDiagnostic>,
{
    match expr {
        Expr::IntLit(v) => Ok(Expr::IntLit(*v)),
        Expr::FixedLit(v) => Ok(Expr::FixedLit(*v)),
        Expr::BoolLit(b) => Ok(Expr::BoolLit(*b)),
        Expr::StrLit(s) => Ok(Expr::StrLit(s.clone())),
        Expr::UnitLit => Ok(Expr::UnitLit),
        Expr::Var(name) => {
            let (cname, _) = env.lookup(name).ok_or_else(|| {
                InfimumDiagnostic::new(
                    "E020_UNKNOWN_VAR",
                    "resolve",
                    format!("Unknown variable '{name}' in function '{self_name}'"),
                    Span::zero(),
                    "Bind the variable with 'let' or pass it as an explicit function parameter (no globals allowed)",
                )
            })?;
            Ok(Expr::Var(cname))
        }
        Expr::Unary { op, expr: inner } => Ok(Expr::Unary {
            op: *op,
            expr: Box::new(canon_expr(
                inner,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
        }),
        Expr::Binary { op, lhs, rhs } => Ok(Expr::Binary {
            op: *op,
            lhs: Box::new(canon_expr(
                lhs,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
            rhs: Box::new(canon_expr(
                rhs,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
        }),
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => Ok(Expr::If {
            cond: Box::new(canon_expr(
                cond,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
            then_branch: Box::new(canon_expr(
                then_branch,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
            else_branch: Box::new(canon_expr(
                else_branch,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
        }),
        Expr::Call { target, args } => {
            let canon_target = match target {
                TargetRef::SelfRec => TargetRef::SelfRec,
                TargetRef::Named(name) if name == self_name => TargetRef::SelfRec,
                TargetRef::Named(name) => {
                    let h = resolve_name(name)?;
                    deps.insert(h.clone());
                    TargetRef::Hash(h)
                }
                TargetRef::Hash(prefix) => {
                    let h = resolve_hash(prefix)?;
                    deps.insert(h.clone());
                    TargetRef::Hash(h)
                }
            };
            let mut cargs = Vec::with_capacity(args.len());
            for a in args {
                cargs.push(canon_expr(
                    a,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?);
            }
            Ok(Expr::Call {
                target: canon_target,
                args: cargs,
            })
        }
        Expr::Perform { effect, op, args } => {
            perform_effects.insert(effect.clone());
            let mut cargs = Vec::with_capacity(args.len());
            for a in args {
                cargs.push(canon_expr(
                    a,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?);
            }
            Ok(Expr::Perform {
                effect: effect.clone(),
                op: op.clone(),
                args: cargs,
            })
        }
        Expr::MemLoad { index, inv, fixed_ty } => {
            perform_effects.insert("Mem".to_string());
            Ok(Expr::MemLoad {
                index: Box::new(canon_expr(
                    index,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?),
                inv: *inv,
                fixed_ty: *fixed_ty,
            })
        }
        Expr::MemStore { index, value, inv, fixed_ty } => {
            perform_effects.insert("Mem".to_string());
            Ok(Expr::MemStore {
                index: Box::new(canon_expr(
                    index,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?),
                value: Box::new(canon_expr(
                    value,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?),
                inv: *inv,
                fixed_ty: *fixed_ty,
            })
        }
        Expr::SecretWrap(inner) => Ok(Expr::SecretWrap(Box::new(canon_expr(
            inner,
            self_name,
            env,
            deps,
            perform_effects,
            resolve_name,
            resolve_hash,
        )?))),
        Expr::Reveal { domain, expr: inner } => Ok(Expr::Reveal {
            domain: domain.clone(),
            expr: Box::new(canon_expr(
                inner,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?),
        }),
        Expr::Block { stmts, tail } => {
            env.push_frame();
            let cstmts = canon_stmts(
                stmts,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?;
            let ctail = canon_expr(
                tail,
                self_name,
                env,
                deps,
                perform_effects,
                resolve_name,
                resolve_hash,
            )?;
            env.pop_frame();

            if cstmts.is_empty() {
                Ok(ctail)
            } else {
                Ok(Expr::Block {
                    stmts: cstmts,
                    tail: Box::new(ctail),
                })
            }
        }
        Expr::TupleLit(elems) => {
                let mut celems = Vec::with_capacity(elems.len());
                for e in elems {
                    celems.push(canon_expr(
                        e,
                        self_name,
                        env,
                        deps,
                        perform_effects,
                        resolve_name,
                        resolve_hash,
                    )?);
                }
                Ok(Expr::TupleLit(celems))
            }
            Expr::TupleField { expr, index } => Ok(Expr::TupleField {
                expr: Box::new(canon_expr(
                    expr,
                    self_name,
                    env,
                    deps,
                    perform_effects,
                    resolve_name,
                    resolve_hash,
                )?),
                index: *index,
            }),
    }
}

pub fn format_canonical_fn(f: &FnDef) -> String {
    let mut out = String::new();
    out.push_str("fn _ (");
    for (i, p) in f.params.iter().enumerate() {
        if i > 0 {
            out.push_str(", ");
        }
        out.push_str(&p.name);
        out.push(':');
        format_type(&p.ty, &mut out);
    }
    out.push(')');

    if f.effects.is_empty() {
        out.push_str(" -> ");
    } else {
        out.push_str(" -[");
        for (i, eff) in f.effects.iter().enumerate() {
            if i > 0 {
                out.push_str(", ");
            }
            out.push_str(&eff.to_string());
        }
        out.push_str("]-> ");
    }
    format_type(&f.ret_type, &mut out);

    for req in &f.requires {
        out.push_str(" req ");
        format_expr(req, &mut out);
    }
    for ens in &f.ensures {
        out.push_str(" ens ");
        format_expr(ens, &mut out);
    }

    out.push_str(" { ");
    format_body(&f.body, &mut out);
    out.push_str(" }");
    out
}

fn format_type(ty: &Type, out: &mut String) {
    out.push_str(&ty.to_string());
}

fn format_stmt(s: &Stmt, out: &mut String) {
    match s {
        Stmt::Let {
            name,
            mutable,
            ty,
            value,
        } => {
            out.push_str(if *mutable { "let mut " } else { "let " });
            out.push_str(name);
            if let Some(t) = ty {
                out.push(':');
                format_type(t, out);
            }
            out.push_str(" = ");
            format_expr(value, out);
            out.push_str("; ");
        }
        Stmt::LetTuple { names, value } => {
            out.push_str("let (");
            for (i, (n, m)) in names.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if *m {
                    out.push_str("mut ");
                }
                out.push_str(n);
            }
            out.push_str(") = ");
            format_expr(value, out);
            out.push_str("; ");
        }
        Stmt::Assign { name, value } => {
            out.push_str(name);
            out.push_str(" = ");
            format_expr(value, out);
            out.push_str("; ");
        }
        Stmt::While {
            cond,
            max_iters,
            body,
        } => {
            out.push_str("while ");
            format_expr(cond, out);
            out.push_str(&format!(" max {max_iters} {{ "));
            for inner in body {
                format_stmt(inner, out);
            }
            out.push_str("} ");
        }
        Stmt::Expr(e) => {
            format_expr(e, out);
            out.push_str("; ");
        }
    }
}

fn format_body(expr: &Expr, out: &mut String) {
    if let Expr::Block { stmts, tail } = expr {
        for s in stmts {
            format_stmt(s, out);
        }
        format_expr(tail, out);
    } else {
        format_expr(expr, out);
    }
}

pub fn format_expr_human(expr: &Expr) -> String {
    let mut out = String::new();
    format_expr(expr, &mut out);
    out
}

fn format_expr(expr: &Expr, out: &mut String) {
    match expr {
        Expr::IntLit(v) => out.push_str(&v.to_string()),
        Expr::FixedLit(v) => out.push_str(&format!("fixed_raw({v})")),
        Expr::BoolLit(b) => out.push_str(if *b { "true" } else { "false" }),
        Expr::StrLit(s) => {
            out.push('"');
            for ch in s.chars() {
                match ch {
                    '\n' => out.push_str("\\n"),
                    '\t' => out.push_str("\\t"),
                    '\r' => out.push_str("\\r"),
                    '"' => out.push_str("\\\""),
                    '\\' => out.push_str("\\\\"),
                    other => out.push(other),
                }
            }
            out.push('"');
        }
        Expr::UnitLit => out.push_str("()"),
        Expr::Var(v) => out.push_str(v),
        Expr::Unary { op, expr: inner } => {
            out.push('(');
            out.push_str(op.as_str());
            format_expr(inner, out);
            out.push(')');
        }
        Expr::Binary { op, lhs, rhs } => {
            out.push('(');
            format_expr(lhs, out);
            out.push(' ');
            out.push_str(op.as_str());
            out.push(' ');
            format_expr(rhs, out);
            out.push(')');
        }
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            out.push_str("if ");
            format_expr(cond, out);
            out.push_str(" { ");
            format_body(then_branch, out);
            out.push_str(" } else { ");
            format_body(else_branch, out);
            out.push_str(" }");
        }
        Expr::Call { target, args } => {
            match target {
                TargetRef::SelfRec => out.push_str("$self"),
                TargetRef::Hash(h) => {
                    out.push('#');
                    out.push_str(h);
                }
                TargetRef::Named(n) => out.push_str(n),
            }
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(a, out);
            }
            out.push(')');
        }
        Expr::Perform { effect, op, args } => {
            out.push_str("do ");
            out.push_str(effect);
            out.push('.');
            out.push_str(op);
            out.push('(');
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(a, out);
            }
            out.push(')');
        }
        Expr::MemLoad { index, .. } => {
            out.push_str("mem_load(");
            format_expr(index, out);
            out.push(')');
        }
        Expr::MemStore { index, value, .. } => {
            out.push_str("mem_store(");
            format_expr(index, out);
            out.push_str(", ");
            format_expr(value, out);
            out.push(')');
        }
        Expr::SecretWrap(inner) => {
            out.push_str("secret(");
            format_expr(inner, out);
            out.push(')');
        }
        Expr::Reveal { domain, expr: inner } => {
            out.push_str("reveal(\"");
            out.push_str(&domain.escape_default().to_string());
            out.push_str("\", ");
            format_expr(inner, out);
            out.push(')');
        }
        Expr::Block { .. } => {
            out.push_str("{ ");
            format_body(expr, out);
            out.push_str(" }");
        }
        Expr::TupleLit(elems) => {
            out.push('(');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                format_expr(e, out);
            }
            out.push(')');
        }
        Expr::TupleField { expr, index } => {
            format_expr(expr, out);
            out.push('.');
            out.push_str(&index.to_string());
        }
    }
}
