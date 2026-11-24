use crate::ast::{BinOp, Expr, FnDef, Param, Stmt, TargetRef, Type, UnaryOp};
use crate::canon::CanonicalNode;
use crate::diag::{InfimumDiagnostic, Span};
use crate::store::GraphStore;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, VecDeque};
use std::env;
use std::fs;
#[cfg(unix)]
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::Instant;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BuildArtifact {
    pub entry_alias_or_ref: String,
    pub root_hash: String,
    pub short_hash: String,
    pub compiled_nodes: usize,
    pub binary_path: String,
    pub binary_size_bytes: u64,
    pub cache_hit: bool,
    pub build_ms: u128,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunOutcome {
    pub artifact: BuildArtifact,
    pub exit_code: i32,
    pub stdout: String,
    pub stderr: String,
    pub run_ms: u128,
}

pub fn collect_reachable_subgraph(
    store: &GraphStore,
    root_hash: &str,
) -> Result<Vec<CanonicalNode>, InfimumDiagnostic> {
    let mut visited: BTreeMap<String, CanonicalNode> = BTreeMap::new();
    let mut queue = VecDeque::new();
    queue.push_back(root_hash.to_string());

    while let Some(h) = queue.pop_front() {
        if visited.contains_key(&h) {
            continue;
        }
        let node = store.load_node(&h)?;
        for dep in &node.direct_deps {
            if !visited.contains_key(dep) {
                queue.push_back(dep.clone());
            }
        }
        visited.insert(h, node);
    }

    Ok(visited.into_values().collect())
}

pub fn emit_native_rust_program(
    store: &GraphStore,
    target_ref: &str,
) -> Result<(String, CanonicalNode, usize), InfimumDiagnostic> {
    let root_hash = store.resolve_ref(target_ref)?;
    let root_node = store.load_node(&root_hash)?;
    let nodes = collect_reachable_subgraph(store, &root_hash)?;
    let node_count = nodes.len();

    let mut rs = String::with_capacity(4096);
    rs.push_str("#![allow(unused_parens, unused_variables, dead_code, clippy::all)]\n");
    rs.push_str("use std::env;\nuse std::fmt;\nuse std::process;\n\n");

    // Zero-cost opaque wrapper ensuring Secret<T> can never be accidentally printed
    rs.push_str(
        r#"#[repr(transparent)]
#[derive(Clone)]
struct InfimumSecret<T>(T);

impl<T> InfimumSecret<T> {
    #[inline(always)]
    fn new(val: T) -> Self {
        Self(val)
    }

    #[inline(always)]
    fn expose_for_pinned_domain(&self, _domain: &'static str) -> &T {
        &self.0
    }
}

impl<T> fmt::Display for InfimumSecret<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "[REDACTED SECRET]")
    }
}

struct TraceEvent {
    step: usize,
    node: &'static str,
    effect: &'static str,
    op: &'static str,
    summary: String,
}

struct InfimumRuntime {
    trace_enabled: bool,
    events: Vec<TraceEvent>,
    mem: Vec<i32>,
}

impl InfimumRuntime {
    fn new(trace_enabled: bool) -> Self {
        Self {
            trace_enabled,
            events: Vec::new(),
            mem: vec![0i32; 1_048_576],
        }
    }

    #[inline]
    fn perform(&mut self, node: &'static str, effect: &'static str, op: &'static str, args_summary: Vec<String>) {
        if self.trace_enabled {
            let step = self.events.len();
            self.events.push(TraceEvent {
                step,
                node,
                effect,
                op,
                summary: args_summary.join(", "),
            });
        }
    }

    fn print_trace(&self) {
        for ev in &self.events {
            println!(
                "[TRACE #{:03}] node=#{} effect={}.{}({})",
                ev.step, ev.node, ev.effect, ev.op, ev.summary
            );
        }
    }
}

trait InfimumFormat {
    fn infimum_display(&self) -> String;
}

impl InfimumFormat for i64 {
    fn infimum_display(&self) -> String { self.to_string() }
}
impl InfimumFormat for bool {
    fn infimum_display(&self) -> String { self.to_string() }
}
impl InfimumFormat for String {
    fn infimum_display(&self) -> String { format!("{:?}", self) }
}
impl InfimumFormat for () {
    fn infimum_display(&self) -> String { "()".to_string() }
}
impl<T> InfimumFormat for InfimumSecret<T> {
    fn infimum_display(&self) -> String { "[REDACTED SECRET]".to_string() }
}

