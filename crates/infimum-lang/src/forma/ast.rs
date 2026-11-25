// Copyright (c) 2026 Yehuda Aminadav
// SPDX-License-Identifier: AGPL-3.0-only OR LicenseRef-Infimum-Commercial

//! AST types for `Infimum.Forma` — the spatial-interactive UI layer.

use crate::ast::{EffectCap, Expr, Param, Type};
use crate::diag::Span;
use serde::{Deserialize, Serialize};

/// Cross-axis alignment inside `Row`, `Col`, `Box`, `Stack`, `Button`, and `Dropdown`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Align {
    #[default]
    Start,
    Center,
    End,
    Stretch,
}

impl Align {
    pub fn from_ident(s: &str) -> Option<Self> {
        match s {
            "start" => Some(Align::Start),
            "center" => Some(Align::Center),
            "end" => Some(Align::End),
            "stretch" => Some(Align::Stretch),
            _ => None,
        }
    }
}

/// Main-axis distribution inside `Row` and `Col` (CSS Flexbox `justify-content`).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Justify {
    #[default]
    Start,
    Center,
    End,
    SpaceBetween,
    SpaceAround,
}

impl Justify {
    pub fn from_ident(s: &str) -> Option<Self> {
        match s {
            "start" => Some(Justify::Start),
            "center" => Some(Justify::Center),
            "end" => Some(Justify::End),
            "space_between" => Some(Justify::SpaceBetween),
            "space_around" => Some(Justify::SpaceAround),
            _ => None,
        }
    }
}

/// Overflow handling for `Text` and constrained containers.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum Overflow {
    #[default]
    Visible,
    Clip,
    Ellipsis,
    Wrap,
}

impl Overflow {
    pub fn from_ident(s: &str) -> Option<Self> {
        match s {
            "visible" => Some(Overflow::Visible),
            "clip" => Some(Overflow::Clip),
            "ellipsis" => Some(Overflow::Ellipsis),
            "wrap" => Some(Overflow::Wrap),
            _ => None,
        }
    }

    pub fn as_i64(self) -> i64 {
        match self {
            Overflow::Visible => 0,
            Overflow::Clip => 1,
            Overflow::Ellipsis => 2,
            Overflow::Wrap => 3,
        }
    }
}

/// Built-in declarative `Infimum.Forma` node kinds.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum FormaNodeKind {
    // Layout containers
    Box,
    Col,
    Row,
    Stack,
    Scroll,
    Overlay,
    // Interactive and leaf nodes
    Button,
    Dropdown,
    Toggle,
    Slider,
    TextInput,
    Spacer,
    Divider,
    Rect,
    Slot,
    Text,
}

impl FormaNodeKind {
    pub fn from_ident(s: &str) -> Option<Self> {
        match s {
            "Box" => Some(Self::Box),
            "Col" => Some(Self::Col),
            "Row" => Some(Self::Row),
            "Stack" => Some(Self::Stack),
            "Scroll" => Some(Self::Scroll),
            "Overlay" => Some(Self::Overlay),
            "Button" => Some(Self::Button),
            "Dropdown" => Some(Self::Dropdown),
            "Toggle" => Some(Self::Toggle),
            "Slider" => Some(Self::Slider),
            "TextInput" => Some(Self::TextInput),
            "Spacer" => Some(Self::Spacer),
            "Divider" => Some(Self::Divider),
            "Rect" => Some(Self::Rect),
            "Slot" => Some(Self::Slot),
            "Text" => Some(Self::Text),
            _ => None,
        }
    }

    pub fn as_str(self) -> &'static str {
        match self {
            Self::Box => "Box",
            Self::Col => "Col",
            Self::Row => "Row",
            Self::Stack => "Stack",
            Self::Scroll => "Scroll",
            Self::Overlay => "Overlay",
            Self::Button => "Button",
            Self::Dropdown => "Dropdown",
            Self::Toggle => "Toggle",
            Self::Slider => "Slider",
            Self::TextInput => "TextInput",
            Self::Spacer => "Spacer",
            Self::Divider => "Divider",
            Self::Rect => "Rect",
            Self::Slot => "Slot",
            Self::Text => "Text",
        }
    }

    /// Interactive nodes must prove non-degeneracy (`rw > 0 && rh > 0`) under Z3.
    pub fn is_interactive(self) -> bool {
        matches!(
            self,
            Self::Button | Self::Dropdown | Self::Scroll | Self::Toggle | Self::Slider | Self::TextInput
        )
    }

    pub fn is_overlay(self) -> bool {
        matches!(self, Self::Overlay)
    }

    pub fn is_scroll(self) -> bool {
        matches!(self, Self::Scroll)
    }
}

