// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

//! Lowering pass from `Infimum.Forma` declarative UI trees into pure `Infimum.Flux` / `Infimum.Strata`
//! functions (`FnDef`) compatible with the `infimum-web` 1-Draw-Call GPU primitive protocol.

use crate::ast::{BinOp, EffectCap, Expr, FnDef, Param, Stmt, TargetRef, Type, UnaryOp};
use crate::forma::ast::{Align, FormaDef, FormaNode, FormaNodeKind, Justify, Overflow};
use std::collections::BTreeMap;

/// Linear memory addresses for the `Infimum.Forma` input/hover register block
/// (aligned with `std/sys_mem.ifm`: clip_rects `64..127`, forma `128..191`, font_metrics `256..1023`).
pub const FORMA_MEM_MX: i64 = 128;
pub const FORMA_MEM_MY: i64 = 129;
pub const FORMA_MEM_CLICKED: i64 = 130;
pub const FORMA_MEM_WHEEL: i64 = 131;
pub const FORMA_MEM_HOVERED_OUT: i64 = 132;
pub const FORMA_MEM_STR_SCRATCH: i64 = 17;
pub const FORMA_SYS_CLIP_RECTS: i64 = 64;
pub const FORMA_SYS_FONT_METRICS_FLAG: i64 = 256;
pub const FORMA_SYS_FONT_METRICS_TABLE: i64 = 257;

const HELPER_CLAMP: &str = "$forma_clamp";
const HELPER_SET_CLIP: &str = "$forma_set_clip";
const HELPER_PUSH_RECT: &str = "$forma_push_rect";
const HELPER_CP_ATLAS: &str = "codepoint_to_atlas";
const HELPER_GLYPH_PX: &str = "glyph_px";
const HELPER_MEASURE_TEXT: &str = "$forma_measure_text";
const HELPER_DRAW_TEXT: &str = "$forma_draw_text";
const HELPER_TOGGLE: &str = "__forma_toggle";
const HELPER_SLIDER: &str = "__forma_slider";
const HELPER_TEXT_INPUT: &str = "__forma_text_input";

#[derive(Debug, Clone)]
struct LoweredNodeInfo {
    id: usize,
    kind: FormaNodeKind,
    x_var: String,
    y_var: String,
    w_var: String,
    h_var: String,
    vis_var: String,
    clip_var: String,
    clip_prop: Option<Expr>,
    clip_x0: String,
    clip_y0: String,
    clip_x1: String,
    clip_y1: String,
    in_overlay: bool,
    overlay_group: Option<usize>,
    bg: Option<Expr>,
    hover_bg: Option<Expr>,
    border: Option<Expr>,
    border_col: Option<Expr>,
    radius: Option<Expr>,
    shadow: Option<Expr>,
    has_visual_rect: bool,
    node_id_expr: Option<Expr>,
    on_click_expr: Option<Expr>,
    render_fn: Option<String>,
    on_wheel_fn: Option<String>,
    text_expr: Option<Expr>,
    text_size: Option<Expr>,
    text_style: Option<Expr>,
    text_color: Option<Expr>,
    text_align: Align,
    text_overflow: Overflow,
    checked: Option<Expr>,
    bind: Option<Expr>,
    min: Option<Expr>,
    max: Option<Expr>,
    step: Option<Expr>,
    buf: Option<Expr>,
    max_len: Option<Expr>,
    placeholder: Option<Expr>,
    mask: Option<Expr>,
    on_change: Option<Expr>,
    on_submit: Option<Expr>,
    accent: Option<Expr>,
}

struct FormaLowerer {
    stmts: Vec<Stmt>,
    nodes: Vec<LoweredNodeInfo>,
    next_id: usize,
    next_overlay_group: usize,
    measure_seq: usize,
    intrinsic_w_cache: BTreeMap<usize, String>,
    intrinsic_h_cache: BTreeMap<usize, String>,
}

impl FormaLowerer {
    fn new() -> Self {
        Self {
            stmts: Vec::new(),
            nodes: Vec::new(),
            next_id: 0,
            next_overlay_group: 0,
            measure_seq: 0,
            intrinsic_w_cache: BTreeMap::new(),
            intrinsic_h_cache: BTreeMap::new(),
        }
    }

    fn emit_fresh_let(&mut self, prefix: &str, value: Expr) -> String {
        let id = self.measure_seq;
        self.measure_seq += 1;
        self.emit_let(&format!("{prefix}_{id}"), value)
    }

    fn clamp_min_max(&mut self, expr: Expr, min_e: &Option<Expr>, max_e: &Option<Expr>) -> Expr {
        let mut res = expr;
        if let Some(mn) = min_e {
            let v = self.emit_fresh_let("_fmn", res);
            res = max_expr(var(&v), mn.clone());
        }
        if let Some(mx) = max_e {
            let v = self.emit_fresh_let("_fmx", res);
            res = min_expr(var(&v), mx.clone());
        }
        res
    }

    fn has_intrinsic_w(node: &FormaNode) -> bool {
        node.props.w.is_some()
            || node.props.w_fit
            || (matches!(node.kind, FormaNodeKind::Text | FormaNodeKind::Toggle)
                && node.props.grow.is_none())
    }

    fn has_intrinsic_h(node: &FormaNode) -> bool {
        node.props.h.is_some()
            || node.props.h_fit
            || (matches!(
                node.kind,
                FormaNodeKind::Text
                    | FormaNodeKind::Toggle
                    | FormaNodeKind::Slider
                    | FormaNodeKind::TextInput
            ) && node.props.grow.is_none())
    }

    fn compute_intrinsic_w(&mut self, node: &FormaNode) -> Expr {
        let key = node as *const FormaNode as usize;
        if let Some(cached) = self.intrinsic_w_cache.get(&key) {
            return var(cached);
        }
        let raw = if let Some(w_expr) = &node.props.w {
            w_expr.clone()
        } else if node.kind == FormaNodeKind::Toggle {
            Expr::IntLit(44)
        } else if node.kind == FormaNodeKind::Slider {
            Expr::IntLit(140)
        } else if node.kind == FormaNodeKind::TextInput {
            Expr::IntLit(180)
        } else if node.kind == FormaNodeKind::Text {
            if let Some(t_expr) = &node.props.text {
                let sz = node.props.size.clone().unwrap_or(Expr::IntLit(16));
                let st = node.props.style.clone().unwrap_or(Expr::IntLit(0));
                call_named(HELPER_MEASURE_TEXT, vec![t_expr.clone(), sz, st])
            } else {
                Expr::IntLit(0)
            }
        } else {
            let flow_children: Vec<&FormaNode> = node
                .children
                .iter()
                .filter(|c| !c.kind.is_overlay())
                .collect();
            let pad_default = node.props.pad.clone().unwrap_or(Expr::IntLit(0));
            let pad_x = node.props.pad_x.clone().unwrap_or(pad_default);
            let two_pad = bin(BinOp::Mul, Expr::IntLit(2), pad_x);
            if flow_children.is_empty() {
                two_pad
            } else if node.kind == FormaNodeKind::Row {
                let mut terms = Vec::with_capacity(flow_children.len());
                for c in &flow_children {
                    let cw = self.compute_intrinsic_w(c);
                    let cv = self.emit_fresh_let("_fmw", cw);
                    terms.push(var(&cv));
                }
                let gap = node.props.gap.clone().unwrap_or(Expr::IntLit(0));
                let total_gap = if flow_children.len() > 1 {
                    bin(
                        BinOp::Mul,
                        Expr::IntLit((flow_children.len() - 1) as i64),
                        gap,
                    )
                } else {
                    Expr::IntLit(0)
                };
                bin(BinOp::Add, bin(BinOp::Add, sum_exprs(&terms), total_gap), two_pad)
            } else {
                let mut acc: Option<Expr> = None;
                for c in &flow_children {
                    let cw = self.compute_intrinsic_w(c);
                    let cv = self.emit_fresh_let("_fmw", cw);
                    acc = Some(match acc {
                        None => var(&cv),
                        Some(prev) => max_expr(prev, var(&cv)),
                    });
                }
                bin(BinOp::Add, acc.unwrap_or(Expr::IntLit(0)), two_pad)
            }
        };
        let clamped = self.clamp_min_max(raw, &node.props.min_w, &node.props.max_w);
        let mv = self.emit_fresh_let("_fiw_m", clamped);
        self.intrinsic_w_cache.insert(key, mv.clone());
        var(&mv)
    }

    fn compute_intrinsic_h(&mut self, node: &FormaNode) -> Expr {
        let key = node as *const FormaNode as usize;
        if let Some(cached) = self.intrinsic_h_cache.get(&key) {
            return var(cached);
        }
        let raw = if let Some(h_expr) = &node.props.h {
            h_expr.clone()
        } else if node.kind == FormaNodeKind::Toggle || node.kind == FormaNodeKind::Slider {
            Expr::IntLit(24)
        } else if node.kind == FormaNodeKind::TextInput {
            Expr::IntLit(34)
        } else if node.kind == FormaNodeKind::Text {
            node.props.size.clone().unwrap_or(Expr::IntLit(16))
        } else {
            let flow_children: Vec<&FormaNode> = node
                .children
                .iter()
                .filter(|c| !c.kind.is_overlay())
                .collect();
            let pad_default = node.props.pad.clone().unwrap_or(Expr::IntLit(0));
            let pad_y = node.props.pad_y.clone().unwrap_or(pad_default);
            let two_pad = bin(BinOp::Mul, Expr::IntLit(2), pad_y);
            if flow_children.is_empty() {
                two_pad
            } else if node.kind == FormaNodeKind::Col {
                let mut terms = Vec::with_capacity(flow_children.len());
                for c in &flow_children {
                    let ch = self.compute_intrinsic_h(c);
                    let cv = self.emit_fresh_let("_fmh", ch);
                    terms.push(var(&cv));
                }
                let gap = node.props.gap.clone().unwrap_or(Expr::IntLit(0));
                let total_gap = if flow_children.len() > 1 {
                    bin(
                        BinOp::Mul,
                        Expr::IntLit((flow_children.len() - 1) as i64),
                        gap,
                    )
                } else {
                    Expr::IntLit(0)
                };
                bin(BinOp::Add, bin(BinOp::Add, sum_exprs(&terms), total_gap), two_pad)
            } else {
                let mut acc: Option<Expr> = None;
                for c in &flow_children {
                    let ch = self.compute_intrinsic_h(c);
                    let cv = self.emit_fresh_let("_fmh", ch);
                    acc = Some(match acc {
                        None => var(&cv),
                        Some(prev) => max_expr(prev, var(&cv)),
                    });
                }
                bin(BinOp::Add, acc.unwrap_or(Expr::IntLit(0)), two_pad)
            }
        };
        let clamped = self.clamp_min_max(raw, &node.props.min_h, &node.props.max_h);
        let mv = self.emit_fresh_let("_fih_m", clamped);
        self.intrinsic_h_cache.insert(key, mv.clone());
        var(&mv)
    }

    fn alloc_id(&mut self) -> usize {
        let id = self.next_id;
        self.next_id += 1;
        id
    }

