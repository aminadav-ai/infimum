// Copyright (c) 2026 Infimum Language Contributors
// License: Apache-2.0

use crate::ast::{BinOp, EffectCap, Param, TargetRef, Type, UnaryOp};
use crate::diag::{InfimumDiagnostic, Span};
use crate::lexer::{Token, TokenKind, tokenize};

use super::raw_ast::{
    RawConst, RawEnum, RawEnumVariant, RawExpr, RawFile, RawFnDef, RawInv, RawItem, RawLayout,
    RawLayoutField, RawLayoutLoad, RawMatchArm, RawMatchPattern, RawMod, RawModCall, RawSliceIndex,
    RawSliceRange, RawSliceStore, RawStmt, RawStruct, RawStructLit, RawType, RawUse, RawUseItem,
    RawUseKind,
};

pub(super) fn parse_raw_file(src: &str) -> Result<RawFile, InfimumDiagnostic> {
    let tokens = tokenize(src)?;
    let mut parser = Parser { tokens, idx: 0 };
    let mut uses = Vec::new();
    let mut items = Vec::new();
    let mut formas = Vec::new();
    let mut functions = Vec::new();

    while !parser.at_eof() {
        let mut is_pub = if parser.peek_is_contextual_kw("pub") {
            parser.advance();
            true
        } else {
            false
        };
        let mut is_export = false;
        if matches!(parser.peek().kind, TokenKind::Export) {
            parser.advance();
            is_export = true;
            if !is_pub && parser.peek_is_contextual_kw("pub") {
                parser.advance();
                is_pub = true;
            }
        }
        match parser.peek().kind {
            TokenKind::Use if !is_export => uses.push(parser.parse_use(is_pub)?),
            TokenKind::Const if !is_export => {
                let mut c = parser.parse_const()?;
                c.is_pub = is_pub;
                items.push(RawItem::Const(c));
            }
            TokenKind::Enum if !is_export => {
                let mut e = parser.parse_enum()?;
                e.is_pub = is_pub;
                items.push(RawItem::Enum(e));
            }
            TokenKind::Layout if !is_export => {
                let mut l = parser.parse_layout()?;
                l.is_pub = is_pub;
                items.push(RawItem::Layout(l));
            }
            _ if !is_export && parser.peek_is_contextual_kw("mod") => {
                let mut rm = parser.parse_mod()?;
                rm.is_pub = is_pub;
                items.push(RawItem::Mod(rm));
            }
            _ if !is_export && parser.peek_is_contextual_kw("struct") => {
                let mut st = parser.parse_struct()?;
                st.is_pub = is_pub;
                items.push(RawItem::Struct(st));
            }
            _ if !is_export && parser.peek_is_contextual_kw("test") => {
                functions.push(parser.parse_test_fn()?)
            }
            TokenKind::Forma => formas.push(crate::forma::parser::parse_forma_def(
                &mut parser,
                is_export,
                is_pub,
            )?),
            _ => {
                let mut f = parser.parse_fn_with_export(is_export)?;
                f.is_pub = is_pub;
                functions.push(f);
            }
        }
    }

    Ok(RawFile {
        uses,
        items,
        formas,
        functions,
    })
}

// ---------------------------------------------------------------------------
// Recursive-Descent Parser
// ---------------------------------------------------------------------------

pub struct Parser {
    pub(super) tokens: Vec<Token>,
    pub(super) idx: usize,
}

impl Parser {
    fn peek(&self) -> &Token {
        &self.tokens[self.idx]
    }

    fn peek_at(&self, offset: usize) -> &TokenKind {
        let pos = self.idx + offset;
        if pos < self.tokens.len() {
            &self.tokens[pos].kind
        } else {
            &TokenKind::Eof
        }
    }

    fn peek_next(&self) -> &TokenKind {
        self.peek_at(1)
    }

    fn at_eof(&self) -> bool {
        matches!(self.peek().kind, TokenKind::Eof)
    }

    fn advance(&mut self) -> Token {
        let tok = self.tokens[self.idx].clone();
        if self.idx + 1 < self.tokens.len() {
            self.idx += 1;
        }
        tok
    }