"#,
    );

    // Emit each verified node in the minimal reachable subgraph
    for node in &nodes {
        emit_node_fn(node, &mut rs);
    }

    // Emit CLI entrypoint wrapper with automatic `req` boundary verification
    emit_main_entrypoint(&root_node, target_ref, &mut rs);

    Ok((rs, root_node, node_count))
}

fn rust_fn_ident(hash: &str) -> String {
    format!("ax_{}", &hash[..16])
}

fn rust_type(ty: &Type) -> String {
    match ty {
        Type::I64 | Type::Fixed => "i64".to_string(),
        Type::Bool => "bool".to_string(),
        Type::Str => "String".to_string(),
        Type::Unit => "()".to_string(),
        Type::Secret(inner) => format!("InfimumSecret<{}>", rust_type(inner)),
        Type::Tuple(ts) => {
            let parts: Vec<String> = ts.iter().map(rust_type).collect();
            format!("({})", parts.join(", "))
        }
    }
}

fn emit_node_fn(node: &CanonicalNode, out: &mut String) {
    let f: &FnDef = &node.ast;
    let fn_id = rust_fn_ident(&node.hash);
    out.push_str("#[inline]\nfn ");
    out.push_str(&fn_id);
    out.push_str("(rt: &mut InfimumRuntime");
    for p in &f.params {
        out.push_str(", ");
        out.push_str(&p.name);
        out.push_str(": ");
        out.push_str(&rust_type(&p.ty));
    }
    out.push_str(") -> ");
    out.push_str(&rust_type(&f.ret_type));
    out.push_str(" {\n    ");
    emit_expr(&f.body, &node.hash, &node.short_hash, out);
    out.push_str("\n}\n\n");
}

