// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

//! Z3 SMT geometry verification for `Infimum.Forma` (`Telos x Forma`).
//!
//! Proves four formal UI invariants for every `forma` block under its `req` preconditions:
//! 1. Non-degeneracy: all visible interactive nodes (`Button`, `Dropdown`, `Scroll`) have `rw > 0 && rh > 0`.
//! 2. No sibling overlap: adjacent children in `Row` (`a.x + a.w <= b.x`) and `Col` (`a.y + a.h <= b.y`).
//! 3. Bounds & clip containment: flow children inside non-`Scroll` containers stay within inner padded
//!    parent bounds, and nested hardware `clip` rectangles stay within parent clip rectangles.
//! 4. Primitive budget bound: static upper bound on emitted GPU primitives is strictly `< 80_000`.

use crate::ast::{BinOp, Expr, Module, Param, Type, UnaryOp};
use crate::diag::{InfimumDiagnostic, Span};
use crate::forma::ast::{Align, FormaDef, FormaNode, FormaNodeKind, Justify};
use crate::verify::{find_z3_binary, normalize_smt_value, parse_sexp_pairs, run_z3};
use std::collections::BTreeMap;

/// Maximum GPU primitives per frame in `infimum-web` linear memory stream.
pub const FORMA_MAX_PRIMS: usize = 80_000;

/// Summary of a verified `forma` block.
#[derive(Debug, Clone)]
pub struct FormaVerifyReport {
    pub name: String,
    pub obligations_proved: usize,
    pub static_prim_bound: usize,
}

#[derive(Debug, Clone)]
struct RectSym {
    x: String,
    y: String,
    w: String,
    h: String,
}

#[derive(Debug, Clone)]
struct ClipSym {
    x0: String,
    y0: String,
    x1: String,
    y1: String,
}

#[derive(Debug, Clone)]
struct UiObligation {
    guard_smt: String,
    goal_smt: String,
    description: String,
    hint: String,
    span: Span,
    watch_vars: Vec<String>,
}

struct FormaSmtBuilder {
    param_env: BTreeMap<String, String>,
    param_smt_to_name: BTreeMap<String, String>,
    decls: Vec<String>,
    asserts: Vec<String>,
    obligations: Vec<UiObligation>,
    node_counter: usize,
    fresh_counter: usize,
    intrinsic_w_cache: BTreeMap<usize, String>,
    intrinsic_h_cache: BTreeMap<usize, String>,
}

impl FormaSmtBuilder {
    fn new(params: &[Param]) -> Self {
        let mut param_env = BTreeMap::new();
        let mut param_smt_to_name = BTreeMap::new();
        let mut decls = Vec::new();
        for p in params {
            let smt_name = format!("fp_{}", p.name);
            let sort = match &p.ty {
                Type::I64 | Type::Fixed => "Int",
                Type::Bool => "Bool",
                Type::Str => "String",
                Type::Unit => "Bool",
                Type::Secret(inner) => match inner.as_ref() {
                    Type::Bool => "Bool",
                    Type::Str => "String",
                    _ => "Int",
                },
                Type::Tuple(_) | Type::Slice(_) => "Int",
            };
            decls.push(format!("(declare-const {smt_name} {sort})"));
            param_env.insert(p.name.clone(), smt_name.clone());
            param_smt_to_name.insert(smt_name, p.name.clone());
        }
        Self {
            param_env,
            param_smt_to_name,
            decls,
            asserts: Vec::new(),
            obligations: Vec::new(),
            node_counter: 0,
            fresh_counter: 0,
            intrinsic_w_cache: BTreeMap::new(),
            intrinsic_h_cache: BTreeMap::new(),
        }
    }

    fn fresh_int(&mut self, prefix: &str) -> String {
        let s = format!("{prefix}_{}", self.fresh_counter);
        self.fresh_counter += 1;
        self.decls.push(format!("(declare-const {s} Int)"));
        s
    }

    fn fresh_bool(&mut self, prefix: &str) -> String {
        let s = format!("{prefix}_{}", self.fresh_counter);
        self.fresh_counter += 1;
        self.decls.push(format!("(declare-const {s} Bool)"));
        s
    }

    fn define_int(&mut self, name: &str, expr_smt: &str) -> String {
        self.decls.push(format!("(declare-const {name} Int)"));
        self.asserts.push(format!("(assert (= {name} {expr_smt}))"));
        name.to_string()
    }

    fn next_node_id(&mut self) -> usize {
        let id = self.node_counter;
        self.node_counter += 1;
        id
    }

    fn has_intrinsic_w(node: &FormaNode) -> bool {
        node.props.w.is_some()
            || node.props.w_fit
            || (matches!(node.kind, FormaNodeKind::Text | FormaNodeKind::Toggle) && node.props.grow.is_none())
    }

    fn has_intrinsic_h(node: &FormaNode) -> bool {
        node.props.h.is_some()
            || node.props.h_fit
            || (matches!(
                node.kind,
                FormaNodeKind::Text | FormaNodeKind::Toggle | FormaNodeKind::Slider | FormaNodeKind::TextInput
            ) && node.props.grow.is_none())
    }

    fn clamp_min_max_smt(
        &mut self,
        expr_smt: String,
        min_e: &Option<Expr>,
        max_e: &Option<Expr>,
    ) -> String {
        let mut cur = expr_smt;
        if let Some(mn) = min_e {
            let mn_s = self.encode_expr(mn);
            let tmp = self.fresh_int("cmn");
            self.asserts
                .push(format!("(assert (= {tmp} (ite (>= {cur} {mn_s}) {cur} {mn_s})))"));
            cur = tmp;
        }
        if let Some(mx) = max_e {
            let mx_s = self.encode_expr(mx);
            let tmp = self.fresh_int("cmx");
            self.asserts
                .push(format!("(assert (= {tmp} (ite (<= {cur} {mx_s}) {cur} {mx_s})))"));
            cur = tmp;
        }
        cur
    }