    fn emit_let(&mut self, name: &str, value: Expr) -> String {
        self.stmts.push(Stmt::Let {
            name: name.to_string(),
            mutable: false,
            ty: None,
            value,
        });
        name.to_string()
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_node_geometry(
        &mut self,
        node: &FormaNode,
        rx_expr: Expr,
        ry_expr: Expr,
        rw_expr: Expr,
        rh_expr: Expr,
        parent_vis: Expr,
        parent_clip_id: Expr,
        parent_clip_rect: (String, String, String, String),
        root_clip_rect: &(String, String, String, String),
        in_overlay: bool,
        overlay_group: Option<usize>,
    ) {
        let nid = self.alloc_id();
        let x_var = self.emit_let(&format!("_fx_{nid}"), rx_expr);
        let y_var = self.emit_let(&format!("_fy_{nid}"), ry_expr);
        let w_var = self.emit_let(&format!("_fw_{nid}"), rw_expr);
        let h_var = self.emit_let(&format!("_fh_{nid}"), rh_expr);

        let vis_expr = match &node.props.visible {
            Some(v) => bin(BinOp::And, parent_vis, v.clone()),
            None => parent_vis,
        };
        let vis_var = self.emit_let(&format!("_fv_{nid}"), vis_expr);

        let (clip_var, clip_x0, clip_y0, clip_x1, clip_y1) = if let Some(cid_expr) = &node.props.clip
        {
            let cv = self.emit_let(&format!("_fc_{nid}"), cid_expr.clone());
            let (pcx0, pcy0, pcx1, pcy1) = parent_clip_rect;
            let cx0 = self.emit_let(
                &format!("_fcx0_{nid}"),
                max_expr(var(&pcx0), var(&x_var)),
            );
            let cy0 = self.emit_let(
                &format!("_fcy0_{nid}"),
                max_expr(var(&pcy0), var(&y_var)),
            );
            let cx1 = self.emit_let(
                &format!("_fcx1_{nid}"),
                min_expr(var(&pcx1), bin(BinOp::Add, var(&x_var), var(&w_var))),
            );
            let cy1 = self.emit_let(
                &format!("_fcy1_{nid}"),
                min_expr(var(&pcy1), bin(BinOp::Add, var(&y_var), var(&h_var))),
            );
            (cv, cx0, cy0, cx1, cy1)
        } else {
            let cv = self.emit_let(&format!("_fc_{nid}"), parent_clip_id);
            (
                cv,
                parent_clip_rect.0,
                parent_clip_rect.1,
                parent_clip_rect.2,
                parent_clip_rect.3,
            )
        };

        self.nodes.push(LoweredNodeInfo {
            id: nid,
            kind: node.kind,
            x_var: x_var.clone(),
            y_var: y_var.clone(),
            w_var: w_var.clone(),
            h_var: h_var.clone(),
            vis_var: vis_var.clone(),
            clip_var: clip_var.clone(),
            clip_prop: node.props.clip.clone(),
            clip_x0: clip_x0.clone(),
            clip_y0: clip_y0.clone(),
            clip_x1: clip_x1.clone(),
            clip_y1: clip_y1.clone(),
            in_overlay,
            overlay_group,
            bg: node.props.bg.clone(),
            hover_bg: node.props.hover_bg.clone(),
            border: node.props.border.clone(),
            border_col: node.props.border_col.clone(),
            radius: node.props.radius.clone(),
            shadow: node.props.shadow.clone(),
            has_visual_rect: node.props.has_visual_rect(),
            node_id_expr: node.props.id.clone(),
            on_click_expr: node.props.on_click.clone(),
            render_fn: node.props.render.clone(),
            on_wheel_fn: node.props.on_wheel.clone(),
            text_expr: node.props.text.clone(),
            text_size: node.props.size.clone(),
            text_style: node.props.style.clone(),
            text_color: node.props.color.clone(),
            text_align: node.props.align.unwrap_or(Align::Start),
            text_overflow: node.props.overflow.unwrap_or(Overflow::Visible),
            checked: node.props.checked.clone(),
            bind: node.props.bind.clone(),
            min: node.props.min.clone(),
            max: node.props.max.clone(),
            step: node.props.step.clone(),
            buf: node.props.buf.clone(),
            max_len: node.props.max_len.clone(),
            placeholder: node.props.placeholder.clone(),
            mask: node.props.mask.clone(),
            on_change: node.props.on_change.clone(),
            on_submit: node.props.on_submit.clone(),
            accent: node.props.accent.clone(),
        });

        if node.children.is_empty() {
            return;
        }

        let pad_default = node.props.pad.clone().unwrap_or(Expr::IntLit(0));
        let pad_x_expr = node.props.pad_x.clone().unwrap_or_else(|| pad_default.clone());
        let pad_y_expr = node.props.pad_y.clone().unwrap_or(pad_default);

        let px_var = self.emit_let(&format!("_fpx_{nid}"), pad_x_expr);
        let py_var = self.emit_let(&format!("_fpy_{nid}"), pad_y_expr);
        let ix_var = self.emit_let(
            &format!("_fix_{nid}"),
            bin(BinOp::Add, var(&x_var), var(&px_var)),
        );
        let iy_var = self.emit_let(
            &format!("_fiy_{nid}"),
            bin(BinOp::Add, var(&y_var), var(&py_var)),
        );
        let iw_var = self.emit_let(
            &format!("_fiw_{nid}"),
            bin(
                BinOp::Sub,
                var(&w_var),
                bin(BinOp::Mul, Expr::IntLit(2), var(&px_var)),
            ),
        );
        let ih_var = self.emit_let(
            &format!("_fih_{nid}"),
            bin(
                BinOp::Sub,
                var(&h_var),
                bin(BinOp::Mul, Expr::IntLit(2), var(&py_var)),
            ),
        );

        let active_clip_rect = (clip_x0, clip_y0, clip_x1, clip_y1);
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

        let align = node.props.align.unwrap_or(Align::Start);

        match node.kind {
            FormaNodeKind::Row => {
                self.lower_row(
                    nid,
                    node,
                    &flow_children,
                    &ix_var,
                    &iy_var,
                    &iw_var,
                    &ih_var,
                    align,
                    &vis_var,
                    &clip_var,
                    &active_clip_rect,
                    root_clip_rect,
                    in_overlay,
                    overlay_group,
                );
            }
            FormaNodeKind::Col => {
                self.lower_col(
                    nid,
                    node,
                    &flow_children,
                    &ix_var,
                    &iy_var,
                    &iw_var,
                    &ih_var,
                    align,
                    &vis_var,
                    &clip_var,
                    &active_clip_rect,
                    root_clip_rect,
                    in_overlay,
                    overlay_group,
                );
            }
            _ => {
                for (ci, child) in flow_children.iter().enumerate() {
                    let cw_expr = if Self::has_intrinsic_w(child) {
                        let iw_c = self.compute_intrinsic_w(child);
                        if child.props.shrink.is_some() {
                            min_expr(iw_c, var(&iw_var))
                        } else {
                            iw_c
                        }
                    } else {
                        self.clamp_min_max(var(&iw_var), &child.props.min_w, &child.props.max_w)
                    };
                    let ch_expr = if Self::has_intrinsic_h(child) {
                        let ih_c = self.compute_intrinsic_h(child);
                        if child.props.shrink.is_some() {
                            min_expr(ih_c, var(&ih_var))
                        } else {
                            ih_c
                        }
                    } else {
                        self.clamp_min_max(var(&ih_var), &child.props.min_h, &child.props.max_h)
                    };
                    let cw_var = self.emit_let(&format!("_fcw_{nid}_{ci}"), cw_expr);
                    let ch_var = self.emit_let(&format!("_fch_{nid}_{ci}"), ch_expr);
                    let cx_expr = child
                        .props
                        .x
                        .clone()
                        .unwrap_or_else(|| align_pos_expr(var(&ix_var), var(&iw_var), var(&cw_var), align));
                    let cy_expr = child
                        .props
                        .y
                        .clone()
                        .unwrap_or_else(|| align_pos_expr(var(&iy_var), var(&ih_var), var(&ch_var), align));
                    self.lower_node_geometry(
                        child,
                        cx_expr,
                        cy_expr,
                        var(&cw_var),
                        var(&ch_var),
                        var(&vis_var),
                        var(&clip_var),
                        active_clip_rect.clone(),
                        root_clip_rect,
                        in_overlay,
                        overlay_group,
                    );
                }
            }
        }

        for oc in overlay_children {
            let group_id = self.next_overlay_group;
            self.next_overlay_group += 1;
            let ox = oc.props.x.clone().unwrap_or_else(|| var(&x_var));
            let oy = oc.props.y.clone().unwrap_or_else(|| var(&y_var));
            let ow = oc.props.w.clone().unwrap_or_else(|| var(&w_var));
            let oh = oc.props.h.clone().unwrap_or_else(|| var(&h_var));
            self.lower_node_geometry(
                oc,
                ox,
                oy,
                ow,
                oh,
                var(&vis_var),
                Expr::IntLit(0),
                root_clip_rect.clone(),
                root_clip_rect,
                true,
                Some(group_id),
            );
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_row(
        &mut self,
        nid: usize,
        parent: &FormaNode,
        children: &[&FormaNode],
        ix_var: &str,
        iy_var: &str,
        iw_var: &str,
        ih_var: &str,
        align: Align,
        vis_var: &str,
        clip_var: &str,
        active_clip_rect: &(String, String, String, String),
        root_clip_rect: &(String, String, String, String),
        in_overlay: bool,
        overlay_group: Option<usize>,
    ) {
        if children.is_empty() {
            return;
        }
        let gap_var = self.emit_let(
            &format!("_fgap_{nid}"),
            parent.props.gap.clone().unwrap_or(Expr::IntLit(0)),
        );
        let n = children.len();
        let total_gap = if n > 1 {
            bin(BinOp::Mul, Expr::IntLit((n - 1) as i64), var(&gap_var))
        } else {
            Expr::IntLit(0)
        };

        let mut base_sized_vars: Vec<Option<String>> = Vec::with_capacity(n);
        let mut shrink_weights: Vec<Option<Expr>> = Vec::with_capacity(n);
        let mut fixed_terms = Vec::new();
        let mut shrink_terms = Vec::new();
        let mut grow_weights: Vec<Option<Expr>> = Vec::with_capacity(n);
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
                let bw_expr = if child.props.w_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.min_w.is_some()
                    || child.props.max_w.is_some()
                {
                    self.compute_intrinsic_w(child)
                } else {
                    child.props.w.clone().unwrap_or_else(|| self.compute_intrinsic_w(child))
                };
                let bw_var = self.emit_let(&format!("_fbw_{nid}_{ci}"), bw_expr);
                fixed_terms.push(var(&bw_var));
                base_sized_vars.push(Some(bw_var));
                grow_weights.push(None);
                if let Some(sh) = &child.props.shrink {
                    shrink_terms.push(sh.clone());
                    shrink_weights.push(Some(sh.clone()));
                } else {
                    shrink_weights.push(None);
                }
            } else {
                let g = child.props.grow.clone().unwrap_or(Expr::IntLit(1));
                grow_terms.push(g.clone());
                grow_weights.push(Some(g));
                base_sized_vars.push(None);
                shrink_weights.push(None);
            }
        }

        // Optional shrink pass when total fixed/intrinsic width exceeds available width.
        let mut final_sized_vars = base_sized_vars.clone();
        if !shrink_terms.is_empty() {
            let raw_fixed = sum_exprs(&fixed_terms);
            let over_var = self.emit_let(
                &format!("_fovw_{nid}"),
                max_expr(
                    Expr::IntLit(0),
                    bin(BinOp::Sub, bin(BinOp::Add, raw_fixed, total_gap.clone()), var(iw_var)),
                ),
            );
            let ts_var = self.emit_let(&format!("_ftsw_{nid}"), sum_exprs(&shrink_terms));
            fixed_terms.clear();
            for (ci, child) in children.iter().enumerate() {
                if let Some(bv) = &base_sized_vars[ci] {
                    if let Some(sh) = &shrink_weights[ci] {
                        let red = Expr::If {
                            cond: Box::new(bin(BinOp::Gt, var(&ts_var), Expr::IntLit(0))),
                            then_branch: Box::new(bin(
                                BinOp::Div,
                                bin(BinOp::Mul, var(&over_var), sh.clone()),
                                var(&ts_var),
                            )),
                            else_branch: Box::new(Expr::IntLit(0)),
                        };
                        let mn = child.props.min_w.clone().unwrap_or(Expr::IntLit(0));
                        let sv = self.emit_let(
                            &format!("_fsw_{nid}_{ci}"),
                            max_expr(mn, bin(BinOp::Sub, var(bv), red)),
                        );
                        fixed_terms.push(var(&sv));
                        final_sized_vars[ci] = Some(sv);
                    } else {
                        fixed_terms.push(var(bv));
                    }
                }
            }
        }

        let fixed_w = sum_exprs(&fixed_terms);
        let flex_count = grow_terms.len();
        let total_grow_var = if flex_count > 0 {
            Some(self.emit_let(&format!("_ftg_{nid}"), sum_exprs(&grow_terms)))
        } else {
            None
        };
        let rem_w_var = if flex_count > 0 {
            let raw_rem = bin(BinOp::Sub, bin(BinOp::Sub, var(iw_var), fixed_w.clone()), total_gap.clone());
            let rem_expr = if uses_forma2_sizing {
                max_expr(Expr::IntLit(0), raw_rem)
            } else {
                raw_rem
            };
            Some(self.emit_let(&format!("_frem_{nid}"), rem_expr))
        } else {
            None
        };

        // Main-axis justify distribution when there are no flex-grow children.
        let justify = parent.props.justify.unwrap_or(Justify::Start);
        let (start_x_expr, step_gap_var) = if flex_count == 0 && justify != Justify::Start {
            let free_var = self.emit_let(
                &format!("_ffrw_{nid}"),
                max_expr(
                    Expr::IntLit(0),
                    bin(BinOp::Sub, bin(BinOp::Sub, var(iw_var), fixed_w), total_gap),
                ),
            );
            match justify {
                Justify::Start => (var(ix_var), gap_var.clone()),
                Justify::Center => (
                    bin(
                        BinOp::Add,
                        var(ix_var),
                        bin(BinOp::Div, var(&free_var), Expr::IntLit(2)),
                    ),
                    gap_var.clone(),
                ),
                Justify::End => (bin(BinOp::Add, var(ix_var), var(&free_var)), gap_var.clone()),
                Justify::SpaceBetween => {
                    if n > 1 {
                        let sg = self.emit_let(
                            &format!("_fsg_{nid}"),
                            bin(
                                BinOp::Add,
                                var(&gap_var),
                                bin(BinOp::Div, var(&free_var), Expr::IntLit((n - 1) as i64)),
                            ),
                        );
                        (var(ix_var), sg)
                    } else {
                        (var(ix_var), gap_var.clone())
                    }
                }
                Justify::SpaceAround => {
                    let extra = self.emit_let(
                        &format!("_fsa_{nid}"),
                        bin(BinOp::Div, var(&free_var), Expr::IntLit(n as i64)),
                    );
                    let sg = self.emit_let(
                        &format!("_fsg_{nid}"),
                        bin(BinOp::Add, var(&gap_var), var(&extra)),
                    );
                    (
                        bin(
                            BinOp::Add,
                            var(ix_var),
                            bin(BinOp::Div, var(&extra), Expr::IntLit(2)),
                        ),
                        sg,
                    )
                }
            }
        } else {
            (var(ix_var), gap_var.clone())
        };

        let mut cur_x_var = self.emit_let(&format!("_fcur_{nid}_0"), start_x_expr);
        let mut alloc_terms = Vec::new();
        let mut flex_seen = 0usize;

        for (ci, child) in children.iter().enumerate() {
            let cw_expr = if let Some(sv) = &final_sized_vars[ci] {
                var(sv)
            } else {
                let g = grow_weights[ci].clone().unwrap();
                let rem_v = rem_w_var.as_ref().unwrap();
                let tg_v = total_grow_var.as_ref().unwrap();
                flex_seen += 1;
                if flex_seen == flex_count {
                    bin(BinOp::Sub, var(rem_v), sum_exprs(&alloc_terms))
                } else {
                    let share_var = self.emit_let(
                        &format!("_fsh_{nid}_{ci}"),
                        Expr::If {
                            cond: Box::new(bin(BinOp::Gt, var(tg_v), Expr::IntLit(0))),
                            then_branch: Box::new(bin(
                                BinOp::Div,
                                bin(BinOp::Mul, var(rem_v), g),
                                var(tg_v),
                            )),
                            else_branch: Box::new(Expr::IntLit(0)),
                        },
                    );
                    alloc_terms.push(var(&share_var));
                    var(&share_var)
                }
            };
            let cw_var = self.emit_let(&format!("_frw_{nid}_{ci}"), cw_expr);
            let ch_expr = if Self::has_intrinsic_h(child) {
                self.compute_intrinsic_h(child)
            } else {
                self.clamp_min_max(var(ih_var), &child.props.min_h, &child.props.max_h)
            };
            let ch_var = self.emit_let(&format!("_frh_{nid}_{ci}"), ch_expr);
            let cy_expr = align_pos_expr(var(iy_var), var(ih_var), var(&ch_var), align);

            self.lower_node_geometry(
                child,
                var(&cur_x_var),
                cy_expr,
                var(&cw_var),
                var(&ch_var),
                var(vis_var),
                var(clip_var),
                active_clip_rect.clone(),
                root_clip_rect,
                in_overlay,
                overlay_group,
            );

            if ci + 1 < n {
                cur_x_var = self.emit_let(
                    &format!("_fcur_{nid}_{}", ci + 1),
                    bin(
                        BinOp::Add,
                        bin(BinOp::Add, var(&cur_x_var), var(&cw_var)),
                        var(&step_gap_var),
                    ),
                );
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn lower_col(
        &mut self,
        nid: usize,
        parent: &FormaNode,
        children: &[&FormaNode],
        ix_var: &str,
        iy_var: &str,
        iw_var: &str,
        ih_var: &str,
        align: Align,
        vis_var: &str,
        clip_var: &str,
        active_clip_rect: &(String, String, String, String),
        root_clip_rect: &(String, String, String, String),
        in_overlay: bool,
        overlay_group: Option<usize>,
    ) {
        if children.is_empty() {
            return;
        }
        let gap_var = self.emit_let(
            &format!("_fgap_{nid}"),
            parent.props.gap.clone().unwrap_or(Expr::IntLit(0)),
        );
        let n = children.len();
        let total_gap = if n > 1 {
            bin(BinOp::Mul, Expr::IntLit((n - 1) as i64), var(&gap_var))
        } else {
            Expr::IntLit(0)
        };

        let mut base_sized_vars: Vec<Option<String>> = Vec::with_capacity(n);
        let mut shrink_weights: Vec<Option<Expr>> = Vec::with_capacity(n);
        let mut fixed_terms = Vec::new();
        let mut shrink_terms = Vec::new();
        let mut grow_weights: Vec<Option<Expr>> = Vec::with_capacity(n);
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
                let bh_expr = if child.props.h_fit
                    || child.kind == FormaNodeKind::Text
                    || child.props.min_h.is_some()
                    || child.props.max_h.is_some()
                {
                    self.compute_intrinsic_h(child)
                } else {
                    child.props.h.clone().unwrap_or_else(|| self.compute_intrinsic_h(child))
                };
                let bh_var = self.emit_let(&format!("_fbh_{nid}_{ci}"), bh_expr);
                fixed_terms.push(var(&bh_var));
                base_sized_vars.push(Some(bh_var));
                grow_weights.push(None);
                if let Some(sh) = &child.props.shrink {
                    shrink_terms.push(sh.clone());
                    shrink_weights.push(Some(sh.clone()));
                } else {
                    shrink_weights.push(None);
                }
            } else {
                let g = child.props.grow.clone().unwrap_or(Expr::IntLit(1));
                grow_terms.push(g.clone());
                grow_weights.push(Some(g));
                base_sized_vars.push(None);
                shrink_weights.push(None);
            }
        }

        // Optional shrink pass when total fixed/intrinsic height exceeds available height.
        let mut final_sized_vars = base_sized_vars.clone();
        if !shrink_terms.is_empty() {
            let raw_fixed = sum_exprs(&fixed_terms);
            let over_var = self.emit_let(
                &format!("_fovh_{nid}"),
                max_expr(
                    Expr::IntLit(0),
                    bin(BinOp::Sub, bin(BinOp::Add, raw_fixed, total_gap.clone()), var(ih_var)),
                ),
            );
            let ts_var = self.emit_let(&format!("_ftsh_{nid}"), sum_exprs(&shrink_terms));
            fixed_terms.clear();
            for (ci, child) in children.iter().enumerate() {
                if let Some(bv) = &base_sized_vars[ci] {
                    if let Some(sh) = &shrink_weights[ci] {
                        let red = Expr::If {
                            cond: Box::new(bin(BinOp::Gt, var(&ts_var), Expr::IntLit(0))),
                            then_branch: Box::new(bin(
                                BinOp::Div,
                                bin(BinOp::Mul, var(&over_var), sh.clone()),
                                var(&ts_var),
                            )),
                            else_branch: Box::new(Expr::IntLit(0)),
                        };
                        let mn = child.props.min_h.clone().unwrap_or(Expr::IntLit(0));
                        let sv = self.emit_let(
                            &format!("_fshk_{nid}_{ci}"),
                            max_expr(mn, bin(BinOp::Sub, var(bv), red)),
                        );
                        fixed_terms.push(var(&sv));
                        final_sized_vars[ci] = Some(sv);
                    } else {
                        fixed_terms.push(var(bv));
                    }
                }
            }
        }

        let fixed_h = sum_exprs(&fixed_terms);
        let flex_count = grow_terms.len();
        let total_grow_var = if flex_count > 0 {
            Some(self.emit_let(&format!("_ftg_{nid}"), sum_exprs(&grow_terms)))
        } else {
            None
        };
        let rem_h_var = if flex_count > 0 {
            let raw_rem = bin(BinOp::Sub, bin(BinOp::Sub, var(ih_var), fixed_h.clone()), total_gap.clone());
            let rem_expr = if uses_forma2_sizing {
                max_expr(Expr::IntLit(0), raw_rem)
            } else {
                raw_rem
            };
            Some(self.emit_let(&format!("_frem_{nid}"), rem_expr))
        } else {
            None
        };

        // Main-axis justify distribution when there are no flex-grow children.
        let justify = parent.props.justify.unwrap_or(Justify::Start);
        let (start_y_expr, step_gap_var) = if flex_count == 0 && justify != Justify::Start {
            let free_var = self.emit_let(
                &format!("_ffrh_{nid}"),
                max_expr(
                    Expr::IntLit(0),
                    bin(BinOp::Sub, bin(BinOp::Sub, var(ih_var), fixed_h), total_gap),
                ),
            );
            match justify {
                Justify::Start => (var(iy_var), gap_var.clone()),
                Justify::Center => (
                    bin(
                        BinOp::Add,
                        var(iy_var),
                        bin(BinOp::Div, var(&free_var), Expr::IntLit(2)),
                    ),
                    gap_var.clone(),
                ),
                Justify::End => (bin(BinOp::Add, var(iy_var), var(&free_var)), gap_var.clone()),
                Justify::SpaceBetween => {
                    if n > 1 {
                        let sg = self.emit_let(
                            &format!("_fsg_{nid}"),
                            bin(
                                BinOp::Add,
                                var(&gap_var),
                                bin(BinOp::Div, var(&free_var), Expr::IntLit((n - 1) as i64)),
                            ),
                        );
                        (var(iy_var), sg)
                    } else {
                        (var(iy_var), gap_var.clone())
                    }
                }
                Justify::SpaceAround => {
                    let extra = self.emit_let(
                        &format!("_fsa_{nid}"),
                        bin(BinOp::Div, var(&free_var), Expr::IntLit(n as i64)),
                    );
                    let sg = self.emit_let(
                        &format!("_fsg_{nid}"),
                        bin(BinOp::Add, var(&gap_var), var(&extra)),
                    );
                    (
                        bin(
                            BinOp::Add,
                            var(iy_var),
                            bin(BinOp::Div, var(&extra), Expr::IntLit(2)),
                        ),
                        sg,
                    )
                }
            }
        } else {
            (var(iy_var), gap_var.clone())
        };

        let mut cur_y_var = self.emit_let(&format!("_fcur_{nid}_0"), start_y_expr);
        let mut alloc_terms = Vec::new();
        let mut flex_seen = 0usize;

        for (ci, child) in children.iter().enumerate() {
            let ch_expr = if let Some(sv) = &final_sized_vars[ci] {
                var(sv)
            } else {
                let g = grow_weights[ci].clone().unwrap();
                let rem_v = rem_h_var.as_ref().unwrap();
                let tg_v = total_grow_var.as_ref().unwrap();
                flex_seen += 1;
                if flex_seen == flex_count {
                    bin(BinOp::Sub, var(rem_v), sum_exprs(&alloc_terms))
                } else {
                    let share_var = self.emit_let(
                        &format!("_fsh_{nid}_{ci}"),
                        Expr::If {
                            cond: Box::new(bin(BinOp::Gt, var(tg_v), Expr::IntLit(0))),
                            then_branch: Box::new(bin(
                                BinOp::Div,
                                bin(BinOp::Mul, var(rem_v), g),
                                var(tg_v),
                            )),
                            else_branch: Box::new(Expr::IntLit(0)),
                        },
                    );
                    alloc_terms.push(var(&share_var));
                    var(&share_var)
                }
            };
            let ch_var = self.emit_let(&format!("_fkh_{nid}_{ci}"), ch_expr);
            let cw_expr = if Self::has_intrinsic_w(child) {
                let iw_c = self.compute_intrinsic_w(child);
                if child.props.shrink.is_some() {
                    min_expr(iw_c, var(iw_var))
                } else {
                    iw_c
                }
            } else {
                self.clamp_min_max(var(iw_var), &child.props.min_w, &child.props.max_w)
            };
            let cw_var = self.emit_let(&format!("_fkw_{nid}_{ci}"), cw_expr);
            let cx_expr = align_pos_expr(var(ix_var), var(iw_var), var(&cw_var), align);

            self.lower_node_geometry(
                child,
                cx_expr,
                var(&cur_y_var),
                var(&cw_var),
                var(&ch_var),
                var(vis_var),
                var(clip_var),
                active_clip_rect.clone(),
                root_clip_rect,
                in_overlay,
                overlay_group,
            );

            if ci + 1 < n {
                cur_y_var = self.emit_let(
                    &format!("_fcur_{nid}_{}", ci + 1),
                    bin(
                        BinOp::Add,
                        bin(BinOp::Add, var(&cur_y_var), var(&ch_var)),
                        var(&step_gap_var),
                    ),
                );
            }
        }
    }
}

/// Lower a `FormaDef` into a pure `Infimum.Flux` / `Infimum.Strata` `FnDef`.
pub fn lower_forma_def(forma: &FormaDef) -> FnDef {
    let mut lowerer = FormaLowerer::new();

    // Resolve mouse / click / wheel inputs from explicit params or Forma linear memory mailbox.
    let has_param = |name: &str| forma.params.iter().any(|p| p.name == name && p.ty == Type::I64);
    let mx_expr = if has_param("mx") {
        var("mx")
    } else {
        mem_load(FORMA_MEM_MX)
    };
    let my_expr = if has_param("my") {
        var("my")
    } else {
        mem_load(FORMA_MEM_MY)
    };
    let click_expr = if has_param("m_clicked") {
        var("m_clicked")
    } else if has_param("clicked") {
        var("clicked")
    } else {
        mem_load(FORMA_MEM_CLICKED)
    };
    let wheel_expr = if has_param("wheel_dy") {
        var("wheel_dy")
    } else {
        mem_load(FORMA_MEM_WHEEL)
    };

    lowerer.emit_let("_f_mx", mx_expr);
    lowerer.emit_let("_f_my", my_expr);
    lowerer.emit_let("_f_click", click_expr);
    lowerer.emit_let("_f_wheel", wheel_expr);

    lowerer.stmts.push(Stmt::Let {
        name: "_f_clicked".to_string(),
        mutable: true,
        ty: Some(Type::I64),
        value: Expr::IntLit(0),
    });
    lowerer.stmts.push(Stmt::Let {
        name: "_f_hovered".to_string(),
        mutable: true,
        ty: Some(Type::I64),
        value: Expr::IntLit(0),
    });
    lowerer.stmts.push(Stmt::Let {
        name: "_f_ov_hit".to_string(),
        mutable: true,
        ty: Some(Type::Bool),
        value: Expr::BoolLit(false),
    });

    // Step 1: Compute single-source-of-truth geometry for all nodes.
    let rx0 = forma.root.props.x.clone().unwrap_or(Expr::IntLit(0));
    let ry0 = forma.root.props.y.clone().unwrap_or(Expr::IntLit(0));
    let rw0 = if forma.root.props.w_fit {
        lowerer.compute_intrinsic_w(&forma.root)
    } else {
        forma.root.props.w.clone().unwrap_or(Expr::IntLit(0))
    };
    let rh0 = if forma.root.props.h_fit {
        lowerer.compute_intrinsic_h(&forma.root)
    } else {
        forma.root.props.h.clone().unwrap_or(Expr::IntLit(0))
    };
    let rcx0 = lowerer.emit_let("_f_root_cx0", rx0.clone());
    let rcy0 = lowerer.emit_let("_f_root_cy0", ry0.clone());
    let rcx1 = lowerer.emit_let("_f_root_cx1", bin(BinOp::Add, var(&rcx0), rw0.clone()));
    let rcy1 = lowerer.emit_let("_f_root_cy1", bin(BinOp::Add, var(&rcy0), rh0.clone()));
    let root_clip_rect = (rcx0, rcy0, rcx1, rcy1);

    lowerer.lower_node_geometry(
        &forma.root,
        rx0,
        ry0,
        rw0,
        rh0,
        Expr::BoolLit(true),
        Expr::IntLit(0),
        root_clip_rect.clone(),
        &root_clip_rect,
        false,
        None,
    );

    // Step 2: Priority hit-test for visible Overlay nodes (top-most first).
    let overlay_nodes: Vec<LoweredNodeInfo> = lowerer
        .nodes
        .iter()
        .filter(|n| n.in_overlay)
        .cloned()
        .collect();
    for n in overlay_nodes.iter().rev() {
        let hit_cond = bin(
            BinOp::And,
            var(&n.vis_var),
            point_in_rect("_f_mx", "_f_my", &n.x_var, &n.y_var, &n.w_var, &n.h_var),
        );
        if n.kind == FormaNodeKind::Overlay {
            lowerer.stmts.push(Stmt::Expr(if_unit(
                hit_cond,
                vec![Stmt::Assign {
                    name: "_f_ov_hit".to_string(),
                    value: Expr::BoolLit(true),
                }],
            )));
        } else if matches!(n.kind, FormaNodeKind::Button | FormaNodeKind::Dropdown) {
            emit_interactive_hit(&mut lowerer.stmts, n, hit_cond, true);
        } else if matches!(
            n.kind,
            FormaNodeKind::Toggle | FormaNodeKind::Slider | FormaNodeKind::TextInput
        ) {
            emit_widget_hit(&mut lowerer.stmts, n, hit_cond, true);
        }
    }

    // Step 3: Normal tree hit-test (suppressed if an Overlay intercepted the pointer).
    let normal_nodes: Vec<LoweredNodeInfo> = lowerer
        .nodes
        .iter()
        .filter(|n| !n.in_overlay)
        .cloned()
        .collect();
    for n in normal_nodes.iter().rev() {
        let not_ov = Expr::Unary {
            op: UnaryOp::Not,
            expr: Box::new(var("_f_ov_hit")),
        };
        let hit_cond = bin(
            BinOp::And,
            bin(BinOp::And, not_ov, var(&n.vis_var)),
            point_in_rect("_f_mx", "_f_my", &n.x_var, &n.y_var, &n.w_var, &n.h_var),
        );
        if matches!(n.kind, FormaNodeKind::Button | FormaNodeKind::Dropdown) {
            emit_interactive_hit(&mut lowerer.stmts, n, hit_cond, false);
        } else if matches!(
            n.kind,
            FormaNodeKind::Toggle | FormaNodeKind::Slider | FormaNodeKind::TextInput
        ) {
            emit_widget_hit(&mut lowerer.stmts, n, hit_cond, false);
        } else if n.kind == FormaNodeKind::Slot {
            if let Some(wheel_fn) = &n.on_wheel_fn {
                let wheel_cond = bin(
                    BinOp::And,
                    bin(BinOp::Ne, var("_f_wheel"), Expr::IntLit(0)),
                    hit_cond,
                );
                lowerer.stmts.push(Stmt::Expr(if_unit(
                    wheel_cond,
                    vec![Stmt::Expr(call_named(
                        wheel_fn,
                        vec![
                            var(&n.x_var),
                            var(&n.y_var),
                            var(&n.w_var),
                            var(&n.h_var),
                            var(&n.clip_var),
                        ],
                    ))],
                )));
            }
        }
    }

    // Step 4: Render normal tree in paint order (1-Draw-Call GPU stream).
    for n in &normal_nodes {
        emit_render_node(&mut lowerer.stmts, n);
    }

    // Step 5: Deferred Overlay render pass (guaranteed above all normal nodes).
    let mut max_group = 0usize;
    for n in &overlay_nodes {
        if let Some(g) = n.overlay_group {
            max_group = max_group.max(g + 1);
        }
    }
    for g in 0..max_group {
        for n in overlay_nodes.iter().filter(|n| n.overlay_group == Some(g)) {
            emit_render_node(&mut lowerer.stmts, n);
        }
    }

    // Step 6: Store hovered ID register and return clicked action ID (or unit).
    lowerer.stmts.push(Stmt::Expr(Expr::MemStore {
        index: Box::new(Expr::IntLit(FORMA_MEM_HOVERED_OUT)),
        value: Box::new(var("_f_hovered")),
        inv: None,
        fixed_ty: false,
    }));

    let tail = if forma.ret_type == Type::Unit {
        Expr::UnitLit
    } else {
        var("_f_clicked")
    };

    let mut effects = forma.effects.clone();
    if !effects.iter().any(|e| e.name == "Mem") {
        effects.push(EffectCap {
            name: "Mem".to_string(),
            scope: None,
        });
    }

    FnDef {
        name: forma.name.clone(),
        exported: forma.exported,
        source_file: None,
        span: None,
        params: forma.params.clone(),
        effects,
        ret_type: forma.ret_type.clone(),
        requires: forma.requires.clone(),
        ensures: forma.ensures.clone(),
        body: Expr::Block {
            stmts: lowerer.stmts,
            tail: Box::new(tail),
        },
    }
}

fn emit_interactive_hit(
    stmts: &mut Vec<Stmt>,
    n: &LoweredNodeInfo,
    hit_cond: Expr,
    is_overlay: bool,
) {
    let hover_id = n
        .node_id_expr
        .clone()
        .unwrap_or(Expr::IntLit((n.id + 1) as i64));
    let click_id = n
        .on_click_expr
        .clone()
        .or_else(|| n.node_id_expr.clone())
        .unwrap_or(Expr::IntLit((n.id + 1) as i64));

    let mut body = Vec::new();
    if is_overlay {
        body.push(Stmt::Assign {
            name: "_f_ov_hit".to_string(),
            value: Expr::BoolLit(true),
        });
    }
    body.push(Stmt::Expr(if_unit(
        bin(BinOp::Eq, var("_f_hovered"), Expr::IntLit(0)),
        vec![Stmt::Assign {
            name: "_f_hovered".to_string(),
            value: hover_id,
        }],
    )));
    body.push(Stmt::Expr(if_unit(
        bin(
            BinOp::And,
            bin(BinOp::Ne, var("_f_click"), Expr::IntLit(0)),
            bin(BinOp::Eq, var("_f_clicked"), Expr::IntLit(0)),
        ),
        vec![Stmt::Assign {
            name: "_f_clicked".to_string(),
            value: click_id,
        }],
    )));
    stmts.push(Stmt::Expr(if_unit(hit_cond, body)));
}

fn emit_widget_hit(
    stmts: &mut Vec<Stmt>,
    n: &LoweredNodeInfo,
    hit_cond: Expr,
    is_overlay: bool,
) {
    let raw_id = n
        .node_id_expr
        .clone()
        .unwrap_or(Expr::IntLit((n.id + 1) as i64));
    let hover_val = if n.kind == FormaNodeKind::TextInput {
        Expr::Unary {
            op: UnaryOp::Neg,
            expr: Box::new(raw_id),
        }
    } else {
        raw_id
    };
    let mut body = Vec::new();
    if is_overlay {
        body.push(Stmt::Assign {
            name: "_f_ov_hit".to_string(),
            value: Expr::BoolLit(true),
        });
    }
    body.push(Stmt::Expr(if_unit(
        bin(BinOp::Eq, var("_f_hovered"), Expr::IntLit(0)),
        vec![Stmt::Assign {
            name: "_f_hovered".to_string(),
            value: hover_val,
        }],
    )));
    stmts.push(Stmt::Expr(if_unit(hit_cond, body)));
}

fn emit_render_node(stmts: &mut Vec<Stmt>, n: &LoweredNodeInfo) {
    let mut body = Vec::new();

    if let Some(cid_expr) = &n.clip_prop {
        body.push(Stmt::Expr(call_named(
            HELPER_SET_CLIP,
            vec![
                cid_expr.clone(),
                var(&n.clip_x0),
                var(&n.clip_y0),
                var(&n.clip_x1),
                var(&n.clip_y1),
            ],
        )));
    }

    match n.kind {
        FormaNodeKind::Spacer => {}
        FormaNodeKind::Divider => {
            let col = n
                .bg
                .clone()
                .or_else(|| n.border_col.clone())
                .unwrap_or(Expr::IntLit(0x44FFFFFF));
            body.push(Stmt::Expr(call_named(
                HELPER_PUSH_RECT,
                vec![
                    var(&n.x_var),
                    var(&n.y_var),
                    var(&n.w_var),
                    var(&n.h_var),
                    Expr::IntLit(0),
                    Expr::IntLit(0),
                    Expr::IntLit(0),
                    col,
                    Expr::IntLit(0),
                    var(&n.clip_var),
                ],
            )));
        }
        FormaNodeKind::Slot => {
            if let Some(render_fn) = &n.render_fn {
                body.push(Stmt::Expr(call_named(
                    render_fn,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        var(&n.clip_var),
                    ],
                )));
            }
        }
        FormaNodeKind::Toggle => {
            let wid = n
                .node_id_expr
                .clone()
                .unwrap_or(Expr::IntLit((n.id + 1) as i64));
            let bind_addr = n.bind.clone().unwrap_or(Expr::IntLit(0));
            let checked_expr = n.checked.clone().unwrap_or(Expr::BoolLit(false));
            let hovered_i64 = Expr::If {
                cond: Box::new(bin(BinOp::Eq, var("_f_hovered"), wid)),
                then_branch: Box::new(Expr::IntLit(1)),
                else_branch: Box::new(Expr::IntLit(0)),
            };
            let bg = n.bg.clone().unwrap_or(Expr::IntLit(0xFF33_4155_i64));
            let accent = n.accent.clone().unwrap_or(Expr::IntLit(0xFF3B_82F6_i64));
            let border_col = n.border_col.clone().unwrap_or(Expr::IntLit(0xFF47_5569_i64));
            let ev_var = format!("_ftog_{}", n.id);
            body.push(Stmt::Let {
                name: ev_var.clone(),
                mutable: false,
                ty: None,
                value: call_named(
                    HELPER_TOGGLE,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        bind_addr,
                        checked_expr,
                        hovered_i64,
                        var("_f_click"),
                        bg,
                        accent,
                        border_col,
                        var(&n.clip_var),
                    ],
                ),
            });
            if let Some(act_expr) = n.on_change.clone().or_else(|| n.on_click_expr.clone()) {
                body.push(Stmt::Expr(if_unit(
                    bin(
                        BinOp::And,
                        bin(BinOp::Ne, var(&ev_var), Expr::IntLit(0)),
                        bin(BinOp::Eq, var("_f_clicked"), Expr::IntLit(0)),
                    ),
                    vec![Stmt::Assign {
                        name: "_f_clicked".to_string(),
                        value: act_expr,
                    }],
                )));
            }
        }
        FormaNodeKind::Slider => {
            let wid = n
                .node_id_expr
                .clone()
                .unwrap_or(Expr::IntLit((n.id + 1) as i64));
            let bind_addr = n.bind.clone().unwrap_or(Expr::IntLit(0));
            let min_v = n.min.clone().unwrap_or(Expr::IntLit(0));
            let max_v = n.max.clone().unwrap_or(Expr::IntLit(100));
            let step_v = n.step.clone().unwrap_or(Expr::IntLit(1));
            let hovered_i64 = Expr::If {
                cond: Box::new(bin(BinOp::Eq, var("_f_hovered"), wid.clone())),
                then_branch: Box::new(Expr::IntLit(1)),
                else_branch: Box::new(Expr::IntLit(0)),
            };
            let bg = n.bg.clone().unwrap_or(Expr::IntLit(0xFF1E_293B_i64));
            let accent = n.accent.clone().unwrap_or(Expr::IntLit(0xFF3B_82F6_i64));
            let border_col = n.border_col.clone().unwrap_or(Expr::IntLit(0xFF47_5569_i64));
            let ev_var = format!("_fsld_{}", n.id);
            body.push(Stmt::Let {
                name: ev_var.clone(),
                mutable: false,
                ty: None,
                value: call_named(
                    HELPER_SLIDER,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        bind_addr,
                        min_v,
                        max_v,
                        step_v,
                        wid,
                        hovered_i64,
                        var("_f_click"),
                        bg,
                        accent,
                        border_col,
                        var(&n.clip_var),
                    ],
                ),
            });
            if let Some(act_expr) = n.on_change.clone().or_else(|| n.on_click_expr.clone()) {
                body.push(Stmt::Expr(if_unit(
                    bin(
                        BinOp::And,
                        bin(BinOp::Ne, var(&ev_var), Expr::IntLit(0)),
                        bin(BinOp::Eq, var("_f_clicked"), Expr::IntLit(0)),
                    ),
                    vec![Stmt::Assign {
                        name: "_f_clicked".to_string(),
                        value: act_expr,
                    }],
                )));
            }
        }
        FormaNodeKind::TextInput => {
            let wid = n
                .node_id_expr
                .clone()
                .unwrap_or(Expr::IntLit((n.id + 1) as i64));
            let neg_wid = Expr::Unary {
                op: UnaryOp::Neg,
                expr: Box::new(wid.clone()),
            };
            let buf_addr = n.buf.clone().or_else(|| n.bind.clone()).unwrap_or(Expr::IntLit(0));
            let max_len = n.max_len.clone().unwrap_or(Expr::IntLit(128));
            let hovered_i64 = Expr::If {
                cond: Box::new(bin(BinOp::Eq, var("_f_hovered"), neg_wid)),
                then_branch: Box::new(Expr::IntLit(1)),
                else_branch: Box::new(Expr::IntLit(0)),
            };
            let bg = n.bg.clone().unwrap_or(Expr::IntLit(0xFF1E_293B_i64));
            let border_col = n.border_col.clone().unwrap_or(Expr::IntLit(0xFF47_5569_i64));
            let accent = n.accent.clone().unwrap_or(Expr::IntLit(0xFF3B_82F6_i64));
            let text_col = n.text_color.clone().unwrap_or(Expr::IntLit(0xFFF8_FAFC_i64));
            let muted_col = Expr::IntLit(0xFF94_A3B8_i64);
            let sz = n.text_size.clone().unwrap_or(Expr::IntLit(14));
            let mask_expr = n.mask.clone().unwrap_or(Expr::BoolLit(false));
            let ph_ptr_var = format!("_fph_{}", n.id);
            if let Some(ph_expr) = &n.placeholder {
                body.push(Stmt::Expr(Expr::Perform {
                    effect: "Mem".to_string(),
                    op: "store_str".to_string(),
                    args: vec![Expr::IntLit(FORMA_MEM_STR_SCRATCH), ph_expr.clone()],
                }));
                body.push(Stmt::Let {
                    name: ph_ptr_var.clone(),
                    mutable: false,
                    ty: None,
                    value: mem_load(FORMA_MEM_STR_SCRATCH),
                });
            } else {
                body.push(Stmt::Let {
                    name: ph_ptr_var.clone(),
                    mutable: false,
                    ty: None,
                    value: Expr::IntLit(0),
                });
            }
            let ev_var = format!("_ftin_{}", n.id);
            body.push(Stmt::Let {
                name: ev_var.clone(),
                mutable: false,
                ty: None,
                value: call_named(
                    HELPER_TEXT_INPUT,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        buf_addr,
                        max_len,
                        wid,
                        hovered_i64,
                        var("_f_click"),
                        bg,
                        border_col,
                        accent,
                        text_col,
                        muted_col,
                        sz,
                        mask_expr,
                        var(&ph_ptr_var),
                        var(&n.clip_var),
                    ],
                ),
            });
            if let Some(sub_expr) = n.on_submit.clone() {
                body.push(Stmt::Expr(if_unit(
                    bin(
                        BinOp::And,
                        bin(BinOp::Eq, var(&ev_var), Expr::IntLit(2)),
                        bin(BinOp::Eq, var("_f_clicked"), Expr::IntLit(0)),
                    ),
                    vec![Stmt::Assign {
                        name: "_f_clicked".to_string(),
                        value: sub_expr,
                    }],
                )));
            }
            if let Some(chg_expr) = n.on_change.clone() {
                body.push(Stmt::Expr(if_unit(
                    bin(
                        BinOp::And,
                        bin(BinOp::Eq, var(&ev_var), Expr::IntLit(1)),
                        bin(BinOp::Eq, var("_f_clicked"), Expr::IntLit(0)),
                    ),
                    vec![Stmt::Assign {
                        name: "_f_clicked".to_string(),
                        value: chg_expr,
                    }],
                )));
            }
        }
        FormaNodeKind::Text => {
            if n.has_visual_rect {
                let base_bg = n.bg.clone().unwrap_or(Expr::IntLit(0));
                let radius = n.radius.clone().unwrap_or(Expr::IntLit(0));
                let border = n.border.clone().unwrap_or(Expr::IntLit(0));
                let shadow = n.shadow.clone().unwrap_or(Expr::IntLit(0));
                let border_col = n.border_col.clone().unwrap_or(Expr::IntLit(0));
                body.push(Stmt::Expr(call_named(
                    HELPER_PUSH_RECT,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        radius,
                        border,
                        shadow,
                        base_bg,
                        border_col,
                        var(&n.clip_var),
                    ],
                )));
            }
            if let Some(text_expr) = &n.text_expr {
                let size_expr = n.text_size.clone().unwrap_or(Expr::IntLit(16));
                let style_expr = n.text_style.clone().unwrap_or(Expr::IntLit(0));
                let color_expr = n
                    .text_color
                    .clone()
                    .unwrap_or(Expr::IntLit(0xFFFF_FFFF_i64));
                let align_code = match n.text_align {
                    Align::Center => 1,
                    Align::End => 2,
                    Align::Start | Align::Stretch => 0,
                };
                let overflow_code = n.text_overflow.as_i64();
                body.push(Stmt::Expr(call_named(
                    HELPER_DRAW_TEXT,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        text_expr.clone(),
                        size_expr,
                        style_expr,
                        color_expr,
                        Expr::IntLit(align_code),
                        Expr::IntLit(overflow_code),
                        var(&n.clip_var),
                    ],
                )));
            }
        }
        _ => {
            if n.has_visual_rect || n.kind == FormaNodeKind::Rect {
                let base_bg = n.bg.clone().unwrap_or(Expr::IntLit(0));
                let fill_expr = if let Some(hbg) = &n.hover_bg {
                    let hover_id = n
                        .node_id_expr
                        .clone()
                        .unwrap_or(Expr::IntLit((n.id + 1) as i64));
                    Expr::If {
                        cond: Box::new(bin(BinOp::Eq, var("_f_hovered"), hover_id)),
                        then_branch: Box::new(hbg.clone()),
                        else_branch: Box::new(base_bg),
                    }
                } else {
                    base_bg
                };
                let radius = n.radius.clone().unwrap_or(Expr::IntLit(0));
                let border = n.border.clone().unwrap_or(Expr::IntLit(0));
                let shadow = n.shadow.clone().unwrap_or(Expr::IntLit(0));
                let border_col = n.border_col.clone().unwrap_or(Expr::IntLit(0));
                body.push(Stmt::Expr(call_named(
                    HELPER_PUSH_RECT,
                    vec![
                        var(&n.x_var),
                        var(&n.y_var),
                        var(&n.w_var),
                        var(&n.h_var),
                        radius,
                        border,
                        shadow,
                        fill_expr,
                        border_col,
                        var(&n.clip_var),
                    ],
                )));
            }
        }
    }

    if !body.is_empty() {
        stmts.push(Stmt::Expr(if_unit(var(&n.vis_var), body)));
    }
}