fn emit_expr(expr: &Expr, self_hash: &str, short_hash: &str, out: &mut String) {
    match expr {
        Expr::IntLit(v) | Expr::FixedLit(v) => {
            out.push_str(&format!("{v}_i64"));
        }
        Expr::BoolLit(b) => {
            out.push_str(if *b { "true" } else { "false" });
        }
        Expr::StrLit(s) => {
            out.push_str(&format!("String::from({s:?})"));
        }
        Expr::UnitLit => {
            out.push_str("()");
        }
        Expr::Var(v) => {
            out.push_str(&format!("{v}.clone()"));
        }
        Expr::SecretWrap(inner) => {
            out.push_str("InfimumSecret::new(");
            emit_expr(inner, self_hash, short_hash, out);
            out.push(')');
        }
        Expr::Reveal { domain, expr: inner } => {
            // Only reachable inside authorized Net.<op>, redacted in public trace
            out.push_str("{ let _ = (");
            emit_expr(inner, self_hash, short_hash, out);
            out.push_str(&format!(
                ").expose_for_pinned_domain({domain:?}); String::from(\"[PINNED SECRET -> {domain}]\") }}"
            ));
        }
        Expr::Unary { op, expr: inner } => match op {
            UnaryOp::IntToFixed => {
                out.push_str("((");
                emit_expr(inner, self_hash, short_hash, out);
                out.push_str(").wrapping_mul(65536_i64))");
            }
            UnaryOp::FixedToInt => {
                out.push_str("((");
                emit_expr(inner, self_hash, short_hash, out);
                out.push_str(") / 65536_i64)");
            }
            UnaryOp::FixedToRaw | UnaryOp::RawToFixed => {
                emit_expr(inner, self_hash, short_hash, out);
            }
            _ => {
                out.push('(');
                out.push_str(op.as_str());
                emit_expr(inner, self_hash, short_hash, out);
                out.push(')');
            }
        },
        Expr::Binary { op, lhs, rhs } => match op {
            BinOp::FixedMul => {
                out.push_str("(((");
                emit_expr(lhs, self_hash, short_hash, out);
                out.push_str(" as i128 * ");
                emit_expr(rhs, self_hash, short_hash, out);
                out.push_str(" as i128) / 65536_i128) as i64)");
            }
            BinOp::FixedDiv => {
                out.push_str("(((");
                emit_expr(lhs, self_hash, short_hash, out);
                out.push_str(" as i128 * 65536_i128) / (");
                emit_expr(rhs, self_hash, short_hash, out);
                out.push_str(" as i128)) as i64)");
            }
            _ => {
                out.push('(');
                emit_expr(lhs, self_hash, short_hash, out);
                out.push(' ');
                out.push_str(op.as_str());
                out.push(' ');
                emit_expr(rhs, self_hash, short_hash, out);
                out.push(')');
            }
        },
        Expr::If {
            cond,
            then_branch,
            else_branch,
        } => {
            out.push_str("if ");
            emit_expr(cond, self_hash, short_hash, out);
            out.push_str(" { ");
            emit_expr(then_branch, self_hash, short_hash, out);
            out.push_str(" } else { ");
            emit_expr(else_branch, self_hash, short_hash, out);
            out.push_str(" }");
        }
        Expr::Call { target, args } => {
            let callee_id = match target {
                TargetRef::SelfRec => rust_fn_ident(self_hash),
                TargetRef::Hash(h) => rust_fn_ident(h),
                TargetRef::Named(n) => format!("ifm_unresolved_{n}"),
            };
            out.push_str(&callee_id);
            out.push_str("(rt");
            for a in args {
                out.push_str(", ");
                emit_expr(a, self_hash, short_hash, out);
            }
            out.push(')');
        }
        Expr::Perform { effect, op, args } => {
            out.push_str(&format!(
                "{{ rt.perform({short_hash:?}, {effect:?}, {op:?}, vec!["
            ));
            for (i, a) in args.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                if matches!(a, Expr::Reveal { .. }) {
                    emit_expr(a, self_hash, short_hash, out);
                } else {
                    out.push('(');
                    emit_expr(a, self_hash, short_hash, out);
                    out.push_str(").infimum_display()");
                }
            }
            out.push_str("]); () }");
        }
        Expr::MemLoad { index, .. } => {
            out.push_str("(rt.mem[((");
            emit_expr(index, self_hash, short_hash, out);
            out.push_str(") as usize) & 0xFFFFF] as i64)");
        }
        Expr::MemStore { index, value, .. } => {
            out.push_str("{ let __idx = ((");
            emit_expr(index, self_hash, short_hash, out);
            out.push_str(") as usize) & 0xFFFFF; rt.mem[__idx] = (");
            emit_expr(value, self_hash, short_hash, out);
            out.push_str(") as i32; () }");
        }
        Expr::Block { stmts, tail } => {
            out.push_str("{\n");
            emit_stmt_list(stmts, self_hash, short_hash, out);
            out.push_str("        ");
            emit_expr(tail, self_hash, short_hash, out);
            out.push_str("\n    }");
        }
        Expr::TupleLit(elems) => {
            out.push('(');
            for (i, e) in elems.iter().enumerate() {
                if i > 0 {
                    out.push_str(", ");
                }
                emit_expr(e, self_hash, short_hash, out);
            }
            out.push(')');
        }
        Expr::TupleField { expr, index } => {
            out.push('(');
            emit_expr(expr, self_hash, short_hash, out);
            out.push_str(&format!(").{index}"));
        }
    }
}

fn emit_stmt_list(stmts: &[Stmt], self_hash: &str, short_hash: &str, out: &mut String) {
    for s in stmts {
        match s {
            Stmt::Let {
                name,
                mutable,
                ty,
                value,
            } => {
                out.push_str(if *mutable {
                    "        let mut "
                } else {
                    "        let "
                });
                out.push_str(name);
                if let Some(t) = ty {
                    out.push_str(": ");
                    out.push_str(&rust_type(t));
                }
                out.push_str(" = ");
                emit_expr(value, self_hash, short_hash, out);
                out.push_str(";\n");
            }
            Stmt::LetTuple { names, value } => {
                out.push_str("        let (");
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
                emit_expr(value, self_hash, short_hash, out);
                out.push_str(";\n");
            }
            Stmt::Assign { name, value } => {
                out.push_str("        ");
                out.push_str(name);
                out.push_str(" = ");
                emit_expr(value, self_hash, short_hash, out);
                out.push_str(";\n");
            }
            Stmt::While {
                cond,
                max_iters,
                body,
            } => {
                out.push_str(&format!(
                    "        {{\n            let mut __fuel: u32 = {max_iters};\n            while __fuel > 0 && ("
                ));
                emit_expr(cond, self_hash, short_hash, out);
                out.push_str(") {\n                __fuel -= 1;\n");
                emit_stmt_list(body, self_hash, short_hash, out);
                out.push_str("            }\n        }\n");
            }
            Stmt::Expr(e) => {
                out.push_str("        let _ = ");
                emit_expr(e, self_hash, short_hash, out);
                out.push_str(";\n");
            }
        }
    }
}