    fn encode_intrinsic_w(&mut self, node: &FormaNode) -> String {
        let key = node as *const FormaNode as usize;
        if let Some(cached) = self.intrinsic_w_cache.get(&key) {
            return cached.clone();
        }
        let raw = if let Some(w_expr) = &node.props.w {
            self.encode_expr(w_expr)
        } else if node.kind == FormaNodeKind::Toggle {
            "44".to_string()
        } else if node.kind == FormaNodeKind::Slider {
            "140".to_string()
        } else if node.kind == FormaNodeKind::TextInput {
            "180".to_string()
        } else if node.kind == FormaNodeKind::Text {
            if let Some(t_expr) = &node.props.text {
                let sz = self.encode_opt_int(&node.props.size, "16");
                let tw = self.fresh_int("txtw");
                match t_expr {
                    Expr::StrLit(s) => {
                        let chars = s.chars().count() as i64;
                        if chars == 0 {
                            self.asserts.push(format!("(assert (= {tw} 0))"));
                        } else {
                            self.asserts.push(format!("(assert (>= {tw} 1))"));
                            self.asserts
                                .push(format!("(assert (<= {tw} (* {chars} {sz})))"));
                        }
                    }
                    _ => {
                        self.asserts.push(format!("(assert (>= {tw} 0))"));
                    }
                }
                tw
            } else {
                "0".to_string()
            }
        } else {
            let flow_children: Vec<&FormaNode> = node
                .children
                .iter()
                .filter(|c| !c.kind.is_overlay())
                .collect();
            let pad_default = self.encode_opt_int(&node.props.pad, "0");
            let pad_x = self.encode_opt_int(&node.props.pad_x, &pad_default);
            let two_pad = format!("(* 2 {pad_x})");
            if flow_children.is_empty() {
                two_pad
            } else if node.kind == FormaNodeKind::Row {
                let mut terms = Vec::with_capacity(flow_children.len());
                for c in &flow_children {
                    terms.push(self.encode_intrinsic_w(c));
                }
                let gap = self.encode_opt_int(&node.props.gap, "0");
                let total_gap = if flow_children.len() > 1 {
                    format!("(* {} {gap})", flow_children.len() - 1)
                } else {
                    "0".to_string()
                };
                format!("(+ (+ {} {total_gap}) {two_pad})", smt_sum(&terms))
            } else {
                let mut acc: Option<String> = None;
                for c in &flow_children {
                    let cw = self.encode_intrinsic_w(c);
                    acc = Some(match acc {
                        None => cw,
                        Some(prev) => {
                            let m = self.fresh_int("mxw");
                            self.asserts.push(format!(
                                "(assert (= {m} (ite (>= {prev} {cw}) {prev} {cw})))"
                            ));
                            m
                        }
                    });
                }
                format!("(+ {} {two_pad})", acc.unwrap_or_else(|| "0".to_string()))
            }
        };
        let res = self.clamp_min_max_smt(raw, &node.props.min_w, &node.props.max_w);
        self.intrinsic_w_cache.insert(key, res.clone());
        res
    }

    fn encode_intrinsic_h(&mut self, node: &FormaNode) -> String {
        let key = node as *const FormaNode as usize;
        if let Some(cached) = self.intrinsic_h_cache.get(&key) {
            return cached.clone();
        }
        let raw = if let Some(h_expr) = &node.props.h {
            self.encode_expr(h_expr)
        } else if node.kind == FormaNodeKind::Toggle || node.kind == FormaNodeKind::Slider {
            "24".to_string()
        } else if node.kind == FormaNodeKind::TextInput {
            "34".to_string()
        } else if node.kind == FormaNodeKind::Text {
            self.encode_opt_int(&node.props.size, "16")
        } else {
            let flow_children: Vec<&FormaNode> = node
                .children
                .iter()
                .filter(|c| !c.kind.is_overlay())
                .collect();
            let pad_default = self.encode_opt_int(&node.props.pad, "0");
            let pad_y = self.encode_opt_int(&node.props.pad_y, &pad_default);
            let two_pad = format!("(* 2 {pad_y})");
            if flow_children.is_empty() {
                two_pad
            } else if node.kind == FormaNodeKind::Col {
                let mut terms = Vec::with_capacity(flow_children.len());
                for c in &flow_children {
                    terms.push(self.encode_intrinsic_h(c));
                }
                let gap = self.encode_opt_int(&node.props.gap, "0");
                let total_gap = if flow_children.len() > 1 {
                    format!("(* {} {gap})", flow_children.len() - 1)
                } else {
                    "0".to_string()
                };
                format!("(+ (+ {} {total_gap}) {two_pad})", smt_sum(&terms))
            } else {
                let mut acc: Option<String> = None;
                for c in &flow_children {
                    let ch = self.encode_intrinsic_h(c);
                    acc = Some(match acc {
                        None => ch,
                        Some(prev) => {
                            let m = self.fresh_int("mxh");
                            self.asserts.push(format!(
                                "(assert (= {m} (ite (>= {prev} {ch}) {prev} {ch})))"
                            ));
                            m
                        }
                    });
                }
                format!("(+ {} {two_pad})", acc.unwrap_or_else(|| "0".to_string()))
            }
        };
        let res = self.clamp_min_max_smt(raw, &node.props.min_h, &node.props.max_h);
        self.intrinsic_h_cache.insert(key, res.clone());
        res
    }

    fn encode_expr(&mut self, expr: &Expr) -> String {
        match expr {
            Expr::IntLit(v) | Expr::FixedLit(v) => {
                if *v < 0 {
                    format!("(- {})", v.unsigned_abs())
                } else {
                    v.to_string()
                }
            }
            Expr::BoolLit(b) => if *b { "true" } else { "false" }.to_string(),
            Expr::StrLit(_) | Expr::UnitLit => "true".to_string(),
            Expr::Var(name) => self
                .param_env
                .get(name)
                .cloned()
                .unwrap_or_else(|| self.fresh_int("ext")),
            Expr::SecretWrap(inner) | Expr::Reveal { expr: inner, .. } => self.encode_expr(inner),
            Expr::Unary { op, expr: inner } => {
                let sub = self.encode_expr(inner);
                match op {
                    UnaryOp::Neg => format!("(- {sub})"),
                    UnaryOp::Not => format!("(not {sub})"),
                    UnaryOp::IntToFixed => format!("(* {sub} 65536)"),
                    UnaryOp::FixedToInt => format!("(div {sub} 65536)"),
                    UnaryOp::FixedToRaw | UnaryOp::RawToFixed => sub,
                }
            }
            Expr::Binary { op, lhs, rhs } => {
                let l = self.encode_expr(lhs);
                let r = self.encode_expr(rhs);
                match op {
                    BinOp::Add => format!("(+ {l} {r})"),
                    BinOp::Sub => format!("(- {l} {r})"),
                    BinOp::Mul => format!("(* {l} {r})"),
                    BinOp::Div => format!("(div {l} {r})"),
                    BinOp::FixedMul => format!("(div (* {l} {r}) 65536)"),
                    BinOp::FixedDiv => format!("(div (* {l} 65536) {r})"),
                    BinOp::Mod => format!("(mod {l} {r})"),
                    BinOp::Eq => format!("(= {l} {r})"),
                    BinOp::Ne => format!("(distinct {l} {r})"),
                    BinOp::Lt => format!("(< {l} {r})"),
                    BinOp::Le => format!("(<= {l} {r})"),
                    BinOp::Gt => format!("(> {l} {r})"),
                    BinOp::Ge => format!("(>= {l} {r})"),
                    BinOp::And => format!("(and {l} {r})"),
                    BinOp::Or => format!("(or {l} {r})"),
                    BinOp::BitAnd
                    | BinOp::BitOr
                    | BinOp::BitXor
                    | BinOp::Shl
                    | BinOp::Shr => self.fresh_int("bitop"),
                }
            }
            Expr::If {
                cond,
                then_branch,
                else_branch,
            } => {
                let c = self.encode_expr(cond);
                let t = self.encode_expr(then_branch);
                let e = self.encode_expr(else_branch);
                format!("(ite {c} {t} {e})")
            }
            Expr::Call { .. } | Expr::Perform { .. } | Expr::MemLoad { .. } => {
                self.fresh_int("dyn")
            }
            Expr::MemStore { .. }
            | Expr::Block { .. }
            | Expr::TupleLit(_)
            | Expr::TupleField { .. }
            | Expr::SliceLit { .. }
            | Expr::SlicePtr(_)
            | Expr::SliceLen(_) => self.fresh_int("blk"),
            Expr::BoundsCheck { index, .. } => self.encode_expr(index),
            Expr::RangeCheck { lo, .. } => self.encode_expr(lo),
        }
    }