/// Generate the shared `Infimum.Forma` runtime helper functions (`$forma_clamp`, `$forma_set_clip`,
/// `$forma_push_rect`) targeting the `infimum-web` 1-Draw-Call linear memory ABI.
pub fn forma_runtime_helpers() -> Vec<FnDef> {
    forma_runtime_helpers_with_text(HELPER_CP_ATLAS, HELPER_GLYPH_PX)
}

pub fn forma_runtime_helpers_with_text(cp_atlas_fn: &str, glyph_px_fn: &str) -> Vec<FnDef> {
    let mem_eff = vec![EffectCap {
        name: "Mem".to_string(),
        scope: None,
    }];

    // fn $forma_clamp(v: i64, lo: i64, hi: i64) -> i64
    let clamp_fn = FnDef {
        name: HELPER_CLAMP.to_string(),
        exported: false,
        source_file: None,
        span: None,
        params: vec![
            Param {
                name: "v".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "lo".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "hi".to_string(),
                ty: Type::I64,
            },
        ],
        effects: Vec::new(),
        ret_type: Type::I64,
        requires: Vec::new(),
        ensures: Vec::new(),
        body: Expr::If {
            cond: Box::new(bin(BinOp::Lt, var("v"), var("lo"))),
            then_branch: Box::new(var("lo")),
            else_branch: Box::new(Expr::If {
                cond: Box::new(bin(BinOp::Gt, var("v"), var("hi"))),
                then_branch: Box::new(var("hi")),
                else_branch: Box::new(var("v")),
            }),
        },
    };

    // fn $forma_set_clip(cid: i64, x0: i64, y0: i64, x1: i64, y1: i64) -[Mem]-> unit
    let set_clip_fn = FnDef {
        name: HELPER_SET_CLIP.to_string(),
        exported: false,
        source_file: None,
        span: None,
        params: vec![
            Param {
                name: "cid".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "x0".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "y0".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "x1".to_string(),
                ty: Type::I64,
            },
            Param {
                name: "y1".to_string(),
                ty: Type::I64,
            },
        ],
        effects: mem_eff.clone(),
        ret_type: Type::Unit,
        requires: Vec::new(),
        ensures: Vec::new(),
        body: if_unit(
            bin(
                BinOp::And,
                bin(BinOp::Ge, var("cid"), Expr::IntLit(0)),
                bin(BinOp::Lt, var("cid"), Expr::IntLit(16)),
            ),
            vec![
                Stmt::Let {
                    name: "b".to_string(),
                    mutable: false,
                    ty: None,
                    value: bin(
                        BinOp::Add,
                        Expr::IntLit(FORMA_SYS_CLIP_RECTS),
                        bin(BinOp::Mul, var("cid"), Expr::IntLit(4)),
                    ),
                },
                mem_store_stmt(var("b"), var("x0")),
                mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(1)), var("y0")),
                mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(2)), var("x1")),
                mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(3)), var("y1")),
            ],
        ),
    };

    // fn $forma_push_rect(x, y, w, h, radius, border, shadow, fill, border_col, clip_id) -[Mem]-> unit
    let push_rect_fn = FnDef {
        name: HELPER_PUSH_RECT.to_string(),
        exported: false,
        source_file: None,
        span: None,
        params: vec![
            Param { name: "x".to_string(), ty: Type::I64 },
            Param { name: "y".to_string(), ty: Type::I64 },
            Param { name: "w".to_string(), ty: Type::I64 },
            Param { name: "h".to_string(), ty: Type::I64 },
            Param { name: "radius".to_string(), ty: Type::I64 },
            Param { name: "border".to_string(), ty: Type::I64 },
            Param { name: "shadow".to_string(), ty: Type::I64 },
            Param { name: "fill".to_string(), ty: Type::I64 },
            Param { name: "border_col".to_string(), ty: Type::I64 },
            Param { name: "clip_id".to_string(), ty: Type::I64 },
        ],
        effects: mem_eff.clone(),
        ret_type: Type::Unit,
        requires: Vec::new(),
        ensures: Vec::new(),
        body: if_unit(
            bin(
                BinOp::And,
                bin(BinOp::Gt, var("w"), Expr::IntLit(0)),
                bin(BinOp::Gt, var("h"), Expr::IntLit(0)),
            ),
            vec![
                Stmt::Let {
                    name: "idx".to_string(),
                    mutable: false,
                    ty: None,
                    value: mem_load(12),
                },
                Stmt::Expr(if_unit(
                    bin(BinOp::Lt, var("idx"), Expr::IntLit(80_000)),
                    vec![
                        Stmt::Let {
                            name: "r".to_string(),
                            mutable: false,
                            ty: None,
                            value: call_named(
                                HELPER_CLAMP,
                                vec![var("radius"), Expr::IntLit(0), Expr::IntLit(120)],
                            ),
                        },
                        Stmt::Let {
                            name: "bw".to_string(),
                            mutable: false,
                            ty: None,
                            value: call_named(
                                HELPER_CLAMP,
                                vec![var("border"), Expr::IntLit(0), Expr::IntLit(15)],
                            ),
                        },
                        Stmt::Let {
                            name: "sh".to_string(),
                            mutable: false,
                            ty: None,
                            value: call_named(
                                HELPER_CLAMP,
                                vec![var("shadow"), Expr::IntLit(0), Expr::IntLit(60)],
                            ),
                        },
                        Stmt::Let {
                            name: "pmeta".to_string(),
                            mutable: false,
                            ty: None,
                            value: bin(
                                BinOp::BitOr,
                                bin(
                                    BinOp::BitOr,
                                    bin(BinOp::Shl, var("r"), Expr::IntLit(4)),
                                    bin(BinOp::Shl, var("bw"), Expr::IntLit(12)),
                                ),
                                bin(BinOp::Shl, var("sh"), Expr::IntLit(16)),
                            ),
                        },
                        Stmt::Let {
                            name: "b".to_string(),
                            mutable: false,
                            ty: None,
                            value: bin(
                                BinOp::Add,
                                Expr::IntLit(65_536),
                                bin(BinOp::Mul, var("idx"), Expr::IntLit(8)),
                            ),
                        },
                        mem_store_stmt(var("b"), var("x")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(1)), var("y")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(2)), var("w")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(3)), var("h")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(4)), var("pmeta")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(5)), var("fill")),
                        mem_store_stmt(
                            bin(BinOp::Add, var("b"), Expr::IntLit(6)),
                            var("border_col"),
                        ),
                        mem_store_stmt(
                            bin(BinOp::Add, var("b"), Expr::IntLit(7)),
                            bin(BinOp::BitAnd, var("clip_id"), Expr::IntLit(15)),
                        ),
                        mem_store_stmt(
                            Expr::IntLit(12),
                            bin(BinOp::Add, var("idx"), Expr::IntLit(1)),
                        ),
                    ],
                )),
            ],
        ),
    };

    // `codepoint_to_atlas` and `glyph_px` come from `std/text.ifm` via `widgets_rt.ifm`
    // (single source of truth for the font atlas mapper).

    // fn $forma_measure_text(s: str, size: i64, style: i64) -[Mem]-> i64
    let measure_text_fn = FnDef {
        name: HELPER_MEASURE_TEXT.to_string(),
        exported: false,
        source_file: None,
        span: None,
        params: vec![
            Param { name: "s".to_string(), ty: Type::Str },
            Param { name: "size".to_string(), ty: Type::I64 },
            Param { name: "style".to_string(), ty: Type::I64 },
        ],
        effects: mem_eff.clone(),
        ret_type: Type::I64,
        requires: Vec::new(),
        ensures: Vec::new(),
        body: Expr::Block {
            stmts: vec![
                Stmt::Expr(Expr::Perform {
                    effect: "Mem".to_string(),
                    op: "store_str".to_string(),
                    args: vec![Expr::IntLit(FORMA_MEM_STR_SCRATCH), var("s")],
                }),
                Stmt::Let {
                    name: "ptr".to_string(),
                    mutable: false,
                    ty: None,
                    value: mem_load(FORMA_MEM_STR_SCRATCH),
                },
                Stmt::Let {
                    name: "n".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![
                            Expr::MemLoad {
                                index: Box::new(var("ptr")),
        inv: None,
        fixed_ty: false,
                            },
                            Expr::IntLit(0),
                            Expr::IntLit(256),
                        ],
                    ),
                },
                Stmt::Let {
                    name: "sz".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![var("size"), Expr::IntLit(8), Expr::IntLit(96)],
                    ),
                },
                Stmt::Let {
                    name: "st".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![var("style"), Expr::IntLit(0), Expr::IntLit(2)],
                    ),
                },
                Stmt::Let {
                    name: "tw".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::IntLit(0),
                },
                Stmt::Let {
                    name: "i".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::IntLit(0),
                },
                Stmt::While {
                    cond: bin(BinOp::Lt, var("i"), var("n")),
                    max_iters: 256,
                    body: vec![
                        Stmt::Let {
                            name: "cp".to_string(),
                            mutable: false,
                            ty: None,
                            value: Expr::MemLoad {
                                index: Box::new(bin(
                                    BinOp::Add,
                                    bin(BinOp::Add, var("ptr"), Expr::IntLit(1)),
                                    var("i"),
                                )),
        inv: None,
        fixed_ty: false,
                            },
                        },
                        Stmt::Assign {
                            name: "tw".to_string(),
                            value: bin(
                                BinOp::Add,
                                var("tw"),
                                call_named(glyph_px_fn, vec![var("cp"), var("sz"), var("st")]),
                            ),
                        },
                        Stmt::Assign {
                            name: "i".to_string(),
                            value: bin(BinOp::Add, var("i"), Expr::IntLit(1)),
                        },
                    ],
                },
            ],
            tail: Box::new(var("tw")),
        },
    };

    // Helper: emit a single atlas glyph primitive at (gx, gy).
    let emit_glyph_stmts = |gx: Expr, gy: Expr, g_idx: Expr| -> Vec<Stmt> {
        vec![Stmt::Expr(if_unit(
            bin(BinOp::Gt, g_idx.clone(), Expr::IntLit(0)),
            vec![
                Stmt::Let {
                    name: "idx".to_string(),
                    mutable: false,
                    ty: None,
                    value: mem_load(12),
                },
                Stmt::Expr(if_unit(
                    bin(BinOp::Lt, var("idx"), Expr::IntLit(80_000)),
                    vec![
                        Stmt::Let {
                            name: "pmeta".to_string(),
                            mutable: false,
                            ty: None,
                            value: bin(
                                BinOp::BitOr,
                                bin(
                                    BinOp::BitOr,
                                    Expr::IntLit(1),
                                    bin(
                                        BinOp::Shl,
                                        bin(BinOp::BitAnd, g_idx, Expr::IntLit(255)),
                                        Expr::IntLit(4),
                                    ),
                                ),
                                bin(
                                    BinOp::Shl,
                                    bin(BinOp::BitAnd, var("st"), Expr::IntLit(3)),
                                    Expr::IntLit(12),
                                ),
                            ),
                        },
                        Stmt::Let {
                            name: "b".to_string(),
                            mutable: false,
                            ty: None,
                            value: bin(
                                BinOp::Add,
                                Expr::IntLit(65_536),
                                bin(BinOp::Mul, var("idx"), Expr::IntLit(8)),
                            ),
                        },
                        mem_store_stmt(var("b"), gx),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(1)), gy),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(2)), var("sz")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(3)), var("sz")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(4)), var("pmeta")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(5)), var("col")),
                        mem_store_stmt(bin(BinOp::Add, var("b"), Expr::IntLit(6)), Expr::IntLit(0)),
                        mem_store_stmt(
                            bin(BinOp::Add, var("b"), Expr::IntLit(7)),
                            bin(BinOp::BitAnd, var("clip_id"), Expr::IntLit(15)),
                        ),
                        mem_store_stmt(
                            Expr::IntLit(12),
                            bin(BinOp::Add, var("idx"), Expr::IntLit(1)),
                        ),
                    ],
                )),
            ],
        ))]
    };

    // fn $forma_draw_text(rx, ry, rw, rh, s, size, style, col, align_code, overflow_code, clip_id) -[Mem]-> unit
    let draw_text_fn = FnDef {
        name: HELPER_DRAW_TEXT.to_string(),
        exported: false,
        source_file: None,
        span: None,
        params: vec![
            Param { name: "rx".to_string(), ty: Type::I64 },
            Param { name: "ry".to_string(), ty: Type::I64 },
            Param { name: "rw".to_string(), ty: Type::I64 },
            Param { name: "rh".to_string(), ty: Type::I64 },
            Param { name: "s".to_string(), ty: Type::Str },
            Param { name: "size".to_string(), ty: Type::I64 },
            Param { name: "style".to_string(), ty: Type::I64 },
            Param { name: "col".to_string(), ty: Type::I64 },
            Param { name: "align_code".to_string(), ty: Type::I64 },
            Param { name: "overflow_code".to_string(), ty: Type::I64 },
            Param { name: "clip_id".to_string(), ty: Type::I64 },
        ],
        effects: mem_eff,
        ret_type: Type::Unit,
        requires: Vec::new(),
        ensures: Vec::new(),
        body: Expr::Block {
            stmts: vec![
                Stmt::Expr(Expr::Perform {
                    effect: "Mem".to_string(),
                    op: "store_str".to_string(),
                    args: vec![Expr::IntLit(FORMA_MEM_STR_SCRATCH), var("s")],
                }),
                Stmt::Let {
                    name: "ptr".to_string(),
                    mutable: false,
                    ty: None,
                    value: mem_load(FORMA_MEM_STR_SCRATCH),
                },
                Stmt::Let {
                    name: "n".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![
                            Expr::MemLoad {
                                index: Box::new(var("ptr")),
        inv: None,
        fixed_ty: false,
                            },
                            Expr::IntLit(0),
                            Expr::IntLit(256),
                        ],
                    ),
                },
                Stmt::Let {
                    name: "sz".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![var("size"), Expr::IntLit(8), Expr::IntLit(96)],
                    ),
                },
                Stmt::Let {
                    name: "st".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        HELPER_CLAMP,
                        vec![var("style"), Expr::IntLit(0), Expr::IntLit(2)],
                    ),
                },
                Stmt::Let {
                    name: "tw".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::IntLit(0),
                },
                Stmt::Expr(if_unit(
                    bin(
                        BinOp::Or,
                        bin(BinOp::Ne, var("align_code"), Expr::IntLit(0)),
                        bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(2)),
                    ),
                    vec![
                        Stmt::Let {
                            name: "mi".to_string(),
                            mutable: true,
                            ty: None,
                            value: Expr::IntLit(0),
                        },
                        Stmt::While {
                            cond: bin(BinOp::Lt, var("mi"), var("n")),
                            max_iters: 256,
                            body: vec![
                                Stmt::Let {
                                    name: "mcp".to_string(),
                                    mutable: false,
                                    ty: None,
                                    value: Expr::MemLoad {
                                        index: Box::new(bin(
                                            BinOp::Add,
                                            bin(BinOp::Add, var("ptr"), Expr::IntLit(1)),
                                            var("mi"),
                                        )),
        inv: None,
        fixed_ty: false,
                                    },
                                },
                                Stmt::Assign {
                                    name: "tw".to_string(),
                                    value: bin(
                                        BinOp::Add,
                                        var("tw"),
                                        call_named(
                                            glyph_px_fn,
                                            vec![var("mcp"), var("sz"), var("st")],
                                        ),
                                    ),
                                },
                                Stmt::Assign {
                                    name: "mi".to_string(),
                                    value: bin(BinOp::Add, var("mi"), Expr::IntLit(1)),
                                },
                            ],
                        },
                    ],
                )),
                Stmt::Let {
                    name: "need_ell".to_string(),
                    mutable: false,
                    ty: None,
                    value: bin(
                        BinOp::And,
                        bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(2)),
                        bin(BinOp::Gt, var("tw"), var("rw")),
                    ),
                },
                Stmt::Let {
                    name: "dot_adv".to_string(),
                    mutable: false,
                    ty: None,
                    value: call_named(
                        glyph_px_fn,
                        vec![Expr::IntLit(46), var("sz"), var("st")],
                    ),
                },
                Stmt::Let {
                    name: "limit_x".to_string(),
                    mutable: false,
                    ty: None,
                    value: Expr::If {
                        cond: Box::new(var("need_ell")),
                        then_branch: Box::new(bin(
                            BinOp::Sub,
                            bin(BinOp::Add, var("rx"), var("rw")),
                            bin(BinOp::Mul, var("dot_adv"), Expr::IntLit(3)),
                        )),
                        else_branch: Box::new(bin(BinOp::Add, var("rx"), var("rw"))),
                    },
                },
                Stmt::Let {
                    name: "cx".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::If {
                        cond: Box::new(bin(
                            BinOp::Or,
                            var("need_ell"),
                            bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(3)),
                        )),
                        then_branch: Box::new(var("rx")),
                        else_branch: Box::new(Expr::If {
                            cond: Box::new(bin(BinOp::Eq, var("align_code"), Expr::IntLit(1))),
                            then_branch: Box::new(bin(
                                BinOp::Add,
                                var("rx"),
                                bin(BinOp::Div, bin(BinOp::Sub, var("rw"), var("tw")), Expr::IntLit(2)),
                            )),
                            else_branch: Box::new(Expr::If {
                                cond: Box::new(bin(BinOp::Eq, var("align_code"), Expr::IntLit(2))),
                                then_branch: Box::new(bin(
                                    BinOp::Add,
                                    var("rx"),
                                    bin(BinOp::Sub, var("rw"), var("tw")),
                                )),
                                else_branch: Box::new(var("rx")),
                            }),
                        }),
                    },
                },
                Stmt::Let {
                    name: "cy".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::If {
                        cond: Box::new(bin(
                            BinOp::And,
                            bin(BinOp::Ne, var("overflow_code"), Expr::IntLit(3)),
                            bin(BinOp::Gt, var("rh"), var("sz")),
                        )),
                        then_branch: Box::new(bin(
                            BinOp::Add,
                            var("ry"),
                            bin(BinOp::Div, bin(BinOp::Sub, var("rh"), var("sz")), Expr::IntLit(2)),
                        )),
                        else_branch: Box::new(var("ry")),
                    },
                },
                Stmt::Let {
                    name: "lh".to_string(),
                    mutable: false,
                    ty: None,
                    value: bin(
                        BinOp::Add,
                        var("sz"),
                        bin(BinOp::Div, var("sz"), Expr::IntLit(4)),
                    ),
                },
                Stmt::Let {
                    name: "i".to_string(),
                    mutable: true,
                    ty: None,
                    value: Expr::IntLit(0),
                },
                Stmt::While {
                    cond: bin(BinOp::Lt, var("i"), var("n")),
                    max_iters: 256,
                    body: vec![
                        Stmt::Let {
                            name: "cp".to_string(),
                            mutable: false,
                            ty: None,
                            value: Expr::MemLoad {
                                index: Box::new(bin(
                                    BinOp::Add,
                                    bin(BinOp::Add, var("ptr"), Expr::IntLit(1)),
                                    var("i"),
                                )),
        inv: None,
        fixed_ty: false,
                            },
                        },
                        Stmt::Let {
                            name: "adv".to_string(),
                            mutable: false,
                            ty: None,
                            value: call_named(glyph_px_fn, vec![var("cp"), var("sz"), var("st")]),
                        },
                        // Wrap mode (overflow_code == 3): newline or line-wrap at rx + rw
                        Stmt::Expr(if_unit(
                            bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(3)),
                            vec![Stmt::Expr(if_unit(
                                bin(
                                    BinOp::Or,
                                    bin(BinOp::Eq, var("cp"), Expr::IntLit(10)),
                                    bin(
                                        BinOp::And,
                                        bin(BinOp::Gt, var("cx"), var("rx")),
                                        bin(
                                            BinOp::Gt,
                                            bin(BinOp::Add, var("cx"), var("adv")),
                                            bin(BinOp::Add, var("rx"), var("rw")),
                                        ),
                                    ),
                                ),
                                vec![
                                    Stmt::Assign {
                                        name: "cx".to_string(),
                                        value: var("rx"),
                                    },
                                    Stmt::Assign {
                                        name: "cy".to_string(),
                                        value: bin(BinOp::Add, var("cy"), var("lh")),
                                    },
                                ],
                            ))],
                        )),
                        // Clip / Ellipsis / Wrap bounds stop
                        Stmt::Let {
                            name: "fits".to_string(),
                            mutable: false,
                            ty: None,
                            value: Expr::If {
                                cond: Box::new(bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(3))),
                                then_branch: Box::new(bin(
                                    BinOp::Le,
                                    bin(BinOp::Add, var("cy"), var("sz")),
                                    bin(BinOp::Add, var("ry"), var("rh")),
                                )),
                                else_branch: Box::new(Expr::If {
                                    cond: Box::new(bin(
                                        BinOp::Or,
                                        bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(1)),
                                        var("need_ell"),
                                    )),
                                    then_branch: Box::new(bin(
                                        BinOp::Le,
                                        bin(BinOp::Add, var("cx"), var("adv")),
                                        var("limit_x"),
                                    )),
                                    else_branch: Box::new(Expr::BoolLit(true)),
                                }),
                            },
                        },
                        Stmt::Expr(Expr::If {
                            cond: Box::new(var("fits")),
                            then_branch: Box::new(Expr::Block {
                                stmts: {
                                    let mut st_emit = Vec::new();
                                    let skip_nl = bin(
                                        BinOp::And,
                                        bin(BinOp::Eq, var("overflow_code"), Expr::IntLit(3)),
                                        bin(BinOp::Eq, var("cp"), Expr::IntLit(10)),
                                    );
                                    let mut draw_body = vec![Stmt::Let {
                                        name: "g_idx".to_string(),
                                        mutable: false,
                                        ty: None,
                                        value: call_named(cp_atlas_fn, vec![var("cp")]),
                                    }];
                                    draw_body.extend(emit_glyph_stmts(
                                        var("cx"),
                                        var("cy"),
                                        var("g_idx"),
                                    ));
                                    draw_body.push(Stmt::Assign {
                                        name: "cx".to_string(),
                                        value: bin(BinOp::Add, var("cx"), var("adv")),
                                    });
                                    st_emit.push(Stmt::Expr(if_unit(
                                        Expr::Unary {
                                            op: UnaryOp::Not,
                                            expr: Box::new(skip_nl),
                                        },
                                        draw_body,
                                    )));
                                    st_emit.push(Stmt::Assign {
                                        name: "i".to_string(),
                                        value: bin(BinOp::Add, var("i"), Expr::IntLit(1)),
                                    });
                                    st_emit
                                },
                                tail: Box::new(Expr::UnitLit),
                            }),
                            else_branch: Box::new(Expr::Block {
                                stmts: vec![Stmt::Assign {
                                    name: "i".to_string(),
                                    value: var("n"),
                                }],
                                tail: Box::new(Expr::UnitLit),
                            }),
                        }),
                    ],
                },
                // Append "..." when overflow == ellipsis and text exceeded rw
                Stmt::Expr(if_unit(
                    var("need_ell"),
                    vec![
                        Stmt::Let {
                            name: "dot_g".to_string(),
                            mutable: false,
                            ty: None,
                            value: call_named(cp_atlas_fn, vec![Expr::IntLit(46)]),
                        },
                        Stmt::Let {
                            name: "di".to_string(),
                            mutable: true,
                            ty: None,
                            value: Expr::IntLit(0),
                        },
                        Stmt::While {
                            cond: bin(
                                BinOp::And,
                                bin(BinOp::Lt, var("di"), Expr::IntLit(3)),
                                bin(
                                    BinOp::Le,
                                    bin(BinOp::Add, var("cx"), var("dot_adv")),
                                    bin(BinOp::Add, var("rx"), var("rw")),
                                ),
                            ),
                            max_iters: 3,
                            body: {
                                let mut db = emit_glyph_stmts(var("cx"), var("cy"), var("dot_g"));
                                db.push(Stmt::Assign {
                                    name: "cx".to_string(),
                                    value: bin(BinOp::Add, var("cx"), var("dot_adv")),
                                });
                                db.push(Stmt::Assign {
                                    name: "di".to_string(),
                                    value: bin(BinOp::Add, var("di"), Expr::IntLit(1)),
                                });
                                db
                            },
                        },
                    ],
                )),
            ],
            tail: Box::new(Expr::UnitLit),
        },
    };

    let helpers = vec![
        clamp_fn,
        set_clip_fn,
        push_rect_fn,
        measure_text_fn,
        draw_text_fn,
    ];
    helpers
}

