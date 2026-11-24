use crate::ast::{BinOp, Expr, FnDef, Module, Stmt, TargetRef, UnaryOp};
use serde::Serialize;
use std::collections::BTreeMap;

#[derive(Debug, Clone, Serialize)]
pub struct TestCaseResult {
    pub name: String,
    pub passed: bool,
    pub fail_line: i64,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
enum Val {
    Int(i64),
    Bool(bool),
    Str(String),
    Unit,
    Tuple(Vec<Val>),
}

impl Val {
    fn as_int(&self) -> Result<i64, String> {
        match self {
            Val::Int(v) => Ok(*v),
            Val::Bool(b) => Ok(if *b { 1 } else { 0 }),
            other => Err(format!("Expected i64, got {other:?}")),
        }
    }

    fn as_bool(&self) -> Result<bool, String> {
        match self {
            Val::Bool(b) => Ok(*b),
            Val::Int(v) => Ok(*v != 0),
            other => Err(format!("Expected bool, got {other:?}")),
        }
    }
}

struct Evaluator<'a> {
    fns: BTreeMap<&'a str, &'a FnDef>,
    mem: Vec<i64>,
    str_pool: BTreeMap<String, i64>,
    str_pool_cursor: i64,
    steps: usize,
}

const MAX_EVAL_STEPS: usize = 5_000_000;
const EVAL_STR_POOL_BASE: i64 = 1_500_000;

impl<'a> Evaluator<'a> {
    fn new(module: &'a Module) -> Self {
        let mut fns = BTreeMap::new();
        for f in &module.functions {
            fns.insert(f.name.as_str(), f);
        }
        Self {
            fns,
            mem: vec![0; 65536],
            str_pool: BTreeMap::new(),
            str_pool_cursor: EVAL_STR_POOL_BASE,
            steps: 0,
        }
    }

    fn intern_str(&mut self, s: &str) -> Result<i64, String> {
        if let Some(&ptr) = self.str_pool.get(s) {
            return Ok(ptr);
        }
        let chars: Vec<i64> = s.chars().map(|c| c as i64).collect();
        let ptr = self.str_pool_cursor;
        let end = ptr + 1 + chars.len() as i64;
        if end > 2_000_000 {
            return Err("Test evaluator string pool exhausted".to_string());
        }
        if (end as usize) > self.mem.len() {
            self.mem.resize(end as usize, 0);
        }
        self.mem[ptr as usize] = chars.len() as i64;
        for (i, ch) in chars.iter().enumerate() {
            self.mem[(ptr as usize) + 1 + i] = *ch;
        }
        self.str_pool_cursor = end + 1;
        self.str_pool.insert(s.to_string(), ptr);
        Ok(ptr)
    }

    fn tick(&mut self) -> Result<(), String> {
        self.steps += 1;
        if self.steps > MAX_EVAL_STEPS {
            return Err("Test exceeded maximum evaluation step budget (5,000,000)".to_string());
        }
        Ok(())
    }

    fn call_fn(&mut self, name: &str, args: Vec<Val>) -> Result<Val, String> {
        let f = *self
            .fns
            .get(name)
            .ok_or_else(|| format!("Unknown function '{name}' during test evaluation"))?;
        if f.params.len() != args.len() {
            return Err(format!(
                "Function '{name}' expects {} args, got {}",
                f.params.len(),
                args.len()
            ));
        }
        let mut scopes: Vec<BTreeMap<String, Val>> = vec![BTreeMap::new()];
        for (p, v) in f.params.iter().zip(args) {
            scopes[0].insert(p.name.clone(), v);
        }
        self.eval_expr(&f.body, &mut scopes, name)
    }

    fn lookup(scopes: &[BTreeMap<String, Val>], name: &str) -> Result<Val, String> {
        for sc in scopes.iter().rev() {
            if let Some(v) = sc.get(name) {
                return Ok(v.clone());
            }
        }
        Err(format!("Undefined variable '{name}'"))
    }

    fn assign(scopes: &mut [BTreeMap<String, Val>], name: &str, val: Val) -> Result<(), String> {
        for sc in scopes.iter_mut().rev() {
            if sc.contains_key(name) {
                sc.insert(name.to_string(), val);
                return Ok(());
            }
        }
        Err(format!("Undefined variable '{name}' in assignment"))
    }