    fn encode_bool_expr(&mut self, expr: &Expr) -> String {
        match expr {
            Expr::BoolLit(b) => if *b { "true" } else { "false" }.to_string(),
            Expr::Var(name) => self
                .param_env
                .get(name)
                .cloned()
                .unwrap_or_else(|| self.fresh_bool("vis")),
            Expr::Unary {
                op: UnaryOp::Not,
                expr: inner,
            } => format!("(not {})", self.encode_bool_expr(inner)),
            Expr::Binary {
                op: BinOp::Eq
                    | BinOp::Ne
                    | BinOp::Lt
                    | BinOp::Le
                    | BinOp::Gt
                    | BinOp::Ge
                    | BinOp::And
                    | BinOp::Or,
                ..
            } => self.encode_expr(expr),
            _ => self.fresh_bool("vis"),
        }
    }

    fn encode_opt_int(&mut self, opt: &Option<Expr>, default_smt: &str) -> String {
        match opt {
            Some(e) => self.encode_expr(e),
            None => default_smt.to_string(),
        }
    }

    fn check_node(
        &mut self,
        node: &FormaNode,
        rect: RectSym,
        parent_clip: ClipSym,
        root_clip: &ClipSym,
        parent_guard: &str,
        in_scroll: bool,
    ) {
        let nid = self.next_node_id();
        let node_guard = match &node.props.visible {
            Some(vis) => {
                let v = self.encode_bool_expr(vis);
                conjoin(parent_guard, &v)
            }
            None => parent_guard.to_string(),
        };

        // Invariant 1: Non-degeneracy for visible interactive nodes.
        if node.kind.is_interactive() {
            self.obligations.push(UiObligation {
                guard_smt: node_guard.clone(),
                goal_smt: format!("(and (> {} 0) (> {} 0))", rect.w, rect.h),
                description: format!(
                    "Non-degeneracy violated for {}#{nid}: width or height can collapse to <= 0",
                    node.kind.as_str()
                ),
                hint: "Strengthen the forma 'req' preconditions, reduce padding/gaps, or decrease fixed sibling sizes so interactive controls always have positive width and height".to_string(),
                span: node.span,
                watch_vars: vec![rect.x.clone(), rect.y.clone(), rect.w.clone(), rect.h.clone()],
            });
        }

        // Widget-specific data invariants proven by Z3:
        if node.kind == FormaNodeKind::Slider {
            let mn_smt = self.encode_opt_int(&node.props.min, "0");
            let mx_smt = self.encode_opt_int(&node.props.max, "100");
            self.obligations.push(UiObligation {
                guard_smt: node_guard.clone(),
                goal_smt: format!("(< {mn_smt} {mx_smt})"),
                description: format!(
                    "Invalid Slider#{nid} range: 'min' must be strictly less than 'max'"
                ),
                hint: "Ensure Slider has min < max (or strengthen 'req' preconditions) to prevent division by zero".to_string(),
                span: node.span,
                watch_vars: vec![],
            });
            if let Some(st) = &node.props.step {
                let st_smt = self.encode_expr(st);
                self.obligations.push(UiObligation {
                    guard_smt: node_guard.clone(),
                    goal_smt: format!("(> {st_smt} 0)"),
                    description: format!(
                        "Invalid Slider#{nid} step: 'step' must be > 0"
                    ),
                    hint: "Pass a strictly positive 'step' (e.g., step: 1 or step: 5)".to_string(),
                    span: node.span,
                    watch_vars: vec![],
                });
            }
        }
        if node.kind == FormaNodeKind::TextInput {
            if let Some(ml) = &node.props.max_len {
                let ml_smt = self.encode_expr(ml);
                self.obligations.push(UiObligation {
                    guard_smt: node_guard.clone(),
                    goal_smt: format!("(> {ml_smt} 0)"),
                    description: format!(
                        "Invalid TextInput#{nid} capacity: 'max_len' must be > 0"
                    ),
                    hint: "Set max_len to a positive buffer capacity (e.g., max_len: 128)".to_string(),
                    span: node.span,
                    watch_vars: vec![],
                });
            }
        }

        // Optional explicit min_w / min_h constraints.
        if let Some(mw) = &node.props.min_w {
            let mw_smt = self.encode_expr(mw);
            self.obligations.push(UiObligation {
                guard_smt: node_guard.clone(),
                goal_smt: format!("(>= {} {mw_smt})", rect.w),
                description: format!(
                    "Minimum width constraint violated for {}#{nid}",
                    node.kind.as_str()
                ),
                hint: "Ensure container width and 'req' preconditions guarantee w >= min_w".to_string(),
                span: node.span,
                watch_vars: vec![rect.w.clone()],
            });
        }
        if let Some(mh) = &node.props.min_h {
            let mh_smt = self.encode_expr(mh);
            self.obligations.push(UiObligation {
                guard_smt: node_guard.clone(),
                goal_smt: format!("(>= {} {mh_smt})", rect.h),
                description: format!(
                    "Minimum height constraint violated for {}#{nid}",
                    node.kind.as_str()
                ),
                hint: "Ensure container height and 'req' preconditions guarantee h >= min_h".to_string(),
                span: node.span,
                watch_vars: vec![rect.h.clone()],
            });
        }

        // Invariant 3b: Clip rectangle containment and non-inversion.
        let active_clip = if node.props.clip.is_some() {
            let rx1 = format!("(+ {} {})", rect.x, rect.w);
            let ry1 = format!("(+ {} {})", rect.y, rect.h);
            let cx0 = self.define_int(
                &format!("n{nid}_cx0"),
                &smt_max(&parent_clip.x0, &rect.x),
            );
            let cy0 = self.define_int(
                &format!("n{nid}_cy0"),
                &smt_max(&parent_clip.y0, &rect.y),
            );
            let cx1 = self.define_int(
                &format!("n{nid}_cx1"),
                &smt_min(&parent_clip.x1, &rx1),
            );
            let cy1 = self.define_int(
                &format!("n{nid}_cy1"),
                &smt_min(&parent_clip.y1, &ry1),
            );
            self.obligations.push(UiObligation {
                guard_smt: node_guard.clone(),
                goal_smt: format!(
                    "(and (>= {cx0} {}) (>= {cy0} {}) (<= {cx1} {}) (<= {cy1} {}) (<= {cx0} {cx1}) (<= {cy0} {cy1}))",
                    parent_clip.x0, parent_clip.y0, parent_clip.x1, parent_clip.y1
                ),
                description: format!(
                    "Clip containment violated for {}#{nid}: clip rectangle is inverted or outside parent clip",
                    node.kind.as_str()
                ),
                hint: "Ensure clipped container stays within parent bounds with non-negative dimensions".to_string(),
                span: node.span,
                watch_vars: vec![cx0.clone(), cy0.clone(), cx1.clone(), cy1.clone()],
            });
            ClipSym {
                x0: cx0,
                y0: cy0,
                x1: cx1,
                y1: cy1,
            }
        } else {
            parent_clip
        };

        if node.children.is_empty() {
            return;
        }

        // Compute inner padded content box.
        let pad_default = self.encode_opt_int(&node.props.pad, "0");
        let pad_x = match &node.props.pad_x {
            Some(e) => self.encode_expr(e),
            None => pad_default.clone(),
        };
        let pad_y = match &node.props.pad_y {
            Some(e) => self.encode_expr(e),
            None => pad_default,
        };
        let ix = self.define_int(&format!("n{nid}_ix"), &format!("(+ {} {pad_x})", rect.x));
        let iy = self.define_int(&format!("n{nid}_iy"), &format!("(+ {} {pad_y})", rect.y));
        let iw = self.define_int(
            &format!("n{nid}_iw"),
            &format!("(- {} (* 2 {pad_x}))", rect.w),
        );
        let ih = self.define_int(
            &format!("n{nid}_ih"),
            &format!("(- {} (* 2 {pad_y}))", rect.h),
        );

        let flow_children: Vec<&FormaNode> = node
            .children
            .iter()
            .filter(|c| !c.kind.is_overlay())
            .collect();
        let overlay_children: Vec<&FormaNode> = node
            .children
            .iter()
            .filter(|c| c.kind.is_overlay())
            .collect();

        let child_in_scroll = in_scroll || node.kind.is_scroll();
        let align = node.props.align.unwrap_or(Align::Start);

        match node.kind {
            FormaNodeKind::Row => {
                self.layout_row(
                    nid,
                    node,
                    &flow_children,
                    &ix,
                    &iy,
                    &iw,
                    &ih,
                    align,
                    &active_clip,
                    root_clip,
                    &node_guard,
                    child_in_scroll,
                );
            }
            FormaNodeKind::Col => {
                self.layout_col(
                    nid,
                    node,
                    &flow_children,
                    &ix,
                    &iy,
                    &iw,
                    &ih,
                    align,
                    &active_clip,
                    root_clip,
                    &node_guard,
                    child_in_scroll,
                );
            }
            _ => {
                for (ci, child) in flow_children.iter().enumerate() {
                    let cw_e = if Self::has_intrinsic_w(child) {
                        let iw_c = self.encode_intrinsic_w(child);
                        if child.props.shrink.is_some() {
                            format!("(ite (<= {iw_c} {iw}) {iw_c} {iw})")
                        } else {
                            iw_c
                        }
                    } else {
                        self.clamp_min_max_smt(iw.clone(), &child.props.min_w, &child.props.max_w)
                    };
                    let cw = self.define_int(&format!("n{nid}_c{ci}_w"), &cw_e);
                    let ch_e = if Self::has_intrinsic_h(child) {
                        let ih_c = self.encode_intrinsic_h(child);
                        if child.props.shrink.is_some() {
                            format!("(ite (<= {ih_c} {ih}) {ih_c} {ih})")
                        } else {
                            ih_c
                        }
                    } else {
                        self.clamp_min_max_smt(ih.clone(), &child.props.min_h, &child.props.max_h)
                    };
                    let ch = self.define_int(&format!("n{nid}_c{ci}_h"), &ch_e);
                    let default_cx = align_pos_smt(&ix, &iw, &cw, align);
                    let default_cy = align_pos_smt(&iy, &ih, &ch, align);
                    let cx_e = self.encode_opt_int(&child.props.x, &default_cx);
                    let cx = self.define_int(&format!("n{nid}_c{ci}_x"), &cx_e);
                    let cy_e = self.encode_opt_int(&child.props.y, &default_cy);
                    let cy = self.define_int(&format!("n{nid}_c{ci}_y"), &cy_e);
                    let c_vis = match &child.props.visible {
                        Some(v) => conjoin(&node_guard, &self.encode_bool_expr(v)),
                        None => node_guard.clone(),
                    };
                    if !child_in_scroll && !node.kind.is_overlay() {
                        self.push_bounds_obligation(
                            &c_vis,
                            &cx,
                            &cy,
                            &cw,
                            &ch,
                            &ix,
                            &iy,
                            &iw,
                            &ih,
                            child.kind,
                            child.span,
                        );
                    }
                    self.check_node(
                        child,
                        RectSym {
                            x: cx,
                            y: cy,
                            w: cw,
                            h: ch,
                        },
                        active_clip.clone(),
                        root_clip,
                        &node_guard,
                        child_in_scroll,
                    );
                }
            }
        }

        // Overlay children escape parent flow and parent clipping.
        for (oi, oc) in overlay_children.iter().enumerate() {
            let ox_e = self.encode_opt_int(&oc.props.x, &rect.x);
            let ox = self.define_int(&format!("n{nid}_ov{oi}_x"), &ox_e);
            let oy_e = self.encode_opt_int(&oc.props.y, &rect.y);
            let oy = self.define_int(&format!("n{nid}_ov{oi}_y"), &oy_e);
            let ow_e = self.encode_opt_int(&oc.props.w, &rect.w);
            let ow = self.define_int(&format!("n{nid}_ov{oi}_w"), &ow_e);
            let oh_e = self.encode_opt_int(&oc.props.h, &rect.h);
            let oh = self.define_int(&format!("n{nid}_ov{oi}_h"), &oh_e);
            self.check_node(
                oc,
                RectSym {
                    x: ox,
                    y: oy,
                    w: ow,
                    h: oh,
                },
                root_clip.clone(),
                root_clip,
                &node_guard,
                false,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_row(
        &mut self,
        nid: usize,
        parent: &FormaNode,
        children: &[&FormaNode],
        ix: &str,
        iy: &str,
        iw: &str,
        ih: &str,
        align: Align,
        active_clip: &ClipSym,
        root_clip: &ClipSym,
        node_guard: &str,
        in_scroll: bool,
    ) {
        if children.is_empty() {
            return;
        }
        let gap = self.encode_opt_int(&parent.props.gap, "0");
        let n = children.len();
        let total_gap = if n > 1 {
            format!("(* {} {gap})", n - 1)
        } else {
            "0".to_string()
        };

        // Pass 1: measure fixed/intrinsic widths, shrink weights, and flex weights.
        let mut base_sized: Vec<Option<String>> = Vec::with_capacity(n);
        let mut shrink_weights: Vec<Option<String>> = Vec::with_capacity(n);
        let mut fixed_terms = Vec::new();
        let mut shrink_terms = Vec::new();
        let mut grow_weights: Vec<Option<String>> = Vec::with_capacity(n);
        let mut grow_terms = Vec::new();
        let mut uses_forma2_sizing = false;

        for (ci, child) in children.iter().enumerate() {
            if Self::has_intrinsic_w(child) {
                if child.props.w_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.shrink.is_some()
                    || child.props.max_w.is_some()
                {
                    uses_forma2_sizing = true;
                }
                let bw_e = if child.props.w_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.min_w.is_some()
                    || child.props.max_w.is_some()
                {
                    self.encode_intrinsic_w(child)
                } else {
                    child.props.w.as_ref().map(|e| self.encode_expr(e)).unwrap_or_else(|| self.encode_intrinsic_w(child))
                };
                let bw = self.define_int(&format!("n{nid}_rbw{ci}"), &bw_e);
                fixed_terms.push(bw.clone());
                base_sized.push(Some(bw));
                grow_weights.push(None);
                if let Some(sh) = &child.props.shrink {
                    let s = self.encode_expr(sh);
                    shrink_terms.push(s.clone());
                    shrink_weights.push(Some(s));
                } else {
                    shrink_weights.push(None);
                }
            } else {
                let g = self.encode_opt_int(&child.props.grow, "1");
                grow_terms.push(g.clone());
                grow_weights.push(Some(g));
                base_sized.push(None);
                shrink_weights.push(None);
            }
        }

        let mut final_sized = base_sized.clone();
        if !shrink_terms.is_empty() {
            let raw_fixed = smt_sum(&fixed_terms);
            let over_raw = format!("(- (+ {raw_fixed} {total_gap}) {iw})");
            let over = self.define_int(
                &format!("n{nid}_ovw"),
                &format!("(ite (>= {over_raw} 0) {over_raw} 0)"),
            );
            let ts = self.define_int(&format!("n{nid}_tsw"), &smt_sum(&shrink_terms));
            fixed_terms.clear();
            for (ci, child) in children.iter().enumerate() {
                if let Some(bv) = &base_sized[ci] {
                    if let Some(sh) = &shrink_weights[ci] {
                        let red = format!("(ite (> {ts} 0) (div (* {over} {sh}) {ts}) 0)");
                        let mn = self.encode_opt_int(&child.props.min_w, "0");
                        let sub = format!("(- {bv} {red})");
                        let sv = self.define_int(
                            &format!("n{nid}_rsw{ci}"),
                            &format!("(ite (>= {sub} {mn}) {sub} {mn})"),
                        );
                        fixed_terms.push(sv.clone());
                        final_sized[ci] = Some(sv);
                    } else {
                        fixed_terms.push(bv.clone());
                    }
                }
            }
        }

        let fixed_w = smt_sum(&fixed_terms);
        let total_grow = smt_sum(&grow_terms);
        let flex_count = grow_terms.len();
        let rem_raw = format!("(- (- {iw} {fixed_w}) {total_gap})");
        let rem_e = if uses_forma2_sizing {
            format!("(ite (>= {rem_raw} 0) {rem_raw} 0)")
        } else {
            rem_raw
        };
        let rem_w = self.define_int(&format!("n{nid}_rem_w"), &rem_e);

        // Main-axis justify when flex_count == 0.
        let justify = parent.props.justify.unwrap_or(Justify::Start);
        let (mut cur_x, step_gap) = if flex_count == 0 && justify != Justify::Start {
            let free_raw = format!("(- (- {iw} {fixed_w}) {total_gap})");
            let free_w = self.define_int(
                &format!("n{nid}_frw"),
                &format!("(ite (>= {free_raw} 0) {free_raw} 0)"),
            );
            match justify {
                Justify::Start => (ix.to_string(), gap.clone()),
                Justify::Center => (format!("(+ {ix} (div {free_w} 2))"), gap.clone()),
                Justify::End => (format!("(+ {ix} {free_w})"), gap.clone()),
                Justify::SpaceBetween => {
                    if n > 1 {
                        let sg = self.define_int(
                            &format!("n{nid}_rsg"),
                            &format!("(+ {gap} (div {free_w} {}))", n - 1),
                        );
                        (ix.to_string(), sg)
                    } else {
                        (ix.to_string(), gap.clone())
                    }
                }
                Justify::SpaceAround => {
                    let extra = self.define_int(
                        &format!("n{nid}_rsa"),
                        &format!("(div {free_w} {n})"),
                    );
                    let sg = self.define_int(
                        &format!("n{nid}_rsg"),
                        &format!("(+ {gap} {extra})"),
                    );
                    (format!("(+ {ix} (div {extra} 2))"), sg)
                }
            }
        } else {
            (ix.to_string(), gap.clone())
        };

        // Pass 2: arrange left-to-right.
        let mut alloc_terms = Vec::new();
        let mut flex_seen = 0usize;
        let mut prev_rect_and_guard: Option<(String, String, String)> = None;

        for (ci, child) in children.iter().enumerate() {
            let cw_expr = if let Some(sv) = &final_sized[ci] {
                sv.clone()
            } else {
                let g = grow_weights[ci].as_ref().unwrap();
                flex_seen += 1;
                if flex_seen == flex_count {
                    let alloc_sum = smt_sum(&alloc_terms);
                    format!("(- {rem_w} {alloc_sum})")
                } else {
                    let share = format!("(div (* {rem_w} {g}) {total_grow})");
                    alloc_terms.push(share.clone());
                    share
                }
            };
            let cw = self.define_int(&format!("n{nid}_r{ci}_w"), &cw_expr);
            let ch_e = if Self::has_intrinsic_h(child) {
                self.encode_intrinsic_h(child)
            } else {
                self.clamp_min_max_smt(ih.to_string(), &child.props.min_h, &child.props.max_h)
            };
            let ch = self.define_int(&format!("n{nid}_r{ci}_h"), &ch_e);
            let cx = self.define_int(&format!("n{nid}_r{ci}_x"), &cur_x);
            let cy = self.define_int(
                &format!("n{nid}_r{ci}_y"),
                &align_pos_smt(iy, ih, &ch, align),
            );

            let c_vis = match &child.props.visible {
                Some(v) => conjoin(node_guard, &self.encode_bool_expr(v)),
                None => node_guard.to_string(),
            };

            // Invariant 2: No sibling overlap in Row.
            if let Some((prev_x, prev_w, prev_vis)) = &prev_rect_and_guard {
                let pair_guard = conjoin(prev_vis, &c_vis);
                self.obligations.push(UiObligation {
                    guard_smt: pair_guard,
                    goal_smt: format!("(<= (+ {prev_x} {prev_w}) {cx})"),
                    description: format!(
                        "Horizontal sibling overlap in Row#{nid} between children {} and {ci}",
                        ci - 1
                    ),
                    hint: "Ensure gap >= 0 and sibling widths fit without overlapping".to_string(),
                    span: child.span,
                    watch_vars: vec![prev_x.clone(), prev_w.clone(), cx.clone()],
                });
            }

            // Invariant 3a: Bounds containment within Row inner box.
            if !in_scroll {
                self.push_bounds_obligation(
                    &c_vis,
                    &cx,
                    &cy,
                    &cw,
                    &ch,
                    ix,
                    iy,
                    iw,
                    ih,
                    child.kind,
                    child.span,
                );
            }

            prev_rect_and_guard = Some((cx.clone(), cw.clone(), c_vis));
            cur_x = format!("(+ (+ {cx} {cw}) {step_gap})");

            self.check_node(
                child,
                RectSym {
                    x: cx,
                    y: cy,
                    w: cw,
                    h: ch,
                },
                active_clip.clone(),
                root_clip,
                node_guard,
                in_scroll,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn layout_col(
        &mut self,
        nid: usize,
        parent: &FormaNode,
        children: &[&FormaNode],
        ix: &str,
        iy: &str,
        iw: &str,
        ih: &str,
        align: Align,
        active_clip: &ClipSym,
        root_clip: &ClipSym,
        node_guard: &str,
        in_scroll: bool,
    ) {
        if children.is_empty() {
            return;
        }
        let gap = self.encode_opt_int(&parent.props.gap, "0");
        let n = children.len();
        let total_gap = if n > 1 {
            format!("(* {} {gap})", n - 1)
        } else {
            "0".to_string()
        };

        // Pass 1: measure fixed/intrinsic heights, shrink weights, and flex weights.
        let mut base_sized: Vec<Option<String>> = Vec::with_capacity(n);
        let mut shrink_weights: Vec<Option<String>> = Vec::with_capacity(n);
        let mut fixed_terms = Vec::new();
        let mut shrink_terms = Vec::new();
        let mut grow_weights: Vec<Option<String>> = Vec::with_capacity(n);
        let mut grow_terms = Vec::new();
        let mut uses_forma2_sizing = false;

        for (ci, child) in children.iter().enumerate() {
            if Self::has_intrinsic_h(child) {
                if child.props.h_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.shrink.is_some()
                    || child.props.max_h.is_some()
                {
                    uses_forma2_sizing = true;
                }
                let bh_e = if child.props.h_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.min_h.is_some()
                    || child.props.max_h.is_some()
                {
                    self.encode_intrinsic_h(child)
                } else {
                    child.props.h.as_ref().map(|e| self.encode_expr(e)).unwrap_or_else(|| self.encode_intrinsic_h(child))
                };
                let bh = self.define_int(&format!("n{nid}_kbh{ci}"), &bh_e);
                fixed_terms.push(bh.clone());
                base_sized.push(Some(bh));
                grow_weights.push(None);
                if let Some(sh) = &child.props.shrink {
                    let s = self.encode_expr(sh);
                    shrink_terms.push(s.clone());
                    shrink_weights.push(Some(s));
                } else {
                    shrink_weights.push(None);
                }
            } else {
                let g = self.encode_opt_int(&child.props.grow, "1");
                grow_terms.push(g.clone());
                grow_weights.push(Some(g));
                base_sized.push(None);
                shrink_weights.push(None);
            }
        }

        let mut final_sized = base_sized.clone();
        if !shrink_terms.is_empty() {
            let raw_fixed = smt_sum(&fixed_terms);
            let over_raw = format!("(- (+ {raw_fixed} {total_gap}) {ih})");
            let over = self.define_int(
                &format!("n{nid}_ovh"),
                &format!("(ite (>= {over_raw} 0) {over_raw} 0)"),
            );
            let ts = self.define_int(&format!("n{nid}_tsh"), &smt_sum(&shrink_terms));
            fixed_terms.clear();
            for (ci, child) in children.iter().enumerate() {
                if let Some(bv) = &base_sized[ci] {
                    if let Some(sh) = &shrink_weights[ci] {
                        let red = format!("(ite (> {ts} 0) (div (* {over} {sh}) {ts}) 0)");
                        let mn = self.encode_opt_int(&child.props.min_h, "0");
                        let sub = format!("(- {bv} {red})");
                        let sv = self.define_int(
                            &format!("n{nid}_ksh{ci}"),
                            &format!("(ite (>= {sub} {mn}) {sub} {mn})"),
                        );
                        fixed_terms.push(sv.clone());
                        final_sized[ci] = Some(sv);
                    } else {
                        fixed_terms.push(bv.clone());
                    }
                }
            }
        }

        let fixed_h = smt_sum(&fixed_terms);
        let total_grow = smt_sum(&grow_terms);
        let flex_count = grow_terms.len();
        let rem_raw = format!("(- (- {ih} {fixed_h}) {total_gap})");
        let rem_e = if uses_forma2_sizing {
            format!("(ite (>= {rem_raw} 0) {rem_raw} 0)")
        } else {
            rem_raw
        };
        let rem_h = self.define_int(&format!("n{nid}_rem_h"), &rem_e);

        // Main-axis justify when flex_count == 0.
        let justify = parent.props.justify.unwrap_or(Justify::Start);
        let (mut cur_y, step_gap) = if flex_count == 0 && justify != Justify::Start {
            let free_raw = format!("(- (- {ih} {fixed_h}) {total_gap})");
            let free_h = self.define_int(
                &format!("n{nid}_frh"),
                &format!("(ite (>= {free_raw} 0) {free_raw} 0)"),
            );
            match justify {
                Justify::Start => (iy.to_string(), gap.clone()),
                Justify::Center => (format!("(+ {iy} (div {free_h} 2))"), gap.clone()),
                Justify::End => (format!("(+ {iy} {free_h})"), gap.clone()),
                Justify::SpaceBetween => {
                    if n > 1 {
                        let sg = self.define_int(
                            &format!("n{nid}_ksg"),
                            &format!("(+ {gap} (div {free_h} {}))", n - 1),
                        );
                        (iy.to_string(), sg)
                    } else {
                        (iy.to_string(), gap.clone())
                    }
                }
                Justify::SpaceAround => {
                    let extra = self.define_int(
                        &format!("n{nid}_ksa"),
                        &format!("(div {free_h} {n})"),
                    );
                    let sg = self.define_int(
                        &format!("n{nid}_ksg"),
                        &format!("(+ {gap} {extra})"),
                    );
                    (format!("(+ {iy} (div {extra} 2))"), sg)
                }
            }
        } else {
            (iy.to_string(), gap.clone())
        };

        // Pass 2: arrange top-to-bottom.
        let mut alloc_terms = Vec::new();
        let mut flex_seen = 0usize;
        let mut prev_rect_and_guard: Option<(String, String, String)> = None;

        for (ci, child) in children.iter().enumerate() {
            let ch_expr = if let Some(sv) = &final_sized[ci] {
                sv.clone()
            } else {
                let g = grow_weights[ci].as_ref().unwrap();
                flex_seen += 1;
                if flex_seen == flex_count {
                    let alloc_sum = smt_sum(&alloc_terms);
                    format!("(- {rem_h} {alloc_sum})")
                } else {
                    let share = format!("(div (* {rem_h} {g}) {total_grow})");
                    alloc_terms.push(share.clone());
                    share
                }
            };
            let ch = self.define_int(&format!("n{nid}_k{ci}_h"), &ch_expr);
            let cw_e = if Self::has_intrinsic_w(child) {
                let iw_c = self.encode_intrinsic_w(child);
                if child.props.shrink.is_some() {
                    format!("(ite (<= {iw_c} {iw}) {iw_c} {iw})")
                } else {
                    iw_c
                }
            } else {
                self.clamp_min_max_smt(iw.to_string(), &child.props.min_w, &child.props.max_w)
            };
            let cw = self.define_int(&format!("n{nid}_k{ci}_w"), &cw_e);
            let cy = self.define_int(&format!("n{nid}_k{ci}_y"), &cur_y);
            let cx = self.define_int(
                &format!("n{nid}_k{ci}_x"),
                &align_pos_smt(ix, iw, &cw, align),
            );

            let c_vis = match &child.props.visible {
                Some(v) => conjoin(node_guard, &self.encode_bool_expr(v)),
                None => node_guard.to_string(),
            };

            // Invariant 2: No sibling overlap in Col.
            if let Some((prev_y, prev_h, prev_vis)) = &prev_rect_and_guard {
                let pair_guard = conjoin(prev_vis, &c_vis);
                self.obligations.push(UiObligation {
                    guard_smt: pair_guard,
                    goal_smt: format!("(<= (+ {prev_y} {prev_h}) {cy})"),
                    description: format!(
                        "Vertical sibling overlap in Col#{nid} between children {} and {ci}",
                        ci - 1
                    ),
                    hint: "Ensure gap >= 0 and sibling heights fit without overlapping".to_string(),
                    span: child.span,
                    watch_vars: vec![prev_y.clone(), prev_h.clone(), cy.clone()],
                });
            }

            // Invariant 3a: Bounds containment within Col inner box.
            if !in_scroll {
                self.push_bounds_obligation(
                    &c_vis,
                    &cx,
                    &cy,
                    &cw,
                    &ch,
                    ix,
                    iy,
                    iw,
                    ih,
                    child.kind,
                    child.span,
                );
            }

            prev_rect_and_guard = Some((cy.clone(), ch.clone(), c_vis));
            cur_y = format!("(+ (+ {cy} {ch}) {step_gap})");

            self.check_node(
                child,
                RectSym {
                    x: cx,
                    y: cy,
                    w: cw,
                    h: ch,
                },
                active_clip.clone(),
                root_clip,
                node_guard,
                in_scroll,
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn push_bounds_obligation(
        &mut self,
        guard: &str,
        cx: &str,
        cy: &str,
        cw: &str,
        ch: &str,
        ix: &str,
        iy: &str,
        iw: &str,
        ih: &str,
        kind: FormaNodeKind,
        span: Span,
    ) {
        self.obligations.push(UiObligation {
            guard_smt: guard.to_string(),
            goal_smt: format!(
                "(and (>= {cx} {ix}) (>= {cy} {iy}) (<= (+ {cx} {cw}) (+ {ix} {iw})) (<= (+ {cy} {ch}) (+ {iy} {ih})))"
            ),
            description: format!(
                "Bounds overflow for {} child: exceeds parent container inner bounds",
                kind.as_str()
            ),
            hint: "Increase parent container size or 'req' bounds, reduce child fixed sizes/gaps/padding, or wrap overflowing content in Scroll".to_string(),
            span,
            watch_vars: vec![
                cx.to_string(),
                cy.to_string(),
                cw.to_string(),
                ch.to_string(),
                iw.to_string(),
                ih.to_string(),
            ],
        });
    }
}

/// Verify all `forma` blocks in a parsed `Module` using Z3.
pub fn verify_module_formas(module: &Module) -> Result<Vec<FormaVerifyReport>, InfimumDiagnostic> {
    let mut reports = Vec::with_capacity(module.formas.len());
    for forma in &module.formas {
        reports.push(verify_forma(forma)?);
    }
    Ok(reports)
}

/// Formally verify a single `FormaDef` via Z3.
pub fn verify_forma(forma: &FormaDef) -> Result<FormaVerifyReport, InfimumDiagnostic> {
    // Invariant 4: Static primitive budget bound (< 80_000).
    let static_prim_bound = forma.root.static_prim_upper_bound();
    if static_prim_bound >= FORMA_MAX_PRIMS {
        let mut cex = BTreeMap::new();
        cex.insert("static_prim_bound".to_string(), static_prim_bound.to_string());
        cex.insert("max_prims".to_string(), FORMA_MAX_PRIMS.to_string());
        return Err(InfimumDiagnostic::new(
            "E090_FORMA_LAYOUT_VIOLATION",
            "forma-verify",
            format!(
                "Forma '{}' exceeds 1-Draw-Call primitive budget ({static_prim_bound} >= {FORMA_MAX_PRIMS})",
                forma.name
            ),
            forma.span,
            "Reduce the number of static nodes in this forma tree below 80,000 primitives",
        )
        .with_counterexample(cex));
    }

    let z3_bin = find_z3_binary()?;
    let mut builder = FormaSmtBuilder::new(&forma.params);

    // Assert all `req` preconditions.
    for req in &forma.requires {
        let r_smt = builder.encode_bool_expr(req);
        builder.asserts.push(format!("(assert {r_smt})"));
    }

    // Root rectangle and root clip rectangle.
    let rx_e = builder.encode_opt_int(&forma.root.props.x, "0");
    let ry_e = builder.encode_opt_int(&forma.root.props.y, "0");
    let rw_e = if forma.root.props.w_fit {
        builder.encode_intrinsic_w(&forma.root)
    } else {
        builder.encode_opt_int(&forma.root.props.w, "0")
    };
    let rh_e = if forma.root.props.h_fit {
        builder.encode_intrinsic_h(&forma.root)
    } else {
        builder.encode_opt_int(&forma.root.props.h, "0")
    };
    let rx = builder.define_int("root_x", &rx_e);
    let ry = builder.define_int("root_y", &ry_e);
    let rw = builder.define_int("root_w", &rw_e);
    let rh = builder.define_int("root_h", &rh_e);
    let rcx1 = builder.define_int("root_cx1", &format!("(+ {rx} {rw})"));
    let rcy1 = builder.define_int("root_cy1", &format!("(+ {ry} {rh})"));
    let root_clip = ClipSym {
        x0: rx.clone(),
        y0: ry.clone(),
        x1: rcx1,
        y1: rcy1,
    };

    builder.check_node(
        &forma.root,
        RectSym {
            x: rx,
            y: ry,
            w: rw,
            h: rh,
        },
        root_clip.clone(),
        &root_clip,
        "true",
        false,
    );

    // Check every obligation via Z3 (negating the goal; `unsat` means proved).
    let mut base_script = String::from("(set-logic ALL)\n(set-option :produce-models true)\n");
    for d in &builder.decls {
        base_script.push_str(d);
        base_script.push('\n');
    }
    for a in &builder.asserts {
        base_script.push_str(a);
        base_script.push('\n');
    }

    for ob in &builder.obligations {
        let mut script = base_script.clone();
        if ob.guard_smt != "true" {
            script.push_str(&format!("(assert {})\n", ob.guard_smt));
        }
        script.push_str(&format!("(assert (not {}))\n", ob.goal_smt));
        script.push_str("(check-sat)\n");

        let mut query_vars: Vec<String> = builder.param_smt_to_name.keys().cloned().collect();
        for wv in &ob.watch_vars {
            if !query_vars.contains(wv) {
                query_vars.push(wv.clone());
            }
        }
        if !query_vars.is_empty() {
            script.push_str(&format!("(get-value ({}))\n", query_vars.join(" ")));
        }

        let z3_out = run_z3(&z3_bin, &script)?;
        let first_line = z3_out.lines().next().unwrap_or("").trim();
        match first_line {
            "unsat" => {}
            "sat" => {
                let mut cex = BTreeMap::new();
                let rest: String = z3_out.lines().skip(1).collect::<Vec<_>>().join(" ");
                for (k, v) in parse_sexp_pairs(&rest) {
                    let clean_v = normalize_smt_value(&v);
                    if let Some(param_name) = builder.param_smt_to_name.get(&k) {
                        cex.insert(param_name.clone(), clean_v);
                    } else {
                        cex.insert(k, clean_v);
                    }
                }
                return Err(InfimumDiagnostic::new(
                    "E090_FORMA_LAYOUT_VIOLATION",
                    "forma-verify",
                    format!("Forma '{}': {}", forma.name, ob.description),
                    ob.span,
                    ob.hint.clone(),
                )
                .with_counterexample(cex));
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E090_FORMA_LAYOUT_VIOLATION",
                    "forma-verify",
                    format!(
                        "Forma '{}': Z3 returned inconclusive result '{other}' while checking: {}",
                        forma.name, ob.description
                    ),
                    ob.span,
                    "Simplify nonlinear layout expressions or add explicit 'req' bounds",
                ));
            }
        }
    }

    Ok(FormaVerifyReport {
        name: forma.name.clone(),
        obligations_proved: builder.obligations.len(),
        static_prim_bound,
    })
}

fn conjoin(a: &str, b: &str) -> String {
    if a == "true" {
        b.to_string()
    } else if b == "true" {
        a.to_string()
    } else {
        format!("(and {a} {b})")
    }
}

fn smt_max(a: &str, b: &str) -> String {
    format!("(ite (>= {a} {b}) {a} {b})")
}

fn smt_min(a: &str, b: &str) -> String {
    format!("(ite (<= {a} {b}) {a} {b})")
}

fn smt_sum(terms: &[String]) -> String {
    match terms.len() {
        0 => "0".to_string(),
        1 => terms[0].clone(),
        _ => format!("(+ {})", terms.join(" ")),
    }
}

fn align_pos_smt(start: &str, avail: &str, size: &str, align: Align) -> String {
    match align {
        Align::Start | Align::Stretch => start.to_string(),
        Align::Center => format!("(+ {start} (div (- {avail} {size}) 2))"),
        Align::End => format!("(+ {start} (- {avail} {size}))"),
    }
}