fn emit_cli_arg_parser(idx: usize, param: &Param, out: &mut String) {
    let pname = &param.name;
    out.push_str(&format!("    let {pname}: {} = ", rust_type(&param.ty)));
    emit_parse_expr(&format!("&cli_args[{idx}]"), &param.ty, idx + 1, out);
    out.push_str(";\n");
}

fn emit_parse_expr(src_expr: &str, ty: &Type, arg_num: usize, out: &mut String) {
    match ty {
        Type::I64 | Type::Fixed => out.push_str(&format!(
            "({src_expr}).parse::<i64>().unwrap_or_else(|_| {{ eprintln!(\"Invalid i64 for arg #{arg_num}: {{}}\", {src_expr}); process::exit(2); }})"
        )),
        Type::Bool => out.push_str(&format!(
            "({src_expr}).parse::<bool>().unwrap_or_else(|_| {{ eprintln!(\"Invalid bool for arg #{arg_num}: {{}}\", {src_expr}); process::exit(2); }})"
        )),
        Type::Str => out.push_str(&format!("({src_expr}).to_string()")),
        Type::Unit => out.push_str("()"),
        Type::Secret(inner) => {
            out.push_str("InfimumSecret::new(");
            emit_parse_expr(src_expr, inner, arg_num, out);
            out.push(')');
        }
        Type::Tuple(_) => out.push_str("()"),
    }
}

fn emit_main_entrypoint(root: &CanonicalNode, target_label: &str, out: &mut String) {
    let f = &root.ast;
    let expected_arity = f.params.len();
    let fn_id = rust_fn_ident(&root.hash);

    out.push_str("fn main() {\n");
    out.push_str("    let raw_args: Vec<String> = env::args().skip(1).collect();\n");
    out.push_str("    let trace_enabled = raw_args.iter().any(|a| a == \"--trace\");\n");
    out.push_str(
        "    let cli_args: Vec<String> = raw_args.into_iter().filter(|a| a != \"--trace\").collect();\n",
    );
    out.push_str(&format!(
        "    if cli_args.len() != {expected_arity} {{\n        eprintln!(\"Usage: <binary> [--trace] ({expected_arity} argument(s) expected for '{target_label}', got {{}})\", cli_args.len());\n        process::exit(2);\n    }}\n"
    ));

    for (i, p) in f.params.iter().enumerate() {
        emit_cli_arg_parser(i, p, out);
    }

    out.push_str("    let mut rt = InfimumRuntime::new(trace_enabled);\n");

    // Boundary check: verify external CLI arguments satisfy root `req` contracts!
    for (i, req_expr) in f.requires.iter().enumerate() {
        let req_desc = crate::canon::format_expr_human(req_expr);
        out.push_str("    if !(");
        emit_expr(req_expr, &root.hash, &root.short_hash, out);
        out.push_str(&format!(
            ") {{\n        eprintln!(\"Boundary Precondition Violation: req #{} ({{}}) failed for CLI inputs\", {req_desc:?});\n        process::exit(2);\n    }}\n",
            i + 1
        ));
    }

    out.push_str(&format!("    let out = {fn_id}(&mut rt"));
    for p in &f.params {
        out.push_str(", ");
        out.push_str(&p.name);
    }
    out.push_str(");\n");
    out.push_str("    if trace_enabled {\n        rt.print_trace();\n    }\n");
    if f.ret_type != Type::Unit {
        out.push_str("    println!(\"{}\", out);\n");
    }
    out.push_str("}\n");
}

