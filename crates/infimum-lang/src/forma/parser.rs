// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

//! Parser for declarative `Infimum.Forma` UI blocks (`forma Name(...) { ... }`).

use crate::ast::{EffectCap, Param, Type};
use crate::diag::{InfimumDiagnostic, Span};
use crate::forma::ast::{Align, FormaDef, FormaNode, FormaNodeKind, FormaProps, Justify, Overflow};
use crate::lexer::{Token, TokenKind};

/// Host parser interface supplied by `crate::parser::Parser` so `forma` blocks
/// reuse the full Infimum expression, type, parameter, and effect grammar.
pub(crate) trait FormaHostParser {
    type Expr;

    fn peek(&self) -> &Token;
    fn peek_next_is_colon(&self) -> bool;
    fn advance(&mut self) -> Token;
    fn expect_ident(&mut self, what: &str) -> Result<String, InfimumDiagnostic>;
    fn parse_params(&mut self) -> Result<Vec<Param>, InfimumDiagnostic>;
    fn parse_effect_and_ret(&mut self) -> Result<(Vec<EffectCap>, Type), InfimumDiagnostic>;
    fn parse_expr(&mut self) -> Result<Self::Expr, InfimumDiagnostic>;
}

fn expect_tok<P: FormaHostParser>(
    p: &mut P,
    expected: &TokenKind,
    desc: &str,
) -> Result<Span, InfimumDiagnostic> {
    if &p.peek().kind == expected {
        Ok(p.advance().span)
    } else {
        let tok = p.peek();
        Err(InfimumDiagnostic::new(
            "E091_FORMA_SYNTAX",
            "forma-parse",
            format!("Expected {desc} in forma block, found {:?}", tok.kind),
            tok.span,
            format!("Insert {desc} according to the Infimum.Forma grammar"),
        ))
    }
}

/// Parse a top-level `[pub] [export] forma Name(...) [req/ens]* [-[Eff]-> Ret] [req/ens]* { <FormaNode> }`.
pub(crate) fn parse_forma_def<P: FormaHostParser>(
    p: &mut P,
    exported: bool,
    is_pub: bool,
) -> Result<FormaDef<P::Expr>, InfimumDiagnostic> {
    let forma_span = expect_tok(p, &TokenKind::Forma, "'forma'")?;
    let name = p.expect_ident("forma component name")?;
    let params = p.parse_params()?;

    let mut requires = Vec::new();
    let mut ensures = Vec::new();
    parse_contracts(p, &mut requires, &mut ensures)?;

    let (effects, ret_type) =
        if matches!(p.peek().kind, TokenKind::EffectOpen | TokenKind::Arrow) {
            p.parse_effect_and_ret()?
        } else {
            (
                vec![EffectCap {
                    name: "Mem".to_string(),
                    scope: None,
                }],
                Type::I64,
            )
        };

    // Also allow contracts after the effect/return arrow (matching `fn` syntax).
    parse_contracts(p, &mut requires, &mut ensures)?;

    expect_tok(p, &TokenKind::LBrace, "'{' to open forma body")?;
    let root = parse_forma_node(p)?;
    expect_tok(p, &TokenKind::RBrace, "'}' to close forma body")?;

    Ok(FormaDef {
        name,
        exported,
        is_pub,
        params,
        effects,
        ret_type,
        requires,
        ensures,
        root,
        span: forma_span,
    })
}

fn parse_contracts<P: FormaHostParser>(
    p: &mut P,
    requires: &mut Vec<P::Expr>,
    ensures: &mut Vec<P::Expr>,
) -> Result<(), InfimumDiagnostic> {
    loop {
        match p.peek().kind {
            TokenKind::Req => {
                p.advance();
                requires.push(p.parse_expr()?);
            }
            TokenKind::Ens => {
                p.advance();
                ensures.push(p.parse_expr()?);
            }
            _ => break,
        }
    }
    Ok(())
}