fn var(name: &str) -> Expr {
    Expr::Var(name.to_string())
}

fn bin(op: BinOp, lhs: Expr, rhs: Expr) -> Expr {
    Expr::Binary {
        op,
        lhs: Box::new(lhs),
        rhs: Box::new(rhs),
    }
}

fn mem_load(addr: i64) -> Expr {
    Expr::MemLoad {
        index: Box::new(Expr::IntLit(addr)),
        inv: None,
        fixed_ty: false,
    }
}

fn mem_store_stmt(index: Expr, value: Expr) -> Stmt {
    Stmt::Expr(Expr::MemStore {
        index: Box::new(index),
        value: Box::new(value),
        inv: None,
        fixed_ty: false,
    })
}

fn call_named(name: &str, args: Vec<Expr>) -> Expr {
    Expr::Call {
        target: TargetRef::Named(name.to_string()),
        args,
    }
}

fn if_unit(cond: Expr, then_stmts: Vec<Stmt>) -> Expr {
    Expr::If {
        cond: Box::new(cond),
        then_branch: Box::new(Expr::Block {
            stmts: then_stmts,
            tail: Box::new(Expr::UnitLit),
        }),
        else_branch: Box::new(Expr::UnitLit),
    }
}

fn max_expr(a: Expr, b: Expr) -> Expr {
    Expr::If {
        cond: Box::new(bin(BinOp::Ge, a.clone(), b.clone())),
        then_branch: Box::new(a),
        else_branch: Box::new(b),
    }
}