/// Declarative properties on a `FormaNode`, generic over expression representation `E`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormaProps<E = Expr> {
    pub x: Option<E>,
    pub y: Option<E>,
    pub w: Option<E>,
    pub h: Option<E>,
    #[serde(default)]
    pub w_fit: bool,
    #[serde(default)]
    pub h_fit: bool,
    pub min_w: Option<E>,
    pub min_h: Option<E>,
    pub max_w: Option<E>,
    pub max_h: Option<E>,
    pub grow: Option<E>,
    pub shrink: Option<E>,
    pub pad: Option<E>,
    pub pad_x: Option<E>,
    pub pad_y: Option<E>,
    pub gap: Option<E>,
    pub align: Option<Align>,
    pub justify: Option<Justify>,
    pub overflow: Option<Overflow>,
    pub bg: Option<E>,
    pub hover_bg: Option<E>,
    pub border: Option<E>,
    pub border_col: Option<E>,
    pub radius: Option<E>,
    pub shadow: Option<E>,
    pub clip: Option<E>,
    pub id: Option<E>,
    pub on_click: Option<E>,
    pub visible: Option<E>,
    pub render: Option<String>,
    pub on_wheel: Option<String>,
    pub text: Option<E>,
    pub size: Option<E>,
    pub style: Option<E>,
    pub color: Option<E>,
    // Interactive widget properties (Toggle / Slider / TextInput)
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub checked: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub bind: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub min: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub max: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub step: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub buf: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub max_len: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub placeholder: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub mask: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub on_change: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub on_submit: Option<E>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub accent: Option<E>,
}

impl<E> Default for FormaProps<E> {
    fn default() -> Self {
        Self {
            x: None,
            y: None,
            w: None,
            h: None,
            w_fit: false,
            h_fit: false,
            min_w: None,
            min_h: None,
            max_w: None,
            max_h: None,
            grow: None,
            shrink: None,
            pad: None,
            pad_x: None,
            pad_y: None,
            gap: None,
            align: None,
            justify: None,
            overflow: None,
            bg: None,
            hover_bg: None,
            border: None,
            border_col: None,
            radius: None,
            shadow: None,
            clip: None,
            id: None,
            on_click: None,
            visible: None,
            render: None,
            on_wheel: None,
            text: None,
            size: None,
            style: None,
            color: None,
            checked: None,
            bind: None,
            min: None,
            max: None,
            step: None,
            buf: None,
            max_len: None,
            placeholder: None,
            mask: None,
            on_change: None,
            on_submit: None,
            accent: None,
        }
    }
}

impl<E> FormaProps<E> {
    pub fn has_visual_rect(&self) -> bool {
        self.bg.is_some()
            || self.hover_bg.is_some()
            || self.border.is_some()
            || self.border_col.is_some()
            || self.shadow.is_some()
    }

    pub fn try_map<T, Err, F>(self, f: &mut F) -> Result<FormaProps<T>, Err>
    where
        F: FnMut(E) -> Result<T, Err>,
    {
        Ok(FormaProps {
            x: self.x.map(&mut *f).transpose()?,
            y: self.y.map(&mut *f).transpose()?,
            w: self.w.map(&mut *f).transpose()?,
            h: self.h.map(&mut *f).transpose()?,
            w_fit: self.w_fit,
            h_fit: self.h_fit,
            min_w: self.min_w.map(&mut *f).transpose()?,
            min_h: self.min_h.map(&mut *f).transpose()?,
            max_w: self.max_w.map(&mut *f).transpose()?,
            max_h: self.max_h.map(&mut *f).transpose()?,
            grow: self.grow.map(&mut *f).transpose()?,
            shrink: self.shrink.map(&mut *f).transpose()?,
            pad: self.pad.map(&mut *f).transpose()?,
            pad_x: self.pad_x.map(&mut *f).transpose()?,
            pad_y: self.pad_y.map(&mut *f).transpose()?,
            gap: self.gap.map(&mut *f).transpose()?,
            align: self.align,
            justify: self.justify,
            overflow: self.overflow,
            bg: self.bg.map(&mut *f).transpose()?,
            hover_bg: self.hover_bg.map(&mut *f).transpose()?,
            border: self.border.map(&mut *f).transpose()?,
            border_col: self.border_col.map(&mut *f).transpose()?,
            radius: self.radius.map(&mut *f).transpose()?,
            shadow: self.shadow.map(&mut *f).transpose()?,
            clip: self.clip.map(&mut *f).transpose()?,
            id: self.id.map(&mut *f).transpose()?,
            on_click: self.on_click.map(&mut *f).transpose()?,
            visible: self.visible.map(&mut *f).transpose()?,
            render: self.render,
            on_wheel: self.on_wheel,
            text: self.text.map(&mut *f).transpose()?,
            size: self.size.map(&mut *f).transpose()?,
            style: self.style.map(&mut *f).transpose()?,
            color: self.color.map(&mut *f).transpose()?,
            checked: self.checked.map(&mut *f).transpose()?,
            bind: self.bind.map(&mut *f).transpose()?,
            min: self.min.map(&mut *f).transpose()?,
            max: self.max.map(&mut *f).transpose()?,
            step: self.step.map(&mut *f).transpose()?,
            buf: self.buf.map(&mut *f).transpose()?,
            max_len: self.max_len.map(&mut *f).transpose()?,
            placeholder: self.placeholder.map(&mut *f).transpose()?,
            mask: self.mask.map(&mut *f).transpose()?,
            on_change: self.on_change.map(&mut *f).transpose()?,
            on_submit: self.on_submit.map(&mut *f).transpose()?,
            accent: self.accent.map(&mut *f).transpose()?,
        })
    }
}