fn parse_forma_node<P: FormaHostParser>(p: &mut P) -> Result<FormaNode<P::Expr>, InfimumDiagnostic> {
    let span = p.peek().span;

    // Conditional UI block: `if cond { Child(...) } [else { Other(...) }]`
    if p.peek().kind == TokenKind::If {
        p.advance();
        let cond = p.parse_expr()?;
        expect_tok(p, &TokenKind::LBrace, "'{' after forma 'if' condition")?;
        let mut then_children = Vec::new();
        while p.peek().kind != TokenKind::RBrace && p.peek().kind != TokenKind::Eof {
            then_children.push(parse_forma_node(p)?);
        }
        expect_tok(p, &TokenKind::RBrace, "'}' to close forma 'if' block")?;
        let mut else_children = Vec::new();
        if p.peek().kind == TokenKind::Else {
            p.advance();
            if p.peek().kind == TokenKind::If {
                else_children.push(parse_forma_node(p)?);
            } else {
                expect_tok(p, &TokenKind::LBrace, "'{' after forma 'else'")?;
                while p.peek().kind != TokenKind::RBrace && p.peek().kind != TokenKind::Eof {
                    else_children.push(parse_forma_node(p)?);
                }
                expect_tok(p, &TokenKind::RBrace, "'}' to close forma 'else' block")?;
            }
        }
        return Ok(FormaNode {
            kind: FormaNodeKind::Box,
            props: FormaProps::default(),
            children: then_children,
            span,
            component_call: None,
            forma_for: None,
            forma_if: Some((cond, else_children)),
        });
    }

    // Static unrolled loop: `for i in 0..N { Child(...) }` or `0..=N`
    if p.peek().kind == TokenKind::For {
        p.advance();
        let var = p.expect_ident("loop variable name after 'for'")?;
        let in_kw = p.expect_ident("'in' after forma 'for' variable")?;
        if in_kw != "in" {
            return Err(InfimumDiagnostic::new(
                "E091_FORMA_SYNTAX",
                "forma-parse",
                format!("Expected 'in' after 'for {var}', found '{in_kw}'"),
                span,
                "Write: for i in 0..N { ... }",
            ));
        }
        let start = p.parse_expr()?;
        let inclusive = if p.peek().kind == TokenKind::DotDotEq {
            p.advance();
            true
        } else if p.peek().kind == TokenKind::DotDot {
            p.advance();
            false
        } else {
            return Err(InfimumDiagnostic::new(
                "E091_FORMA_SYNTAX",
                "forma-parse",
                format!("Expected '..' or '..=' in forma 'for' range, found {:?}", p.peek().kind),
                p.peek().span,
                "Write: for i in 0..N { ... } or for i in 0..=N { ... }",
            ));
        };
        let end = p.parse_expr()?;
        expect_tok(p, &TokenKind::LBrace, "'{' to open forma 'for' body")?;
        let mut body_children = Vec::new();
        while p.peek().kind != TokenKind::RBrace && p.peek().kind != TokenKind::Eof {
            body_children.push(parse_forma_node(p)?);
        }
        expect_tok(p, &TokenKind::RBrace, "'}' to close forma 'for' body")?;
        return Ok(FormaNode {
            kind: FormaNodeKind::Box,
            props: FormaProps::default(),
            children: body_children,
            span,
            component_call: None,
            forma_for: Some((var, start, end, inclusive)),
            forma_if: None,
        });
    }

    let mut kind_name = p.expect_ident("Forma node kind (Box, Col, Row, ..., or custom forma component)")?;
    if p.peek().kind == TokenKind::ColonColon {
        p.advance();
        let sub = p.expect_ident("Forma component name after '::'")?;
        kind_name = format!("{kind_name}::{sub}");
    }
    let Some(kind) = FormaNodeKind::from_ident(&kind_name) else {
        // Custom nested `forma` component call: `ScoreCard("Вы", score)`
        let mut args = Vec::new();
        if p.peek().kind == TokenKind::LParen {
            p.advance();
            while p.peek().kind != TokenKind::RParen && p.peek().kind != TokenKind::Eof {
                args.push(p.parse_expr()?);
                if p.peek().kind == TokenKind::Comma {
                    p.advance();
                } else {
                    break;
                }
            }
            expect_tok(p, &TokenKind::RParen, "')' to close Forma component arguments")?;
        }
        return Ok(FormaNode {
            kind: FormaNodeKind::Box,
            props: FormaProps::default(),
            children: Vec::new(),
            span,
            component_call: Some((kind_name, args)),
            forma_for: None,
            forma_if: None,
        });
    };

    let props = if p.peek().kind == TokenKind::LParen {
        parse_forma_props(p, kind)?
    } else {
        FormaProps::default()
    };

    let mut children = Vec::new();
    if p.peek().kind == TokenKind::LBrace {
        p.advance();
        while p.peek().kind != TokenKind::RBrace && p.peek().kind != TokenKind::Eof {
            children.push(parse_forma_node(p)?);
        }
        expect_tok(p, &TokenKind::RBrace, "'}' to close Forma node children")?;
    }

    Ok(FormaNode {
        kind,
        props,
        children,
        span,
        component_call: None,
        forma_for: None,
        forma_if: None,
    })
}