fn min_expr(a: Expr, b: Expr) -> Expr {
    Expr::If {
        cond: Box::new(bin(BinOp::Le, a.clone(), b.clone())),
        then_branch: Box::new(a),
        else_branch: Box::new(b),
    }
}

fn sum_exprs(terms: &[Expr]) -> Expr {
    let mut iter = terms.iter();
    let Some(first) = iter.next() else {
        return Expr::IntLit(0);
    };
    iter.fold(first.clone(), |acc, e| bin(BinOp::Add, acc, e.clone()))
}

fn align_pos_expr(start: Expr, avail: Expr, size: Expr, align: Align) -> Expr {
    match align {
        Align::Start | Align::Stretch => start,
        Align::Center => bin(
            BinOp::Add,
            start,
            bin(BinOp::Div, bin(BinOp::Sub, avail, size), Expr::IntLit(2)),
        ),
        Align::End => bin(BinOp::Add, start, bin(BinOp::Sub, avail, size)),
    }
}

fn point_in_rect(mx: &str, my: &str, rx: &str, ry: &str, rw: &str, rh: &str) -> Expr {
    bin(
        BinOp::And,
        bin(
            BinOp::And,
            bin(BinOp::Ge, var(mx), var(rx)),
            bin(BinOp::Lt, var(mx), bin(BinOp::Add, var(rx), var(rw))),
        ),
        bin(
            BinOp::And,
            bin(BinOp::Ge, var(my), var(ry)),
            bin(BinOp::Lt, var(my), bin(BinOp::Add, var(ry), var(rh))),
        ),
    )
}