fn default_none<T>() -> Option<T> {
    None
}

/// A single node in a `forma` UI tree.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormaNode<E = Expr> {
    pub kind: FormaNodeKind,
    pub props: FormaProps<E>,
    pub children: Vec<FormaNode<E>>,
    pub span: Span,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub component_call: Option<(String, Vec<E>)>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub forma_for: Option<(String, E, E, bool)>,
    #[serde(default = "default_none", skip_serializing_if = "Option::is_none")]
    pub forma_if: Option<(E, Vec<FormaNode<E>>)>,
}

impl<E> FormaNode<E> {
    pub fn try_map<T, Err, F>(self, f: &mut F) -> Result<FormaNode<T>, Err>
    where
        F: FnMut(E) -> Result<T, Err>,
    {
        let props = self.props.try_map(f)?;
        let mut children = Vec::with_capacity(self.children.len());
        for child in self.children {
            children.push(child.try_map(f)?);
        }
        let component_call = match self.component_call {
            Some((name, args)) => {
                let mut margs = Vec::with_capacity(args.len());
                for a in args {
                    margs.push(f(a)?);
                }
                Some((name, margs))
            }
            None => None,
        };
        let forma_for = match self.forma_for {
            Some((var, start, end, inc)) => Some((var, f(start)?, f(end)?, inc)),
            None => None,
        };
        let forma_if = match self.forma_if {
            Some((cond, else_nodes)) => {
                let mc = f(cond)?;
                let mut me = Vec::with_capacity(else_nodes.len());
                for en in else_nodes {
                    me.push(en.try_map(f)?);
                }
                Some((mc, me))
            }
            None => None,
        };
        Ok(FormaNode {
            kind: self.kind,
            props,
            children,
            span: self.span,
            component_call,
            forma_for,
            forma_if,
        })
    }

    /// Static upper bound on GPU primitives emitted by this subtree in a single frame.
    pub fn static_prim_upper_bound(&self) -> usize {
        let self_prims = match self.kind {
            FormaNodeKind::Spacer => 0,
            FormaNodeKind::Divider | FormaNodeKind::Rect => 1,
            FormaNodeKind::Slot => 64,
            FormaNodeKind::Toggle => 2,
            FormaNodeKind::Slider => 3,
            FormaNodeKind::TextInput => 514,
            FormaNodeKind::Text => 256 + usize::from(self.props.has_visual_rect()),
            FormaNodeKind::Box
            | FormaNodeKind::Col
            | FormaNodeKind::Row
            | FormaNodeKind::Stack
            | FormaNodeKind::Scroll
            | FormaNodeKind::Overlay
            | FormaNodeKind::Button
            | FormaNodeKind::Dropdown => usize::from(self.props.has_visual_rect()),
        };
        self_prims
            + self
                .children
                .iter()
                .map(FormaNode::static_prim_upper_bound)
                .sum::<usize>()
    }
}

/// Top-level `forma Name(...)` declaration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FormaDef<E = Expr> {
    pub name: String,
    #[serde(default)]
    pub exported: bool,
    #[serde(default)]
    pub is_pub: bool,
    pub params: Vec<Param>,
    pub effects: Vec<EffectCap>,
    pub ret_type: Type,
    pub requires: Vec<E>,
    pub ensures: Vec<E>,
    pub root: FormaNode<E>,
    pub span: Span,
}

impl<E> FormaDef<E> {
    pub fn try_map<T, Err, F>(self, f: &mut F) -> Result<FormaDef<T>, Err>
    where
        F: FnMut(E) -> Result<T, Err>,
    {
        let mut requires = Vec::with_capacity(self.requires.len());
        for r in self.requires {
            requires.push(f(r)?);
        }
        let mut ensures = Vec::with_capacity(self.ensures.len());
        for e in self.ensures {
            ensures.push(f(e)?);
        }
        let root = self.root.try_map(f)?;
        Ok(FormaDef {
            name: self.name,
            exported: self.exported,
            is_pub: self.is_pub,
            params: self.params,
            effects: self.effects,
            ret_type: self.ret_type,
            requires,
            ensures,
            root,
            span: self.span,
        })
    }
}