    fn eval_stmt(
        &mut self,
        stmt: &Stmt,
        scopes: &mut Vec<BTreeMap<String, Val>>,
        cur_fn: &str,
    ) -> Result<(), String> {
        self.tick()?;
        match stmt {
            Stmt::Let { name, value, .. } => {
                let v = self.eval_expr(value, scopes, cur_fn)?;
                if let Some(top) = scopes.last_mut() {
                    top.insert(name.clone(), v);
                }
                Ok(())
            }
            Stmt::LetTuple { names, value } => {
                let v = self.eval_expr(value, scopes, cur_fn)?;
                let Val::Tuple(elems) = v else {
                    return Err(format!("Expected tuple value in let destructuring, got {v:?}"));
                };
                if elems.len() != names.len() {
                    return Err(format!(
                        "Tuple arity mismatch: expected {}, got {}",
                        names.len(),
                        elems.len()
                    ));
                }
                if let Some(top) = scopes.last_mut() {
                    for ((n, _), ev) in names.iter().zip(elems) {
                        top.insert(n.clone(), ev);
                    }
                }
                Ok(())
            }
            Stmt::Assign { name, value } => {
                let v = self.eval_expr(value, scopes, cur_fn)?;
                Self::assign(scopes, name, v)
            }
            Stmt::While {
                cond,
                max_iters,
                body,
            } => {
                for _ in 0..*max_iters {
                    self.tick()?;
                    if !self.eval_expr(cond, scopes, cur_fn)?.as_bool()? {
                        break;
                    }
                    scopes.push(BTreeMap::new());
                    for s in body {
                        let res = self.eval_stmt(s, scopes, cur_fn);
                        if res.is_err() {
                            scopes.pop();
                            return res;
                        }
                    }
                    scopes.pop();
                }
                Ok(())
            }
            Stmt::Expr(e) => {
                let _ = self.eval_expr(e, scopes, cur_fn)?;
                Ok(())
            }
        }
    }