fn parse_forma_props<P: FormaHostParser>(
    p: &mut P,
    kind: FormaNodeKind,
) -> Result<FormaProps<P::Expr>, InfimumDiagnostic> {
    expect_tok(p, &TokenKind::LParen, "'('")?;
    let mut props = FormaProps::default();

    // Support shorthand positional expression as first argument of `Text(...)`:
    // e.g. `Text("Hello", size: 24)` or `Text(summary, size: 12, shrink: 1)`.
    let is_named_prop =
        matches!(p.peek().kind, TokenKind::Ident(_)) && p.peek_next_is_colon();
    if p.peek().kind != TokenKind::RParen
        && (matches!(p.peek().kind, TokenKind::Str(_))
            || (kind == FormaNodeKind::Text && !is_named_prop))
    {
        props.text = Some(p.parse_expr()?);
        if p.peek().kind == TokenKind::Comma {
            p.advance();
        }
    }

    while p.peek().kind != TokenKind::RParen && p.peek().kind != TokenKind::Eof {
        let prop_span = p.peek().span;
        let prop_name = if p.peek().kind == TokenKind::Max {
            p.advance();
            "max".to_string()
        } else {
            p.expect_ident("Forma property name")?
        };
        expect_tok(p, &TokenKind::Colon, "':' after property name")?;

        match prop_name.as_str() {
            "x" => props.x = Some(p.parse_expr()?),
            "y" => props.y = Some(p.parse_expr()?),
            "w" => {
                if matches!(&p.peek().kind, TokenKind::Ident(s) if s == "fit") {
                    p.advance();
                    props.w_fit = true;
                } else {
                    props.w = Some(p.parse_expr()?);
                }
            }
            "h" => {
                if matches!(&p.peek().kind, TokenKind::Ident(s) if s == "fit") {
                    p.advance();
                    props.h_fit = true;
                } else {
                    props.h = Some(p.parse_expr()?);
                }
            }
            "min_w" => props.min_w = Some(p.parse_expr()?),
            "min_h" => props.min_h = Some(p.parse_expr()?),
            "max_w" => props.max_w = Some(p.parse_expr()?),
            "max_h" => props.max_h = Some(p.parse_expr()?),
            "grow" => props.grow = Some(p.parse_expr()?),
            "shrink" => props.shrink = Some(p.parse_expr()?),
            "pad" => props.pad = Some(p.parse_expr()?),
            "pad_x" => props.pad_x = Some(p.parse_expr()?),
            "pad_y" => props.pad_y = Some(p.parse_expr()?),
            "gap" => props.gap = Some(p.parse_expr()?),
            "align" => {
                let a_span = p.peek().span;
                let a_name = p.expect_ident("alignment (start | center | end | stretch)")?;
                let align = Align::from_ident(&a_name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E091_FORMA_SYNTAX",
                        "forma-parse",
                        format!("Invalid alignment '{a_name}'"),
                        a_span,
                        "Use one of: start, center, end, stretch",
                    )
                })?;
                props.align = Some(align);
            }
            "justify" => {
                let j_span = p.peek().span;
                let j_name = p.expect_ident(
                    "justify (start | center | end | space_between | space_around)",
                )?;
                let justify = Justify::from_ident(&j_name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E091_FORMA_SYNTAX",
                        "forma-parse",
                        format!("Invalid justify '{j_name}'"),
                        j_span,
                        "Use one of: start, center, end, space_between, space_around",
                    )
                })?;
                props.justify = Some(justify);
            }
            "overflow" => {
                let o_span = p.peek().span;
                let o_name = p.expect_ident("overflow (visible | clip | ellipsis | wrap)")?;
                let overflow = Overflow::from_ident(&o_name).ok_or_else(|| {
                    InfimumDiagnostic::new(
                        "E091_FORMA_SYNTAX",
                        "forma-parse",
                        format!("Invalid overflow '{o_name}'"),
                        o_span,
                        "Use one of: visible, clip, ellipsis, wrap",
                    )
                })?;
                props.overflow = Some(overflow);
            }
            "bg" => props.bg = Some(p.parse_expr()?),
            "hover_bg" => props.hover_bg = Some(p.parse_expr()?),
            "border" => props.border = Some(p.parse_expr()?),
            "border_col" => props.border_col = Some(p.parse_expr()?),
            "radius" => props.radius = Some(p.parse_expr()?),
            "shadow" => props.shadow = Some(p.parse_expr()?),
            "clip" => props.clip = Some(p.parse_expr()?),
            "id" => props.id = Some(p.parse_expr()?),
            "on_click" => props.on_click = Some(p.parse_expr()?),
            "visible" => props.visible = Some(p.parse_expr()?),
            "render" => {
                let mut fn_name = p.expect_ident("Slot render function name")?;
                if p.peek().kind == TokenKind::ColonColon {
                    p.advance();
                    let sub = p.expect_ident("Slot render function name after '::'")?;
                    fn_name = format!("{fn_name}::{sub}");
                }
                props.render = Some(fn_name);
            }
            "on_wheel" => {
                let mut fn_name = p.expect_ident("Slot on_wheel function name")?;
                if p.peek().kind == TokenKind::ColonColon {
                    p.advance();
                    let sub = p.expect_ident("Slot on_wheel function name after '::'")?;
                    fn_name = format!("{fn_name}::{sub}");
                }
                props.on_wheel = Some(fn_name);
            }
            "text" => props.text = Some(p.parse_expr()?),
            "size" => props.size = Some(p.parse_expr()?),
            "style" => props.style = Some(p.parse_expr()?),
            "color" => props.color = Some(p.parse_expr()?),
            "checked" => props.checked = Some(p.parse_expr()?),
            "bind" => props.bind = Some(p.parse_expr()?),
            "min" => props.min = Some(p.parse_expr()?),
            "max" => props.max = Some(p.parse_expr()?),
            "step" => props.step = Some(p.parse_expr()?),
            "buf" => props.buf = Some(p.parse_expr()?),
            "max_len" => props.max_len = Some(p.parse_expr()?),
            "placeholder" => props.placeholder = Some(p.parse_expr()?),
            "mask" => props.mask = Some(p.parse_expr()?),
            "on_change" => props.on_change = Some(p.parse_expr()?),
            "on_submit" => props.on_submit = Some(p.parse_expr()?),
            "accent" => props.accent = Some(p.parse_expr()?),
            other => {
                return Err(InfimumDiagnostic::new(
                    "E091_FORMA_SYNTAX",
                    "forma-parse",
                    format!("Unknown Forma property '{other}'"),
                    prop_span,
                    "Supported properties: x, y, w (or fit), h (or fit), min_w, min_h, max_w, max_h, grow, shrink, pad, pad_x, pad_y, gap, align, justify, overflow, bg, hover_bg, border, border_col, radius, shadow, clip, id, on_click, visible, render, on_wheel, text, size, style, color, checked, bind, min, max, step, buf, max_len, placeholder, mask, on_change, on_submit, accent",
                ));
            }
        }

        if p.peek().kind == TokenKind::Comma {
            p.advance();
        } else {
            break;
        }
    }

    expect_tok(p, &TokenKind::RParen, "')' to close Forma properties")?;
    Ok(props)
}