pub fn build_native_binary(
    store: &GraphStore,
    workspace_root: &Path,
    target_ref: &str,
    explicit_out: Option<&Path>,
) -> Result<BuildArtifact, InfimumDiagnostic> {
    let t0 = Instant::now();
    let (rust_src, root_node, compiled_nodes) = emit_native_rust_program(store, target_ref)?;

    let cache_dir = workspace_root.join(".infimum").join("bin");
    fs::create_dir_all(&cache_dir).map_err(|e| {
        InfimumDiagnostic::new(
            "E070_BUILD_IO",
            "codegen",
            format!("Cannot create .infimum/bin directory: {e}"),
            Span::zero(),
            "Check write permissions in workspace",
        )
    })?;

    let cached_bin = cache_dir.join(&root_node.hash);
    let cache_hit = cached_bin.is_file();

    if !cache_hit {
        let src_path = cache_dir.join(format!("{}.rs", root_node.short_hash));
        fs::write(&src_path, &rust_src).map_err(|e| {
            InfimumDiagnostic::new(
                "E070_BUILD_IO",
                "codegen",
                format!("Failed to write intermediate codegen source: {e}"),
                Span::zero(),
                "Check disk space",
            )
        })?;

        let rustc = find_rustc_binary();
        let output = Command::new(&rustc)
            .args([
                "--edition=2021",
                "-O",
                "-C",
                "target-cpu=native",
                "-C",
                "strip=symbols",
                src_path.to_str().unwrap_or_default(),
                "-o",
                cached_bin.to_str().unwrap_or_default(),
            ])
            .output()
            .map_err(|e| {
                InfimumDiagnostic::new(
                    "E071_RUSTC_EXEC",
                    "codegen",
                    format!("Failed to invoke rustc ('{}'): {e}", rustc.display()),
                    Span::zero(),
                    "Ensure rustc is installed via rustup (~/.cargo/bin/rustc)",
                )
            })?;

        let _ = fs::remove_file(&src_path);

        if !output.status.success() {
            let stderr = String::from_utf8_lossy(&output.stderr);
            return Err(InfimumDiagnostic::new(
                "E072_BACKEND_COMPILE_FAILED",
                "codegen",
                format!("Backend native compilation failed: {}", stderr.trim()),
                Span::zero(),
                "Inspect generated types and expressions",
            ));
        }
    }

    let final_path = if let Some(out_p) = explicit_out {
        if let Some(parent) = out_p.parent() {
            if !parent.as_os_str().is_empty() {
                let _ = fs::create_dir_all(parent);
            }
        }
        fs::copy(&cached_bin, out_p).map_err(|e| {
            InfimumDiagnostic::new(
                "E070_BUILD_IO",
                "codegen",
                format!("Failed to copy binary to '{}': {e}", out_p.display()),
                Span::zero(),
                "Verify output path permissions",
            )
        })?;
        #[cfg(unix)]
        {
            if let Ok(meta) = fs::metadata(out_p) {
                let mut perms = meta.permissions();
                perms.set_mode(0o755);
                let _ = fs::set_permissions(out_p, perms);
            }
        }
        out_p.to_path_buf()
    } else {
        cached_bin
    };

    let size = fs::metadata(&final_path).map(|m| m.len()).unwrap_or(0);

    Ok(BuildArtifact {
        entry_alias_or_ref: target_ref.to_string(),
        root_hash: root_node.hash,
        short_hash: root_node.short_hash,
        compiled_nodes,
        binary_path: final_path.display().to_string(),
        binary_size_bytes: size,
        cache_hit,
        build_ms: t0.elapsed().as_millis(),
    })
}

pub fn run_native_binary(
    store: &GraphStore,
    workspace_root: &Path,
    target_ref: &str,
    prog_args: &[&str],
) -> Result<RunOutcome, InfimumDiagnostic> {
    let artifact = build_native_binary(store, workspace_root, target_ref, None)?;
    let t_run = Instant::now();

    let output = Command::new(&artifact.binary_path)
        .args(prog_args)
        .output()
        .map_err(|e| {
            InfimumDiagnostic::new(
                "E073_RUN_EXEC_FAILED",
                "codegen",
                format!("Failed to execute binary '{}': {e}", artifact.binary_path),
                Span::zero(),
                "Check execute permissions on .infimum/bin",
            )
        })?;

    Ok(RunOutcome {
        artifact,
        exit_code: output.status.code().unwrap_or(-1),
        stdout: String::from_utf8_lossy(&output.stdout).to_string(),
        stderr: String::from_utf8_lossy(&output.stderr).to_string(),
        run_ms: t_run.elapsed().as_millis(),
    })
}

fn find_rustc_binary() -> PathBuf {
    if let Ok(custom) = env::var("RUSTC") {
        let p = PathBuf::from(custom);
        if p.is_file() {
            return p;
        }
    }
    if let Some(home) = env::var_os("HOME") {
        let cargo_rustc = PathBuf::from(home).join(".cargo/bin/rustc");
        if cargo_rustc.is_file() {
            return cargo_rustc;
        }
    }
    PathBuf::from("rustc")
}