    fn eval_expr(
        &mut self,
        expr: &Expr,
        scopes: &mut Vec<BTreeMap<String, Val>>,
        cur_fn: &str,
    ) -> Result<Val, String> {
        self.tick()?;
        match expr {
            Expr::IntLit(v) | Expr::FixedLit(v) => Ok(Val::Int(*v)),
            Expr::BoolLit(b) => Ok(Val::Bool(*b)),
            Expr::StrLit(s) => Ok(Val::Str(s.clone())),
            Expr::UnitLit => Ok(Val::Unit),
            Expr::Var(name) => Self::lookup(scopes, name),
            Expr::Unary { op, expr: inner } => {
                let v = self.eval_expr(inner, scopes, cur_fn)?;
                match op {
                    UnaryOp::Neg => Ok(Val::Int(v.as_int()?.wrapping_neg())),
                    UnaryOp::Not => Ok(Val::Bool(!v.as_bool()?)),
                    UnaryOp::IntToFixed => Ok(Val::Int(v.as_int()?.wrapping_mul(65536))),
                    UnaryOp::FixedToInt => Ok(Val::Int(v.as_int()? / 65536)),
                    UnaryOp::FixedToRaw | UnaryOp::RawToFixed => Ok(Val::Int(v.as_int()?)),
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                match op {
                    BinOp::And => {
                        let l = self.eval_expr(lhs, scopes, cur_fn)?.as_bool()?;
                        if !l {
                            return Ok(Val::Bool(false));
                        }
                        let r = self.eval_expr(rhs, scopes, cur_fn)?.as_bool()?;
                        return Ok(Val::Bool(r));
                    }
                    BinOp::Or => {
                        let l = self.eval_expr(lhs, scopes, cur_fn)?.as_bool()?;
                        if l {
                            return Ok(Val::Bool(true));
                        }
                        let r = self.eval_expr(rhs, scopes, cur_fn)?.as_bool()?;
                        return Ok(Val::Bool(r));
                    }
                    _ => {}
                }
                let lv = self.eval_expr(lhs, scopes, cur_fn)?;
                let rv = self.eval_expr(rhs, scopes, cur_fn)?;
                match op {
                    BinOp::Add => Ok(Val::Int(lv.as_int()?.wrapping_add(rv.as_int()?))),
                    BinOp::Sub => Ok(Val::Int(lv.as_int()?.wrapping_sub(rv.as_int()?))),
                    BinOp::Mul => Ok(Val::Int(lv.as_int()?.wrapping_mul(rv.as_int()?))),
                    BinOp::Div => {
                        let d = rv.as_int()?;
                        if d == 0 {
                            return Err("Division by zero during test".to_string());
                        }
                        Ok(Val::Int(lv.as_int()?.wrapping_div(d)))
                    }
                    BinOp::FixedMul => {
                        let prod = (lv.as_int()? as i128 * rv.as_int()? as i128) / 65536;
                        Ok(Val::Int(prod as i64))
                    }
                    BinOp::FixedDiv => {
                        let d = rv.as_int()?;
                        if d == 0 {
                            return Err("Fixed-point division by zero during test".to_string());
                        }
                        let q = ((lv.as_int()? as i128) * 65536) / (d as i128);
                        Ok(Val::Int(q as i64))
                    }
                    BinOp::Mod => {
                        let d = rv.as_int()?;
                        if d == 0 {
                            return Err("Modulo by zero during test".to_string());
                        }
                        Ok(Val::Int(lv.as_int()?.wrapping_rem(d)))
                    }
                    BinOp::BitAnd => Ok(Val::Int(lv.as_int()? & rv.as_int()?)),
                    BinOp::BitOr => Ok(Val::Int(lv.as_int()? | rv.as_int()?)),
                    BinOp::BitXor => Ok(Val::Int(lv.as_int()? ^ rv.as_int()?)),
                    BinOp::Shl => Ok(Val::Int(lv.as_int()?.wrapping_shl((rv.as_int()? & 63) as u32))),
                    BinOp::Shr => Ok(Val::Int(lv.as_int()?.wrapping_shr((rv.as_int()? & 63) as u32))),
                    BinOp::Eq => match (&lv, &rv) {
                        (Val::Bool(a), Val::Bool(b)) => Ok(Val::Bool(a == b)),
                        (Val::Str(a), Val::Str(b)) => Ok(Val::Bool(a == b)),
                        _ => Ok(Val::Bool(lv.as_int()? == rv.as_int()?)),
                    },
                    BinOp::Ne => match (&lv, &rv) {
                        (Val::Bool(a), Val::Bool(b)) => Ok(Val::Bool(a != b)),
                        (Val::Str(a), Val::Str(b)) => Ok(Val::Bool(a != b)),
                        _ => Ok(Val::Bool(lv.as_int()? != rv.as_int()?)),
                    },
                    BinOp::Lt => Ok(Val::Bool(lv.as_int()? < rv.as_int()?)),
                    BinOp::Le => Ok(Val::Bool(lv.as_int()? <= rv.as_int()?)),
                    BinOp::Gt => Ok(Val::Bool(lv.as_int()? > rv.as_int()?)),
                    BinOp::Ge => Ok(Val::Bool(lv.as_int()? >= rv.as_int()?)),
                    BinOp::And | BinOp::Or => unreachable!(),
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                if self.eval_expr(cond, scopes, cur_fn)?.as_bool()? {
                    self.eval_expr(then_branch, scopes, cur_fn)
                } else {
                    self.eval_expr(else_branch, scopes, cur_fn)
                }
            }
            Expr::Call { target, args } => {
                let mut vals = Vec::with_capacity(args.len());
                for a in args {
                    vals.push(self.eval_expr(a, scopes, cur_fn)?);
                }
                let callee = match target {
                    TargetRef::Named(n) => n.as_str(),
                    TargetRef::SelfRec => cur_fn,
                    TargetRef::Hash(h) => {
                        return Err(format!("Hash call #{h} is not supported in test evaluator"))
                    }
                };
                self.call_fn(callee, vals)
            }
            Expr::Perform { effect, op, args } => {
                if effect == "Mem" && op == "store_str" && args.len() == 2 {
                    let slot = self.eval_expr(&args[0], scopes, cur_fn)?.as_int()?;
                    let val = self.eval_expr(&args[1], scopes, cur_fn)?;
                    if !(0..=2_000_000).contains(&slot) {
                        return Err(format!("Out-of-bounds Mem.store_str({slot})"));
                    }
                    let ptr = match val {
                        Val::Str(s) => self.intern_str(&s)?,
                        Val::Int(p) => p,
                        other => return Err(format!("Mem.store_str expected str, got {other:?}")),
                    };
                    let u = slot as usize;
                    if u >= self.mem.len() {
                        self.mem.resize(u + 1, 0);
                    }
                    self.mem[u] = ptr;
                    Ok(Val::Unit)
                } else {
                    for a in args {
                        let _ = self.eval_expr(a, scopes, cur_fn)?;
                    }
                    Ok(Val::Unit)
                }
            }
            Expr::MemLoad { index, .. } => {
                let idx = self.eval_expr(index, scopes, cur_fn)?.as_int()?;
                if !(0..=2_000_000).contains(&idx) {
                    return Err(format!("Out-of-bounds mem_load({idx})"));
                }
                let u = idx as usize;
                Ok(Val::Int(if u < self.mem.len() { self.mem[u] } else { 0 }))
            }
            Expr::MemStore { index, value, .. } => {
                let idx = self.eval_expr(index, scopes, cur_fn)?.as_int()?;
                let val = self.eval_expr(value, scopes, cur_fn)?.as_int()?;
                if !(0..=2_000_000).contains(&idx) {
                    return Err(format!("Out-of-bounds mem_store({idx})"));
                }
                let u = idx as usize;
                if u >= self.mem.len() {
                    self.mem.resize(u + 1, 0);
                }
                self.mem[u] = val;
                Ok(Val::Unit)
            }
            Expr::SecretWrap(inner) | Expr::Reveal { expr: inner, .. } => {
                self.eval_expr(inner, scopes, cur_fn)
            }
            Expr::Block { stmts, tail } => {
                scopes.push(BTreeMap::new());
                for s in stmts {
                    if let Err(e) = self.eval_stmt(s, scopes, cur_fn) {
                        scopes.pop();
                        return Err(e);
                    }
                }
                let res = self.eval_expr(tail, scopes, cur_fn);
                scopes.pop();
                res
            }
            Expr::TupleLit(elems) => {
                let mut vals = Vec::with_capacity(elems.len());
                for e in elems {
                    vals.push(self.eval_expr(e, scopes, cur_fn)?);
                }
                Ok(Val::Tuple(vals))
            }
            Expr::TupleField { expr, index } => {
                let v = self.eval_expr(expr, scopes, cur_fn)?;
                match v {
                    Val::Tuple(elems) => elems.get(*index).cloned().ok_or_else(|| {
                        format!("Tuple index .{index} out of range (len {})", elems.len())
                    }),
                    other => Err(format!("Expected tuple for .{index}, got {other:?}")),
                }
            }
        }
    }
}

pub fn run_module_tests(module: &Module) -> Vec<TestCaseResult> {
    let mut results = Vec::with_capacity(module.tests.len());
    for (fn_name, display_name) in &module.tests {
        let mut ev = Evaluator::new(module);
        match ev.call_fn(fn_name, vec![]) {
            Ok(v) => match v.as_int() {
                Ok(0) => results.push(TestCaseResult {
                    name: display_name.clone(),
                    passed: true,
                    fail_line: 0,
                    error: None,
                }),
                Ok(line) => results.push(TestCaseResult {
                    name: display_name.clone(),
                    passed: false,
                    fail_line: line,
                    error: Some(format!("Assertion failed at line {line}")),
                }),
                Err(e) => results.push(TestCaseResult {
                    name: display_name.clone(),
                    passed: false,
                    fail_line: 0,
                    error: Some(e),
                }),
            },
            Err(e) => results.push(TestCaseResult {
                name: display_name.clone(),
                passed: false,
                fail_line: 0,
                error: Some(e),
            }),
        }
    }
    results
}