    fn expect(&mut self, expected: &TokenKind, hint: &str) -> Result<Token, InfimumDiagnostic> {
        if &self.peek().kind == expected {
            Ok(self.advance())
        } else {
            Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected {:?}, found {:?}", expected, self.peek().kind),
                self.peek().span,
                hint,
            ))
        }
    }

    fn expect_ident(&mut self, hint: &str) -> Result<(String, Span), InfimumDiagnostic> {
        let tok = self.peek().clone();
        if let TokenKind::Ident(name) = tok.kind {
            self.advance();
            Ok((name, tok.span))
        } else {
            Err(InfimumDiagnostic::new(
                "E011_EXPECTED_IDENT",
                "parse",
                format!("Expected identifier, found {:?}", tok.kind),
                tok.span,
                hint,
            ))
        }
    }

    fn peek_is_contextual_kw(&self, kw: &str) -> bool {
        matches!(&self.peek().kind, TokenKind::Ident(s) if s == kw)
    }

    fn parse_use(&mut self, is_pub: bool) -> Result<RawUse, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Use, "Expected 'use'")?;
        let path = match self.peek().kind.clone() {
            TokenKind::Str(s) => {
                self.advance();
                s
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E010_UNEXPECTED_TOKEN",
                    "parse",
                    format!("Expected relative file path string after 'use', found {other:?}"),
                    self.peek().span,
                    "Write import directives as: use \"relative/path.ifm\"; or use \"file.ifm\" as alias; or use \"file.ifm\" { a, b as c };",
                ));
            }
        };
        let kind = if self.peek_is_contextual_kw("as") {
            self.advance();
            let (alias, alias_span) = self.expect_ident("Expected namespace alias after 'as'")?;
            RawUseKind::Alias(alias, alias_span)
        } else if matches!(self.peek().kind, TokenKind::LBrace) {
            self.advance();
            let mut items = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                let (item_name, item_span) =
                    self.expect_ident("Expected imported symbol name inside '{ ... }'")?;
                let alias = if self.peek_is_contextual_kw("as") {
                    self.advance();
                    let (al, _) =
                        self.expect_ident("Expected alias name after 'as' in selective import")?;
                    Some(al)
                } else {
                    None
                };
                items.push(RawUseItem {
                    name: item_name,
                    alias,
                    span: item_span,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RBrace, "Expected '}' to close selective import list")?;
            RawUseKind::Selective(items)
        } else {
            RawUseKind::All
        };
        self.expect(&TokenKind::Semi, "Terminate 'use' directive with ';'")?;
        Ok(RawUse {
            is_pub,
            path,
            kind,
            span: tok.span,
        })
    }

    fn parse_const(&mut self) -> Result<RawConst, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Const, "Expected 'const'")?;
        let (name, _) = self.expect_ident("Expected constant name after 'const'")?;
        self.expect(&TokenKind::Colon, "Expected ':' and type after constant name")?;
        let ty = self.parse_type()?;
        if !matches!(ty, Type::I64 | Type::Fixed | Type::Bool) {
            return Err(InfimumDiagnostic::new(
                "E064_CONST_EVAL",
                "parse",
                format!("Constant '{name}' has unsupported type '{ty}'; only 'i64', 'fixed', and 'bool' are allowed"),
                tok.span,
                "Declare constants as 'const NAME: i64 = ...;', 'const G: fixed = 9.81;', or 'const FLAG: bool = ...;'",
            ));
        }
        self.expect(&TokenKind::Assign, "Expected '=' in const declaration")?;
        let expr = self.parse_expr()?;
        self.expect(&TokenKind::Semi, "Terminate 'const' declaration with ';'")?;
        Ok(RawConst {
            name,
            is_pub: false,
            ty,
            expr,
            span: tok.span,
        })
    }

    fn parse_mod(&mut self) -> Result<RawMod, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'mod'")?;
        let (name, _) = self.expect_ident("Expected module name after 'mod'")?;
        if self.peek_is_contextual_kw("from") {
            let from_span = self.peek().span;
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("'mod {name} from \"...\";' has been removed; use 'use \"file.ifm\" as {name};' instead"),
                from_span,
                format!("Replace with: use \"<file>.ifm\" as {name};"),
            ));
        }
        self.expect(&TokenKind::LBrace, "Expected '{' after inline module name")?;
        let mut uses = Vec::new();
        let mut items = Vec::new();
        let mut formas = Vec::new();
        let mut functions = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let mut is_pub = if self.peek_is_contextual_kw("pub") {
                self.advance();
                true
            } else {
                false
            };
            let mut is_export = false;
            if matches!(self.peek().kind, TokenKind::Export) {
                self.advance();
                is_export = true;
                if !is_pub && self.peek_is_contextual_kw("pub") {
                    self.advance();
                    is_pub = true;
                }
            }
            match self.peek().kind {
                TokenKind::Use if !is_export => uses.push(self.parse_use(is_pub)?),
                TokenKind::Const if !is_export => {
                    let mut c = self.parse_const()?;
                    c.is_pub = is_pub;
                    items.push(RawItem::Const(c));
                }
                TokenKind::Enum if !is_export => {
                    let mut e = self.parse_enum()?;
                    e.is_pub = is_pub;
                    items.push(RawItem::Enum(e));
                }
                TokenKind::Layout if !is_export => {
                    let mut l = self.parse_layout()?;
                    l.is_pub = is_pub;
                    items.push(RawItem::Layout(l));
                }
                _ if !is_export && self.peek_is_contextual_kw("mod") => {
                    let mut nested = self.parse_mod()?;
                    nested.is_pub = is_pub;
                    items.push(RawItem::Mod(nested));
                }
                _ if !is_export && self.peek_is_contextual_kw("struct") => {
                    let mut st = self.parse_struct()?;
                    st.is_pub = is_pub;
                    items.push(RawItem::Struct(st));
                }
                TokenKind::Forma => {
                    formas.push(crate::forma::parser::parse_forma_def(self, is_export, is_pub)?)
                }
                _ => {
                    let mut f = self.parse_fn_with_export(is_export)?;
                    f.is_pub = is_pub;
                    functions.push(f);
                }
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close module body")?;
        Ok(RawMod {
            name,
            is_pub: false,
            span,
            uses,
            items,
            formas,
            functions,
        })
    }

    fn parse_enum(&mut self) -> Result<RawEnum, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Enum, "Expected 'enum'")?;
        let (name, _) = self.expect_ident("Expected enum name after 'enum'")?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open enum body")?;

        let mut variants = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (vname, vspan) = self.expect_ident("Expected enum variant name")?;
            let value = if matches!(self.peek().kind, TokenKind::Assign) {
                self.advance();
                Some(self.parse_expr()?)
            } else {
                None
            };
            variants.push(RawEnumVariant {
                name: vname,
                value,
                span: vspan,
            });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close enum body")?;
        Ok(RawEnum {
            name,
            is_pub: false,
            variants,
            span: tok.span,
        })
    }

    fn parse_opt_inv(&mut self) -> Result<Option<RawInv>, InfimumDiagnostic> {
        if !self.peek_is_contextual_kw("inv") {
            return Ok(None);
        }
        let tok = self.advance();
        let lo = self.parse_expr()?;
        let inclusive = if matches!(self.peek().kind, TokenKind::DotDotEq) {
            self.advance();
            true
        } else if matches!(self.peek().kind, TokenKind::DotDot) {
            self.advance();
            false
        } else {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!(
                    "Expected '..=' or '..' in field invariant, found {:?}",
                    self.peek().kind
                ),
                self.peek().span,
                "Write 'inv 0..=7' or 'inv 0..8' after a struct or layout field",
            ));
        };
        let hi = self.parse_expr()?;
        Ok(Some(RawInv {
            lo,
            hi,
            inclusive,
            span: tok.span,
        }))
    }

    fn parse_struct(&mut self) -> Result<RawStruct, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'struct'")?;
        let (name, _) = self.expect_ident("Expected struct name after 'struct'")?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open struct body")?;
        let mut fields = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (fname, fspan) = self.expect_ident("Expected struct field name")?;
            let mut is_fixed = false;
            if matches!(self.peek().kind, TokenKind::Colon) {
                self.advance();
                let (fty, fty_span) =
                    self.expect_ident("Expected 'word', 'i64', or 'fixed' for struct field type")?;
                if fty == "fixed" {
                    is_fixed = true;
                } else if fty != "word" && fty != "i64" {
                    return Err(InfimumDiagnostic::new(
                        "E010_UNEXPECTED_TOKEN",
                        "parse",
                        format!("Unsupported struct field type '{fty}'; expected 'word', 'i64', or 'fixed'"),
                        fty_span,
                        "Every struct field occupies 1 linear-memory word; declare it as 'field: word' or 'field: fixed'",
                    ));
                }
            }
            let inv = self.parse_opt_inv()?;
            fields.push((fname, is_fixed, inv, fspan));
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close struct body")?;
        Ok(RawStruct {
            name,
            is_pub: false,
            fields,
            span,
        })
    }

    fn parse_layout(&mut self) -> Result<RawLayout, InfimumDiagnostic> {
        let tok = self.expect(&TokenKind::Layout, "Expected 'layout'")?;
        let (name, _) = self.expect_ident("Expected layout name after 'layout'")?;
        let base_expr = if self.peek_is_contextual_kw("at") {
            self.advance();
            Some(self.parse_expr()?)
        } else {
            None
        };
        self.expect(&TokenKind::LBrace, "Expected '{' to open layout body")?;

        let mut fields = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let (fname, fspan) = self.expect_ident("Expected layout field name")?;
            self.expect(&TokenKind::Colon, "Expected ':' after layout field name")?;
            let (kind_kw, _) =
                self.expect_ident("Expected 'word', 'words[N]', 'fixed', 'fixed[N]', 'Struct', or 'Struct[N]'")?;
            let (struct_module, struct_type, is_array, is_fixed, len_expr) =
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (sname, _) =
                        self.expect_ident("Expected struct name after '::' in layout field")?;
                    if matches!(self.peek().kind, TokenKind::LBracket) {
                        self.advance();
                        let len = self.parse_expr()?;
                        self.expect(&TokenKind::RBracket, "Expected ']' after struct array length")?;
                        (Some(kind_kw), Some(sname), true, false, Some(len))
                    } else {
                        (Some(kind_kw), Some(sname), false, false, None)
                    }
                } else {
                    match kind_kw.as_str() {
                        "word" => (None, None, false, false, None),
                        "words" => {
                            self.expect(
                                &TokenKind::LBracket,
                                "Expected '[' after 'words' (e.g. words[8200])",
                            )?;
                            let len = self.parse_expr()?;
                            self.expect(&TokenKind::RBracket, "Expected ']' after words length")?;
                            (None, None, true, false, Some(len))
                        }
                        "fixed" => {
                            if matches!(self.peek().kind, TokenKind::LBracket) {
                                self.advance();
                                let len = self.parse_expr()?;
                                self.expect(
                                    &TokenKind::RBracket,
                                    "Expected ']' after fixed array length",
                                )?;
                                (None, None, true, true, Some(len))
                            } else {
                                (None, None, false, true, None)
                            }
                        }
                        other => {
                            if matches!(self.peek().kind, TokenKind::LBracket) {
                                self.advance();
                                let len = self.parse_expr()?;
                                self.expect(
                                    &TokenKind::RBracket,
                                    "Expected ']' after struct array length",
                                )?;
                                (None, Some(other.to_string()), true, false, Some(len))
                            } else {
                                (None, Some(other.to_string()), false, false, None)
                            }
                        }
                    }
                };
            let mut at_expr = None;
            let mut inv = None;
            for _ in 0..2 {
                if at_expr.is_none() && self.peek_is_contextual_kw("at") {
                    self.advance();
                    at_expr = Some(self.parse_expr()?);
                } else if inv.is_none() && self.peek_is_contextual_kw("inv") {
                    inv = self.parse_opt_inv()?;
                } else {
                    break;
                }
            }
            fields.push(RawLayoutField {
                name: fname,
                struct_module,
                struct_type,
                len_expr,
                is_array,
                is_vec: false,
                is_fixed,
                at_expr,
                inv,
                span: fspan,
            });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else {
                break;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close layout body")?;
        Ok(RawLayout {
            name,
            is_pub: false,
            base_expr,
            fields,
            span: tok.span,
        })
    }

    fn parse_fn_with_export(&mut self, exported_already: bool) -> Result<RawFnDef, InfimumDiagnostic> {
        let exported = if exported_already {
            true
        } else if matches!(self.peek().kind, TokenKind::Export) {
            self.advance();
            true
        } else {
            false
        };

        self.expect(
            &TokenKind::Fn,
            "Start each Infimum node definition with 'fn <name>(...) -> <type> { ... }' or 'export fn ...'",
        )?;
        let (name, name_span) = self.expect_ident("Provide a function alias name after 'fn'")?;
        self.expect(&TokenKind::LParen, "Expected '(' to start parameter list")?;

        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let (pname, _) = self.expect_ident("Expected parameter name")?;
                self.expect(&TokenKind::Colon, "Expected ':' after parameter name")?;
                let pty = self.parse_raw_type()?;
                params.push((pname, pty));
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' after parameter list")?;

        let mut effects = Vec::new();
        match &self.peek().kind {
            TokenKind::Arrow => {
                self.advance();
            }
            TokenKind::EffectOpen => {
                self.advance();
                if !matches!(self.peek().kind, TokenKind::EffectClose) {
                    loop {
                        let (ename, _) =
                            self.expect_ident("Expected effect capability name inside '-[...]->'")?;
                        let scope = if matches!(self.peek().kind, TokenKind::LParen) {
                            self.advance();
                            let s = match self.peek().kind.clone() {
                                TokenKind::Str(val) => {
                                    self.advance();
                                    val
                                }
                                other => {
                                    return Err(InfimumDiagnostic::new(
                                        "E012_EXPECTED_SCOPE_STR",
                                        "parse",
                                        format!("Expected string literal scope, found {other:?}"),
                                        self.peek().span,
                                        "Write capability scope as a string literal, e.g. Fs(\"/tmp\")",
                                    ));
                                }
                            };
                            self.expect(
                                &TokenKind::RParen,
                                "Expected ')' after capability scope literal",
                            )?;
                            Some(s)
                        } else {
                            None
                        };
                        effects.push(EffectCap { name: ename, scope });
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(
                    &TokenKind::EffectClose,
                    "Close capability list with ']->' before return type",
                )?;
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E013_EXPECTED_RETURN_ARROW",
                    "parse",
                    format!("Expected '->' or '-[...]->', found {other:?}"),
                    self.peek().span,
                    "Specify return type with '-> <Type>' or effects + return type with '-[Mem]-> <Type>'",
                ));
            }
        }

        let ret_type = self.parse_raw_type()?;

        let mut requires = Vec::new();
        let mut ensures = Vec::new();
        loop {
            match self.peek().kind {
                TokenKind::Req => {
                    self.advance();
                    requires.push(self.parse_expr()?);
                }
                TokenKind::Ens => {
                    self.advance();
                    ensures.push(self.parse_expr()?);
                }
                _ => break,
            }
        }

        let body = self.parse_block_expr()?;

        Ok(RawFnDef {
            name,
            exported,
            is_pub: false,
            test_label: None,
            params,
            effects,
            ret_type,
            requires,
            ensures,
            body,
            source_file: None,
            span: name_span,
        })
    }

    fn parse_test_fn(&mut self) -> Result<RawFnDef, InfimumDiagnostic> {
        let (_, span) = self.expect_ident("Expected 'test'")?;
        let tok = self.peek().clone();
        let label = match tok.kind {
            TokenKind::Str(s) => {
                self.advance();
                s
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E010_UNEXPECTED_TOKEN",
                    "parse",
                    format!("Expected test name string literal after 'test', found {other:?}"),
                    tok.span,
                    "Write: test \"my test name\" { assert(1 + 1 == 2); }",
                ));
            }
        };
        let raw_stmts = self.parse_stmt_list_block()?;
        let mut stmts = Vec::with_capacity(raw_stmts.len() + 1);
        stmts.push(RawStmt::Let {
            name: "__ifm_test_fail".to_string(),
            mutable: true,
            ty: Some(RawType::Builtin(Type::I64)),
            value: RawExpr::IntLit(0),
        });
        for st in raw_stmts {
            stmts.push(rewrite_assert_stmt(st, span.line as i64));
        }
        let body = RawExpr::Block {
            stmts,
            tail: Box::new(RawExpr::Var("__ifm_test_fail".to_string())),
        };
        Ok(RawFnDef {
            name: format!("__ifm_test_L{}_C{}", span.line, span.col),
            exported: false,
            is_pub: false,
            test_label: Some(label),
            params: vec![],
            effects: vec![EffectCap {
                name: "Mem".to_string(),
                scope: None,
            }],
            ret_type: RawType::Builtin(Type::I64),
            requires: vec![],
            ensures: vec![],
            body,
            source_file: None,
            span,
        })
    }

    fn parse_raw_type(&mut self) -> Result<RawType, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::LBracket) {
            self.advance(); // '['
            let elem = if self.peek_is_contextual_kw("word") {
                self.advance();
                RawType::Builtin(Type::I64)
            } else {
                self.parse_raw_type()?
            };
            self.expect(&TokenKind::RBracket, "Expected ']' to close slice type '[T]'")?;
            return Ok(RawType::Slice(Box::new(elem)));
        }
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::RParen) {
                self.advance();
                return Ok(RawType::Builtin(Type::Unit));
            }
            let first = self.parse_raw_type()?;
            if matches!(self.peek().kind, TokenKind::Comma) {
                let mut elems = vec![first];
                while matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    elems.push(self.parse_raw_type()?);
                }
                self.expect(&TokenKind::RParen, "Expected ')' to close tuple type")?;
                return Ok(RawType::Tuple(elems));
            }
            self.expect(&TokenKind::RParen, "Expected ')' after parenthesized type")?;
            return Ok(first);
        }
        let (tname, span) = self.expect_ident(
            "Expected type name (i64, fixed, bool, str, unit, StructName, Secret<T>, (T1, T2))",
        )?;
        match tname.as_str() {
            "i64" => Ok(RawType::Builtin(Type::I64)),
            "fixed" => Ok(RawType::Builtin(Type::Fixed)),
            "bool" => Ok(RawType::Builtin(Type::Bool)),
            "str" | "String" => Ok(RawType::Builtin(Type::Str)),
            "unit" | "Unit" => Ok(RawType::Builtin(Type::Unit)),
            "Secret" => {
                self.expect(&TokenKind::Lt, "Expected '<' after Secret")?;
                let inner = self.parse_raw_type()?;
                self.expect(&TokenKind::Gt, "Expected '>' to close Secret<T>")?;
                Ok(RawType::Secret(Box::new(inner)))
            }
            _ => {
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    self.advance();
                    let (sname, _) = self.expect_ident("Expected type name after '::'")?;
                    Ok(RawType::Named {
                        module: Some(tname),
                        name: sname,
                        span,
                    })
                } else {
                    Ok(RawType::Named {
                        module: None,
                        name: tname,
                        span,
                    })
                }
            }
        }
    }

    fn parse_type(&mut self) -> Result<Type, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::RParen) {
                self.advance();
                return Ok(Type::Unit);
            }
            let first = self.parse_type()?;
            if matches!(self.peek().kind, TokenKind::Comma) {
                let mut elems = vec![first];
                while matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                    elems.push(self.parse_type()?);
                }
                self.expect(&TokenKind::RParen, "Expected ')' to close tuple type")?;
                return Ok(Type::Tuple(elems));
            }
            self.expect(&TokenKind::RParen, "Expected ')' after parenthesized type")?;
            return Ok(first);
        }
        let (tname, span) =
            self.expect_ident("Expected type name (i64, bool, str, unit, Secret<T>, (T1, T2))")?;
        match tname.as_str() {
            "i64" => Ok(Type::I64),
            "fixed" => Ok(Type::Fixed),
            "bool" => Ok(Type::Bool),
            "str" | "String" => Ok(Type::Str),
            "unit" | "Unit" => Ok(Type::Unit),
            "Secret" => {
                self.expect(&TokenKind::Lt, "Expected '<' after Secret")?;
                let inner = self.parse_type()?;
                self.expect(&TokenKind::Gt, "Expected '>' to close Secret<T>")?;
                Ok(Type::Secret(Box::new(inner)))
            }
            other => Err(InfimumDiagnostic::new(
                "E014_UNKNOWN_TYPE",
                "parse",
                format!("Unknown type '{other}'"),
                span,
                "Use one of the built-in Infimum types: i64, bool, str, unit, or Secret<T>",
            )),
        }
    }

    fn assign_op_for(kind: &TokenKind) -> Option<Option<BinOp>> {
        match kind {
            TokenKind::Assign => Some(None),
            TokenKind::PlusEq => Some(Some(BinOp::Add)),
            TokenKind::MinusEq => Some(Some(BinOp::Sub)),
            TokenKind::StarEq => Some(Some(BinOp::Mul)),
            TokenKind::PipeEq => Some(Some(BinOp::BitOr)),
            TokenKind::AmpEq => Some(Some(BinOp::BitAnd)),
            _ => None,
        }
    }

    fn try_parse_assign_stmt(&mut self) -> Result<Option<RawStmt>, InfimumDiagnostic> {
        // Variable assignment: `x = e;` or `x += e;` / `-=` / `*=` / `|=` / `&=`
        if matches!(&self.peek().kind, TokenKind::Ident(_)) {
            if let Some(op) = Self::assign_op_for(self.peek_next()) {
                let (vname, _) = self.expect_ident("Expected variable name")?;
                self.advance(); // consume assignment operator
                let val = self.parse_expr()?;
                self.expect(&TokenKind::Semi, "Terminate assignment statement with ';'")?;
                return Ok(Some(RawStmt::Assign {
                    name: vname,
                    op,
                    value: val,
                }));
            }
            // Slice element assignment: `s[i] = val;`, `s[i] += val;`, `s[i].f = val;`, `s[i].f += val;`
            if matches!(self.peek_at(1), TokenKind::LBracket) {
                let saved = self.idx;
                let (vname, span) = self.expect_ident("Expected slice variable name")?;
                self.advance(); // '['
                if !matches!(self.peek().kind, TokenKind::DotDot | TokenKind::RBracket) {
                    let idx_expr = self.parse_expr()?;
                    if matches!(self.peek().kind, TokenKind::RBracket) {
                        self.advance(); // ']'
                        let subfield = if matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_at(1), TokenKind::Ident(_))
                        {
                            self.advance();
                            let (sf, _) = self.expect_ident("Expected struct field name")?;
                            Some(sf)
                        } else {
                            None
                        };
                        if let Some(op) = Self::assign_op_for(&self.peek().kind) {
                            self.advance();
                            let val = self.parse_expr()?;
                            self.expect(
                                &TokenKind::Semi,
                                "Terminate slice assignment statement with ';'",
                            )?;
                            return Ok(Some(RawStmt::SliceStore(Box::new(RawSliceStore {
                                base: RawExpr::Var(vname),
                                index: idx_expr,
                                subfield,
                                op,
                                value: val,
                                span,
                            }))));
                        }
                    }
                }
                self.idx = saved;
            }
            // Layout assignment: `[mod::]Layout.field`, `...field[i]`, `...field.sub`, `...field[i].sub`
            let is_direct_layout = matches!(self.peek_at(1), TokenKind::Dot)
                && matches!(self.peek_at(2), TokenKind::Ident(_));
            let is_mod_layout = matches!(self.peek_at(1), TokenKind::ColonColon)
                && matches!(self.peek_at(2), TokenKind::Ident(_))
                && matches!(self.peek_at(3), TokenKind::Dot)
                && matches!(self.peek_at(4), TokenKind::Ident(_));
            if is_direct_layout || is_mod_layout {
                let saved = self.idx;
                let (module, layout, span) = if is_mod_layout {
                    let (m, sp) = self.expect_ident("Expected module name")?;
                    self.advance(); // '::'
                    let (l, _) = self.expect_ident("Expected layout name")?;
                    (Some(m), l, sp)
                } else {
                    let (l, sp) = self.expect_ident("Expected layout name")?;
                    (None, l, sp)
                };
                self.advance(); // '.'
                let (field, _) = self.expect_ident("Expected layout field name")?;
                let index = if matches!(self.peek().kind, TokenKind::LBracket) {
                    self.advance();
                    let idx_expr = self.parse_expr()?;
                    if !matches!(self.peek().kind, TokenKind::RBracket) {
                        self.idx = saved;
                        return Ok(None);
                    }
                    self.advance();
                    Some(idx_expr)
                } else {
                    None
                };
                let subfield = if matches!(self.peek().kind, TokenKind::Dot)
                    && matches!(self.peek_at(1), TokenKind::Ident(_))
                {
                    self.advance();
                    let (sf, _) = self.expect_ident("Expected struct field name")?;
                    Some(sf)
                } else {
                    None
                };
                if let Some(op) = Self::assign_op_for(&self.peek().kind) {
                    self.advance(); // assignment op
                    let val = self.parse_expr()?;
                    self.expect(
                        &TokenKind::Semi,
                        "Terminate layout field assignment with ';'",
                    )?;
                    return Ok(Some(RawStmt::LayoutStore {
                        module,
                        layout,
                        field,
                        index,
                        subfield,
                        op,
                        value: val,
                        span,
                    }));
                }
                self.idx = saved;
            }
        }
        Ok(None)
    }

    fn parse_stmt_list_block(&mut self) -> Result<Vec<RawStmt>, InfimumDiagnostic> {
        self.expect(&TokenKind::LBrace, "Expected '{' to start loop body")?;
        let mut stmts = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            stmts.push(self.parse_single_stmt_in_loop()?);
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close loop body")?;
        Ok(stmts)
    }

    fn parse_single_stmt_in_loop(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        if matches!(self.peek().kind, TokenKind::Let) {
            return self.parse_let_stmt();
        }
        if matches!(self.peek().kind, TokenKind::While) {
            return self.parse_while_stmt();
        }
        if matches!(self.peek().kind, TokenKind::For) {
            return self.parse_for_stmt();
        }
        if matches!(self.peek().kind, TokenKind::Break) {
            let tok = self.advance();
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
            }
            return Ok(RawStmt::Break(tok.span));
        }
        if matches!(self.peek().kind, TokenKind::Continue) {
            let tok = self.advance();
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
            }
            return Ok(RawStmt::Continue(tok.span));
        }
        if let Some(assign) = self.try_parse_assign_stmt()? {
            return Ok(assign);
        }
        let expr = self.parse_expr()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        } else if !matches!(
            expr,
            RawExpr::If { .. } | RawExpr::Match { .. } | RawExpr::Block { .. }
        ) {
            self.expect(&TokenKind::Semi, "Terminate statement inside loop with ';'")?;
        }
        Ok(RawStmt::Expr(expr))
    }

    fn parse_let_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        self.expect(&TokenKind::Let, "Expected 'let'")?;
        if matches!(self.peek().kind, TokenKind::LParen) {
            self.advance();
            let mut names = Vec::new();
            while !matches!(self.peek().kind, TokenKind::RParen | TokenKind::Eof) {
                let is_mut = if matches!(self.peek().kind, TokenKind::Mut) {
                    self.advance();
                    true
                } else {
                    false
                };
                let (vname, _) = self.expect_ident("Expected variable name in tuple pattern")?;
                names.push((vname, is_mut));
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                } else {
                    break;
                }
            }
            self.expect(&TokenKind::RParen, "Expected ')' to close tuple pattern")?;
            self.expect(&TokenKind::Assign, "Expected '=' in tuple let binding")?;
            let val = self.parse_expr()?;
            self.expect(&TokenKind::Semi, "Terminate tuple 'let' binding with ';'")?;
            return Ok(RawStmt::LetTuple { names, value: val });
        }
        let mutable = if matches!(self.peek().kind, TokenKind::Mut) {
            self.advance();
            true
        } else {
            false
        };
        let (vname, _) = self.expect_ident("Expected variable name after 'let'")?;
        let ty = if matches!(self.peek().kind, TokenKind::Colon) {
            self.advance();
            Some(self.parse_raw_type()?)
        } else {
            None
        };
        self.expect(&TokenKind::Assign, "Expected '=' in let binding")?;
        let val = self.parse_expr()?;
        self.expect(&TokenKind::Semi, "Terminate 'let' binding with ';'")?;
        Ok(RawStmt::Let {
            name: vname,
            mutable,
            ty,
            value: val,
        })
    }

    fn parse_const_atom(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawExpr::IntLit(v))
            }
            TokenKind::Fixed(v) => {
                self.advance();
                Ok(RawExpr::FixedLit(v))
            }
            TokenKind::LParen => {
                self.advance();
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after constant expression")?;
                Ok(inner)
            }
            TokenKind::Ident(name) => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected identifier after '::'")?;
                        segs.push(seg);
                    }
                    if segs.len() == 2 && matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall(Box::new(RawModCall {
                            module: segs.remove(0),
                            func: segs.remove(0),
                            args,
                            span: tok.span,
                        }));
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                        {
                            let dot_span = self.advance().span;
                            match self.advance().kind {
                                TokenKind::Int(idx) => {
                                    expr = RawExpr::TupleField {
                                        expr: Box::new(expr),
                                        index: idx.max(0) as usize,
                                    };
                                }
                                TokenKind::Ident(field) => {
                                    expr = RawExpr::FieldAccess {
                                        expr: Box::new(expr),
                                        field,
                                        span: dot_span,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Ok(expr)
                    } else {
                        Ok(RawExpr::Path {
                            segs,
                            span: tok.span,
                        })
                    }
                } else {
                    Ok(RawExpr::Var(name))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E017_INVALID_LOOP_BOUND",
                "parse",
                format!("Expected positive integer constant after 'max', found {other:?}"),
                tok.span,
                "Provide a positive compile-time iteration bound (e.g. 'while i < n max 4096 { ... }' or 'max MAX_ITERS')",
            )),
        }
    }

    fn parse_while_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        let w_tok = self.expect(&TokenKind::While, "Expected 'while'")?;
        let cond = self.parse_expr()?;
        self.expect(
            &TokenKind::Max,
            "Every 'while' loop in Infimum.Strata must declare an explicit upper bound: 'while <cond> max <N> { ... }'",
        )?;
        let max_iters = self.parse_const_atom()?;
        let body = self.parse_stmt_list_block()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        }
        Ok(RawStmt::While {
            cond,
            max_iters,
            body,
            span: w_tok.span,
        })
    }

    fn parse_for_stmt(&mut self) -> Result<RawStmt, InfimumDiagnostic> {
        let f_tok = self.expect(&TokenKind::For, "Expected 'for'")?;
        let (var, _) = self.expect_ident("Expected loop variable name after 'for'")?;
        if !self.peek_is_contextual_kw("in") {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected 'in' after 'for {var}', found {:?}", self.peek().kind),
                self.peek().span,
                "Write 'for i in 0..N { ... }' or 'for i in 0..=N { ... }'",
            ));
        }
        self.advance();
        let start = self.parse_expr()?;
        let inclusive = if matches!(self.peek().kind, TokenKind::DotDotEq) {
            self.advance();
            true
        } else if matches!(self.peek().kind, TokenKind::DotDot) {
            self.advance();
            false
        } else {
            return Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Expected '..' or '..=' in 'for' range, found {:?}", self.peek().kind),
                self.peek().span,
                "Write 'for i in start..end { ... }' or 'for i in start..=end { ... }'",
            ));
        };
        let end = self.parse_expr()?;
        let max_iters = if matches!(self.peek().kind, TokenKind::Max) {
            self.advance();
            Some(self.parse_const_atom()?)
        } else {
            None
        };
        let body = self.parse_stmt_list_block()?;
        if matches!(self.peek().kind, TokenKind::Semi) {
            self.advance();
        }
        Ok(RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body,
            span: f_tok.span,
        })
    }

    fn parse_block_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.expect(&TokenKind::LBrace, "Expected '{' to open block")?;
        let mut stmts = Vec::new();

        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            if matches!(self.peek().kind, TokenKind::Let) {
                stmts.push(self.parse_let_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::While) {
                stmts.push(self.parse_while_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::For) {
                stmts.push(self.parse_for_stmt()?);
                continue;
            }
            if matches!(self.peek().kind, TokenKind::Break) {
                let tok = self.advance();
                if matches!(self.peek().kind, TokenKind::Semi) {
                    self.advance();
                }
                stmts.push(RawStmt::Break(tok.span));
                continue;
            }
            if matches!(self.peek().kind, TokenKind::Continue) {
                let tok = self.advance();
                if matches!(self.peek().kind, TokenKind::Semi) {
                    self.advance();
                }
                stmts.push(RawStmt::Continue(tok.span));
                continue;
            }
            if let Some(assign) = self.try_parse_assign_stmt()? {
                stmts.push(assign);
                continue;
            }

            let expr = self.parse_expr()?;
            if matches!(self.peek().kind, TokenKind::Semi) {
                self.advance();
                if matches!(self.peek().kind, TokenKind::RBrace) {
                    self.advance();
                    stmts.push(RawStmt::Expr(expr));
                    return Ok(RawExpr::Block {
                        stmts,
                        tail: Box::new(RawExpr::UnitLit),
                    });
                }
                stmts.push(RawStmt::Expr(expr));
            } else if matches!(
                expr,
                RawExpr::If { .. } | RawExpr::Match { .. } | RawExpr::Block { .. }
            ) && !matches!(self.peek().kind, TokenKind::RBrace)
            {
                // Allow `if ... { ... }`, `match ... { ... }`, and `{ ... }` statements without ';' before next statement
                stmts.push(RawStmt::Expr(expr));
            } else {
                self.expect(
                    &TokenKind::RBrace,
                    "Expected '}' after block tail expression (or ';' before next statement)",
                )?;
                if stmts.is_empty() {
                    return Ok(expr);
                }
                return Ok(RawExpr::Block {
                    stmts,
                    tail: Box::new(expr),
                });
            }
        }

        self.expect(&TokenKind::RBrace, "Expected '}' to close block")?;
        Ok(RawExpr::Block {
            stmts,
            tail: Box::new(RawExpr::UnitLit),
        })
    }

    pub(super) fn parse_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.parse_or()
    }

    fn parse_or(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_and()?;
        while matches!(self.peek().kind, TokenKind::OrOr) {
            self.advance();
            let rhs = self.parse_and()?;
            lhs = RawExpr::Binary {
                op: BinOp::Or,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_and(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitor()?;
        while matches!(self.peek().kind, TokenKind::AndAnd) {
            self.advance();
            let rhs = self.parse_bitor()?;
            lhs = RawExpr::Binary {
                op: BinOp::And,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitor(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitxor()?;
        while matches!(self.peek().kind, TokenKind::Pipe) {
            self.advance();
            let rhs = self.parse_bitxor()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitOr,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitxor(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_bitand()?;
        while matches!(self.peek().kind, TokenKind::Caret) {
            self.advance();
            let rhs = self.parse_bitand()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitXor,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_bitand(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_eq()?;
        while matches!(self.peek().kind, TokenKind::Amp) {
            self.advance();
            let rhs = self.parse_eq()?;
            lhs = RawExpr::Binary {
                op: BinOp::BitAnd,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_eq(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_rel()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::EqEq => BinOp::Eq,
                TokenKind::NotEq => BinOp::Ne,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_rel()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_rel(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_shift()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Lt => BinOp::Lt,
                TokenKind::Le => BinOp::Le,
                TokenKind::Gt => BinOp::Gt,
                TokenKind::Ge => BinOp::Ge,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_shift()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_shift(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_add()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Shl => BinOp::Shl,
                TokenKind::Shr => BinOp::Shr,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_add()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_add(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_mul()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Plus => BinOp::Add,
                TokenKind::Minus => BinOp::Sub,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_mul()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_mul(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mut lhs = self.parse_unary()?;
        loop {
            let op = match self.peek().kind {
                TokenKind::Star => BinOp::Mul,
                TokenKind::Slash => BinOp::Div,
                TokenKind::Percent => BinOp::Mod,
                _ => break,
            };
            self.advance();
            let rhs = self.parse_unary()?;
            lhs = RawExpr::Binary {
                op,
                lhs: Box::new(lhs),
                rhs: Box::new(rhs),
            };
        }
        Ok(lhs)
    }

    fn parse_unary(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        match self.peek().kind {
            TokenKind::Minus => {
                self.advance();
                let inner = self.parse_unary()?;
                Ok(RawExpr::Unary {
                    op: UnaryOp::Neg,
                    expr: Box::new(inner),
                })
            }
            TokenKind::Bang => {
                self.advance();
                let inner = self.parse_unary()?;
                Ok(RawExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(inner),
                })
            }
            _ => self.parse_primary(),
        }
    }

    fn parse_if_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        self.expect(&TokenKind::If, "Expected 'if'")?;
        let cond = self.parse_expr()?;
        let then_branch = self.parse_block_expr()?;
        let else_branch = if matches!(self.peek().kind, TokenKind::Else) {
            self.advance();
            if matches!(self.peek().kind, TokenKind::If) {
                self.parse_if_expr()?
            } else {
                self.parse_block_expr()?
            }
        } else {
            RawExpr::UnitLit
        };
        Ok(RawExpr::If {
            cond: Box::new(cond),
            then_branch: Box::new(then_branch),
            else_branch: Box::new(else_branch),
        })
    }

    fn parse_match_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let mtok = self.expect(&TokenKind::Match, "Expected 'match'")?;
        let scrutinee = self.parse_expr()?;
        self.expect(&TokenKind::LBrace, "Expected '{' to open match arms")?;

        let mut arms = Vec::new();
        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
            let mut patterns = vec![self.parse_match_pattern()?];
            while matches!(self.peek().kind, TokenKind::Pipe) {
                self.advance();
                patterns.push(self.parse_match_pattern()?);
            }
            self.expect(&TokenKind::FatArrow, "Expected '=>' after match pattern")?;
            let body = self.parse_expr()?;
            let is_block_like = matches!(
                body,
                RawExpr::Block { .. } | RawExpr::If { .. } | RawExpr::Match { .. }
            );
            arms.push(RawMatchArm { patterns, body });
            if matches!(self.peek().kind, TokenKind::Comma) {
                self.advance();
            } else if !is_block_like && !matches!(self.peek().kind, TokenKind::RBrace) {
                self.expect(&TokenKind::Comma, "Expected ',' after match arm")?;
            }
        }
        self.expect(&TokenKind::RBrace, "Expected '}' to close match expression")?;
        Ok(RawExpr::Match {
            scrutinee: Box::new(scrutinee),
            arms,
            span: mtok.span,
        })
    }

    fn parse_match_pattern(&mut self) -> Result<RawMatchPattern, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawMatchPattern::Int(v))
            }
            TokenKind::Minus => {
                self.advance();
                let next = self.peek().clone();
                if let TokenKind::Int(v) = next.kind {
                    self.advance();
                    Ok(RawMatchPattern::Int(v.wrapping_neg()))
                } else {
                    Err(InfimumDiagnostic::new(
                        "E010_UNEXPECTED_TOKEN",
                        "parse",
                        format!("Expected integer literal after '-' in match pattern, found {:?}", next.kind),
                        next.span,
                        "Use an integer literal (e.g. -1) in match patterns",
                    ))
                }
            }
            TokenKind::True => {
                self.advance();
                Ok(RawMatchPattern::Bool(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(RawMatchPattern::Bool(false))
            }
            TokenKind::Ident(name) => {
                self.advance();
                if name == "_" {
                    return Ok(RawMatchPattern::Wildcard(tok.span));
                }
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected variant or member name after '::'")?;
                        segs.push(seg);
                    }
                    Ok(RawMatchPattern::Path(segs, tok.span))
                } else {
                    Ok(RawMatchPattern::ConstRef(name, tok.span))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E010_UNEXPECTED_TOKEN",
                "parse",
                format!("Invalid match pattern starting with {other:?}"),
                tok.span,
                "Use an integer/char/bool literal, const name, Enum::Variant, or '_' wildcard",
            )),
        }
    }

    fn parse_primary(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        let tok = self.peek().clone();
        match tok.kind {
            TokenKind::Int(v) => {
                self.advance();
                Ok(RawExpr::IntLit(v))
            }
            TokenKind::Fixed(v) => {
                self.advance();
                Ok(RawExpr::FixedLit(v))
            }
            TokenKind::True => {
                self.advance();
                Ok(RawExpr::BoolLit(true))
            }
            TokenKind::False => {
                self.advance();
                Ok(RawExpr::BoolLit(false))
            }
            TokenKind::Str(s) => {
                self.advance();
                Ok(RawExpr::StrLit(s))
            }
            TokenKind::LBrace => self.parse_block_expr(),
            TokenKind::LParen => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::RParen) {
                    self.advance();
                    return Ok(RawExpr::UnitLit);
                }
                let first = self.parse_expr()?;
                let mut expr = if matches!(self.peek().kind, TokenKind::Comma) {
                    let mut elems = vec![first];
                    while matches!(self.peek().kind, TokenKind::Comma) {
                        self.advance();
                        if matches!(self.peek().kind, TokenKind::RParen) {
                            break;
                        }
                        elems.push(self.parse_expr()?);
                    }
                    self.expect(&TokenKind::RParen, "Expected ')' to close tuple literal")?;
                    RawExpr::TupleLit(elems)
                } else {
                    self.expect(&TokenKind::RParen, "Expected ')' after parenthesized expression")?;
                    first
                };
                while matches!(self.peek().kind, TokenKind::Dot)
                    && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                {
                    let dot_span = self.advance().span;
                    match self.advance().kind {
                        TokenKind::Int(idx) => {
                            expr = RawExpr::TupleField {
                                expr: Box::new(expr),
                                index: idx.max(0) as usize,
                            };
                        }
                        TokenKind::Ident(field) => {
                            expr = RawExpr::FieldAccess {
                                expr: Box::new(expr),
                                field,
                                span: dot_span,
                            };
                        }
                        _ => {}
                    }
                }
                Ok(expr)
            }
            TokenKind::If => self.parse_if_expr(),
            TokenKind::Match => self.parse_match_expr(),
            TokenKind::Do => {
                self.advance();
                let (effect, _) = self.expect_ident(
                    "Expected capability name after 'do' (e.g. do Net.post(...))",
                )?;
                self.expect(&TokenKind::Dot, "Expected '.' between capability and operation")?;
                let (op, _) = self.expect_ident("Expected effect operation name after '.'")?;
                let args = self.parse_arg_list()?;
                Ok(RawExpr::Perform { effect, op, args })
            }
            TokenKind::MemLoadKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'mem_load'")?;
                let index = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after mem_load(index)")?;
                Ok(RawExpr::MemLoad {
                    index: Box::new(index),
                })
            }
            TokenKind::MemStoreKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'mem_store'")?;
                let index = self.parse_expr()?;
                self.expect(&TokenKind::Comma, "Expected ',' between mem_store index and value")?;
                let value = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after mem_store(index, value)")?;
                Ok(RawExpr::MemStore {
                    index: Box::new(index),
                    value: Box::new(value),
                })
            }
            TokenKind::SecretKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'secret'")?;
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after secret(expr)")?;
                Ok(RawExpr::SecretWrap(Box::new(inner)))
            }
            TokenKind::RevealKw => {
                self.advance();
                self.expect(&TokenKind::LParen, "Expected '(' after 'reveal'")?;
                let domain = match self.peek().kind.clone() {
                    TokenKind::Str(s) => {
                        self.advance();
                        s
                    }
                    other => {
                        return Err(InfimumDiagnostic::new(
                            "E015_EXPECTED_DOMAIN_STR",
                            "parse",
                            format!("Expected destination domain string literal, found {other:?}"),
                            self.peek().span,
                            "Pass explicit destination domain literal: reveal(\"mybank.com\", secret_val)",
                        ));
                    }
                };
                self.expect(&TokenKind::Comma, "Expected ',' after reveal domain")?;
                let inner = self.parse_expr()?;
                self.expect(&TokenKind::RParen, "Expected ')' after reveal arguments")?;
                Ok(RawExpr::Reveal {
                    domain,
                    expr: Box::new(inner),
                })
            }
            TokenKind::HashRef(hash) => {
                self.advance();
                let args = self.parse_arg_list()?;
                Ok(RawExpr::Call {
                    target: TargetRef::Hash(hash),
                    args,
                    span: tok.span,
                })
            }
            TokenKind::Ident(name) => {
                self.advance();
                if matches!(self.peek().kind, TokenKind::ColonColon) {
                    let mut segs = vec![name];
                    while matches!(self.peek().kind, TokenKind::ColonColon) {
                        self.advance();
                        let (seg, _) = self.expect_ident("Expected member name after '::'")?;
                        segs.push(seg);
                    }
                    if segs.len() == 2 && matches!(self.peek().kind, TokenKind::LParen) {
                        let args = self.parse_arg_list()?;
                        let mut expr = RawExpr::ModCall(Box::new(RawModCall {
                            module: segs.remove(0),
                            func: segs.remove(0),
                            args,
                            span: tok.span,
                        }));
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                        {
                            let dot_span = self.advance().span;
                            match self.advance().kind {
                                TokenKind::Int(idx) => {
                                    expr = RawExpr::TupleField {
                                        expr: Box::new(expr),
                                        index: idx.max(0) as usize,
                                    };
                                }
                                TokenKind::Ident(field) => {
                                    expr = RawExpr::FieldAccess {
                                        expr: Box::new(expr),
                                        field,
                                        span: dot_span,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Ok(expr)
                    } else if segs.len() == 2
                        && matches!(self.peek().kind, TokenKind::LBrace)
                        && matches!(self.peek_at(1), TokenKind::Ident(_))
                        && matches!(self.peek_at(2), TokenKind::Colon)
                    {
                        self.advance(); // '{'
                        let mut fields = Vec::new();
                        while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                            let (fname, fspan) = self.expect_ident("Expected struct field name")?;
                            self.expect(&TokenKind::Colon, "Expected ':' after struct field name")?;
                            let fval = self.parse_expr()?;
                            fields.push((fname, fval, fspan));
                            if matches!(self.peek().kind, TokenKind::Comma) {
                                self.advance();
                            } else {
                                break;
                            }
                        }
                        self.expect(&TokenKind::RBrace, "Expected '}' to close struct literal")?;
                        let mut expr = RawExpr::StructLit(Box::new(RawStructLit {
                            module: Some(segs.remove(0)),
                            name: segs.remove(0),
                            fields,
                            span: tok.span,
                        }));
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                        {
                            let dot_span = self.advance().span;
                            match self.advance().kind {
                                TokenKind::Int(idx) => {
                                    expr = RawExpr::TupleField {
                                        expr: Box::new(expr),
                                        index: idx.max(0) as usize,
                                    };
                                }
                                TokenKind::Ident(field) => {
                                    expr = RawExpr::FieldAccess {
                                        expr: Box::new(expr),
                                        field,
                                        span: dot_span,
                                    };
                                }
                                _ => {}
                            }
                        }
                        Ok(expr)
                    } else if segs.len() == 2
                        && matches!(self.peek().kind, TokenKind::Dot)
                        && matches!(self.peek_next(), TokenKind::Ident(_))
                    {
                        self.advance(); // '.'
                        let (field, _) = self.expect_ident("Expected layout field name after '.'")?;
                        let base = RawExpr::LayoutLoad(Box::new(RawLayoutLoad {
                            module: Some(segs.remove(0)),
                            layout: segs.remove(0),
                            field,
                            index: None,
                            subfield: None,
                            span: tok.span,
                        }));
                        self.parse_postfix_chain(base, tok.span)
                    } else {
                        Ok(RawExpr::Path {
                            segs,
                            span: tok.span,
                        })
                    }
                } else if matches!(self.peek().kind, TokenKind::Dot) {
                    if matches!(self.peek_next(), TokenKind::Int(_)) {
                        let mut expr = RawExpr::Var(name);
                        while matches!(self.peek().kind, TokenKind::Dot)
                            && matches!(self.peek_next(), TokenKind::Int(_))
                        {
                            self.advance();
                            if let TokenKind::Int(idx) = self.advance().kind {
                                expr = RawExpr::TupleField {
                                    expr: Box::new(expr),
                                    index: idx.max(0) as usize,
                                };
                            }
                        }
                        return Ok(expr);
                    }
                    self.advance();
                    let (field, _) = self.expect_ident("Expected layout field name after '.'")?;
                    let base = RawExpr::LayoutLoad(Box::new(RawLayoutLoad {
                        module: None,
                        layout: name,
                        field,
                        index: None,
                        subfield: None,
                        span: tok.span,
                    }));
                    self.parse_postfix_chain(base, tok.span)
                } else if matches!(self.peek().kind, TokenKind::LBracket) {
                    self.parse_postfix_chain(RawExpr::Var(name), tok.span)
                } else if matches!(self.peek().kind, TokenKind::LParen) {
                    let args = self.parse_arg_list()?;
                    let target = if name == "$self" {
                        TargetRef::SelfRec
                    } else {
                        TargetRef::Named(name)
                    };
                    let expr = RawExpr::Call {
                        target,
                        args,
                        span: tok.span,
                    };
                    self.parse_postfix_chain(expr, tok.span)
                } else if matches!(self.peek().kind, TokenKind::LBrace)
                    && matches!(self.peek_at(1), TokenKind::Ident(_))
                    && matches!(self.peek_at(2), TokenKind::Colon)
                {
                    self.advance(); // '{'
                    let mut fields = Vec::new();
                    while !matches!(self.peek().kind, TokenKind::RBrace | TokenKind::Eof) {
                        let (fname, fspan) = self.expect_ident("Expected struct field name")?;
                        self.expect(&TokenKind::Colon, "Expected ':' after struct field name")?;
                        let fval = self.parse_expr()?;
                        fields.push((fname, fval, fspan));
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                    self.expect(&TokenKind::RBrace, "Expected '}' to close struct literal")?;
                    let mut expr = RawExpr::StructLit(Box::new(RawStructLit {
                        module: None,
                        name,
                        fields,
                        span: tok.span,
                    }));
                    while matches!(self.peek().kind, TokenKind::Dot)
                        && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
                    {
                        let dot_span = self.advance().span;
                        match self.advance().kind {
                            TokenKind::Int(idx) => {
                                expr = RawExpr::TupleField {
                                    expr: Box::new(expr),
                                    index: idx.max(0) as usize,
                                };
                            }
                            TokenKind::Ident(field) => {
                                expr = RawExpr::FieldAccess {
                                    expr: Box::new(expr),
                                    field,
                                    span: dot_span,
                                };
                            }
                            _ => {}
                        }
                    }
                    Ok(expr)
                } else {
                    Ok(RawExpr::Var(name))
                }
            }
            other => Err(InfimumDiagnostic::new(
                "E016_EXPECTED_EXPR",
                "parse",
                format!("Expected expression, found {other:?}"),
                tok.span,
                "Provide a valid expression (literal, variable, call, block, if/else, match, mem_load/mem_store, or 'do Effect.op(...)')",
            )),
        }
    }

    fn parse_postfix_chain(
        &mut self,
        mut expr: RawExpr,
        origin_span: Span,
    ) -> Result<RawExpr, InfimumDiagnostic> {
        loop {
            if matches!(self.peek().kind, TokenKind::LBracket) {
                let lspan = self.advance().span; // '['
                if matches!(self.peek().kind, TokenKind::DotDot) {
                    self.advance(); // '..'
                    let hi = if matches!(self.peek().kind, TokenKind::RBracket) {
                        None
                    } else {
                        Some(self.parse_expr()?)
                    };
                    self.expect(&TokenKind::RBracket, "Expected ']' to close slice range")?;
                    expr = RawExpr::SliceRange(Box::new(RawSliceRange {
                        base: expr,
                        lo: None,
                        hi,
                        span: lspan,
                    }));
                } else {
                    let first = self.parse_expr()?;
                    if matches!(self.peek().kind, TokenKind::DotDot) {
                        self.advance(); // '..'
                        let hi = if matches!(self.peek().kind, TokenKind::RBracket) {
                            None
                        } else {
                            Some(self.parse_expr()?)
                        };
                        self.expect(&TokenKind::RBracket, "Expected ']' to close slice range")?;
                        expr = RawExpr::SliceRange(Box::new(RawSliceRange {
                            base: expr,
                            lo: Some(first),
                            hi,
                            span: lspan,
                        }));
                    } else {
                        self.expect(&TokenKind::RBracket, "Expected ']' after index expression")?;
                        if let RawExpr::LayoutLoad(ref mut ll) = expr {
                            if ll.index.is_none() && ll.subfield.is_none() {
                                ll.index = Some(Box::new(first));
                                continue;
                            }
                        }
                        expr = RawExpr::SliceIndex(Box::new(RawSliceIndex {
                            base: expr,
                            index: first,
                            subfield: None,
                            span: lspan,
                        }));
                    }
                }
            } else if matches!(self.peek().kind, TokenKind::Dot)
                && matches!(self.peek_next(), TokenKind::Int(_) | TokenKind::Ident(_))
            {
                let dot_span = self.advance().span;
                match self.advance().kind {
                    TokenKind::Int(idx) => {
                        expr = RawExpr::TupleField {
                            expr: Box::new(expr),
                            index: idx.max(0) as usize,
                        };
                    }
                    TokenKind::Ident(field) => {
                        if let RawExpr::LayoutLoad(ref mut ll) = expr {
                            if ll.subfield.is_none() {
                                ll.subfield = Some(field);
                                continue;
                            }
                        }
                        if let RawExpr::SliceIndex(ref mut si) = expr {
                            if si.subfield.is_none() {
                                si.subfield = Some(field);
                                continue;
                            }
                        }
                        expr = RawExpr::FieldAccess {
                            expr: Box::new(expr),
                            field,
                            span: if dot_span.line > 0 { dot_span } else { origin_span },
                        };
                    }
                    _ => {}
                }
            } else {
                break;
            }
        }
        Ok(expr)
    }

    fn parse_arg_list(&mut self) -> Result<Vec<RawExpr>, InfimumDiagnostic> {
        self.expect(&TokenKind::LParen, "Expected '(' to start argument list")?;
        let mut args = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                args.push(self.parse_expr()?);
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' to close argument list")?;
        Ok(args)
    }
}

// ---------------------------------------------------------------------------
// Stage B: Compile-Time Evaluation, Static Layout Overlap Proof, & Desugaring
// ---------------------------------------------------------------------------


impl crate::forma::parser::FormaHostParser for Parser {
    type Expr = RawExpr;

    fn peek(&self) -> &Token {
        Parser::peek(self)
    }

    fn peek_next_is_colon(&self) -> bool {
        *self.peek_at(1) == TokenKind::Colon
    }

    fn advance(&mut self) -> Token {
        Parser::advance(self)
    }

    fn expect_ident(&mut self, what: &str) -> Result<String, InfimumDiagnostic> {
        let (name, _) = Parser::expect_ident(self, what)?;
        Ok(name)
    }

    fn parse_params(&mut self) -> Result<Vec<Param>, InfimumDiagnostic> {
        self.expect(&TokenKind::LParen, "Expected '(' to start parameter list")?;
        let mut params = Vec::new();
        if !matches!(self.peek().kind, TokenKind::RParen) {
            loop {
                let (pname, _) = Parser::expect_ident(self, "Expected parameter name")?;
                self.expect(&TokenKind::Colon, "Expected ':' after parameter name")?;
                let pty = self.parse_type()?;
                params.push(Param {
                    name: pname,
                    ty: pty,
                });
                if matches!(self.peek().kind, TokenKind::Comma) {
                    self.advance();
                    if matches!(self.peek().kind, TokenKind::RParen) {
                        break;
                    }
                } else {
                    break;
                }
            }
        }
        self.expect(&TokenKind::RParen, "Expected ')' after parameter list")?;
        Ok(params)
    }

    fn parse_effect_and_ret(&mut self) -> Result<(Vec<EffectCap>, Type), InfimumDiagnostic> {
        let mut effects = Vec::new();
        match &self.peek().kind {
            TokenKind::Arrow => {
                self.advance();
            }
            TokenKind::EffectOpen => {
                self.advance();
                if !matches!(self.peek().kind, TokenKind::EffectClose) {
                    loop {
                        let (ename, _) = Parser::expect_ident(
                            self,
                            "Expected effect capability name inside '-[...]->'",
                        )?;
                        let scope = if matches!(self.peek().kind, TokenKind::LParen) {
                            self.advance();
                            let s = match self.peek().kind.clone() {
                                TokenKind::Str(val) => {
                                    self.advance();
                                    val
                                }
                                other => {
                                    return Err(InfimumDiagnostic::new(
                                        "E012_EXPECTED_SCOPE_STR",
                                        "parse",
                                        format!("Expected string literal scope, found {other:?}"),
                                        self.peek().span,
                                        "Write capability scope as a string literal, e.g. Fs(\"/tmp\")",
                                    ));
                                }
                            };
                            self.expect(
                                &TokenKind::RParen,
                                "Expected ')' after capability scope literal",
                            )?;
                            Some(s)
                        } else {
                            None
                        };
                        effects.push(EffectCap { name: ename, scope });
                        if matches!(self.peek().kind, TokenKind::Comma) {
                            self.advance();
                        } else {
                            break;
                        }
                    }
                }
                self.expect(
                    &TokenKind::EffectClose,
                    "Close capability list with ']->' before return type",
                )?;
            }
            other => {
                return Err(InfimumDiagnostic::new(
                    "E013_EXPECTED_RETURN_ARROW",
                    "parse",
                    format!("Expected '->' or '-[...]->', found {other:?}"),
                    self.peek().span,
                    "Specify return type with '-> <Type>' or effects + return type with '-[Mem]-> <Type>'",
                ));
            }
        }
        let ret_type = self.parse_type()?;
        Ok((effects, ret_type))
    }

    fn parse_expr(&mut self) -> Result<RawExpr, InfimumDiagnostic> {
        Parser::parse_expr(self)
    }
}


fn rewrite_assert_stmt(stmt: RawStmt, default_line: i64) -> RawStmt {
    match stmt {
        RawStmt::Expr(RawExpr::Call {
            target: TargetRef::Named(name),
            args,
            span: _,
        }) if name == "assert" && args.len() == 1 => {
            let cond = args.into_iter().next().unwrap();
            RawStmt::Expr(RawExpr::If {
                cond: Box::new(RawExpr::Unary {
                    op: UnaryOp::Not,
                    expr: Box::new(cond),
                }),
                then_branch: Box::new(RawExpr::If {
                    cond: Box::new(RawExpr::Binary {
                        op: BinOp::Eq,
                        lhs: Box::new(RawExpr::Var("__ifm_test_fail".to_string())),
                        rhs: Box::new(RawExpr::IntLit(0)),
                    }),
                    then_branch: Box::new(RawExpr::Block {
                        stmts: vec![RawStmt::Assign {
                            name: "__ifm_test_fail".to_string(),
                            op: None,
                            value: RawExpr::IntLit(default_line),
                        }],
                        tail: Box::new(RawExpr::UnitLit),
                    }),
                    else_branch: Box::new(RawExpr::UnitLit),
                }),
                else_branch: Box::new(RawExpr::UnitLit),
            })
        }
        RawStmt::While {
            cond,
            max_iters,
            body,
            span,
        } => RawStmt::While {
            cond,
            max_iters,
            body: body
                .into_iter()
                .map(|s| rewrite_assert_stmt(s, span.line as i64))
                .collect(),
            span,
        },
        RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body,
            span,
        } => RawStmt::For {
            var,
            start,
            end,
            inclusive,
            max_iters,
            body: body
                .into_iter()
                .map(|s| rewrite_assert_stmt(s, span.line as i64))
                .collect(),
            span,
        },
        other => other,
    }
}

