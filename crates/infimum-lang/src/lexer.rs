use crate::diag::{InfimumDiagnostic, Span};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TokenKind {
    // Keywords
    Export,
    Use,
    Const,
    Enum,
    Layout,
    Forma,
    Match,
    Fn,
    Let,
    Mut,
    While,
    For,
    Break,
    Continue,
    Max,
    If,
    Else,
    Do,
    Req,
    Ens,
    True,
    False,
    SecretKw,
    RevealKw,
    MemLoadKw,
    MemStoreKw,

    // Identifiers & Literals
    Ident(String),
    HashRef(String),
    Int(i64),
    Fixed(i64),
    Str(String),

    // Punctuation & Arrows
    LParen,
    RParen,
    LBrace,
    RBrace,
    LBracket,
    RBracket,
    EffectOpen,  // -[
    EffectClose, // ]->
    Arrow,       // ->
    FatArrow,    // =>
    ColonColon,  // ::
    Colon,
    Comma,
    Semi,
    DotDotEq, // ..=
    DotDot,   // ..
    Dot,
    Assign,  // =
    PlusEq,  // +=
    MinusEq, // -=
    StarEq,  // *=
    PipeEq,  // |=
    AmpEq,   // &=

    // Operators
    Plus,
    Minus,
    Star,
    Slash,
    Percent,
    Amp,
    Pipe,
    Caret,
    Shl,
    Shr,
    Bang,
    EqEq,
    NotEq,
    Lt,
    Le,
    Gt,
    Ge,
    AndAnd,
    OrOr,

    Eof,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub fn tokenize(src: &str) -> Result<Vec<Token>, InfimumDiagnostic> {
    let bytes = src.as_bytes();
    let len = bytes.len();
    let mut pos = 0usize;
    let mut line = 1usize;
    let mut col = 1usize;
    let mut tokens = Vec::new();

    while pos < len {
        let ch = src[pos..].chars().next().unwrap();
        let ch_len = ch.len_utf8();

        if ch.is_whitespace() {
            if ch == '\n' {
                line += 1;
                col = 1;
            } else {
                col += 1;
            }
            pos += ch_len;
            continue;
        }

        if src[pos..].starts_with("//") {
            while pos < len && bytes[pos] != b'\n' {
                pos += 1;
            }
            continue;
        }

        let start = pos;
        let start_line = line;
        let start_col = col;

        // 3-char tokens
        if src[pos..].starts_with("]->") {
            pos += 3;
            col += 3;
            tokens.push(Token {
                kind: TokenKind::EffectClose,
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }
        if src[pos..].starts_with("..=") {
            pos += 3;
            col += 3;
            tokens.push(Token {
                kind: TokenKind::DotDotEq,
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // 2-char tokens
        let two_char = if src[pos..].starts_with("-[") {
            Some(TokenKind::EffectOpen)
        } else if src[pos..].starts_with("..") {
            Some(TokenKind::DotDot)
        } else if src[pos..].starts_with("->") {
            Some(TokenKind::Arrow)
        } else if src[pos..].starts_with("=>") {
            Some(TokenKind::FatArrow)
        } else if src[pos..].starts_with("::") {
            Some(TokenKind::ColonColon)
        } else if src[pos..].starts_with("+=") {
            Some(TokenKind::PlusEq)
        } else if src[pos..].starts_with("-=") {
            Some(TokenKind::MinusEq)
        } else if src[pos..].starts_with("*=") {
            Some(TokenKind::StarEq)
        } else if src[pos..].starts_with("|=") {
            Some(TokenKind::PipeEq)
        } else if src[pos..].starts_with("&=") {
            Some(TokenKind::AmpEq)
        } else if src[pos..].starts_with("==") {
            Some(TokenKind::EqEq)
        } else if src[pos..].starts_with("!=") {
            Some(TokenKind::NotEq)
        } else if src[pos..].starts_with("<<") {
            Some(TokenKind::Shl)
        } else if src[pos..].starts_with(">>") {
            Some(TokenKind::Shr)
        } else if src[pos..].starts_with("<=") {
            Some(TokenKind::Le)
        } else if src[pos..].starts_with(">=") {
            Some(TokenKind::Ge)
        } else if src[pos..].starts_with("&&") {
            Some(TokenKind::AndAnd)
        } else if src[pos..].starts_with("||") {
            Some(TokenKind::OrOr)
        } else {
            None
        };

        if let Some(kind) = two_char {
            pos += 2;
            col += 2;
            tokens.push(Token {
                kind,
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // Hash literal: #<hex>
        if ch == '#' {
            pos += 1;
            col += 1;
            let hex_start = pos;
            while pos < len && bytes[pos].is_ascii_hexdigit() {
                pos += 1;
                col += 1;
            }
            let hex = &src[hex_start..pos];
            if hex.len() < 6 {
                return Err(InfimumDiagnostic::new(
                    "E001_INVALID_HASH_REF",
                    "lex",
                    format!("Hash reference '#{hex}' must have at least 6 hex digits"),
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Provide a valid node hash prefix of 6..64 lowercase hex digits (e.g. #8f3a12)",
                ));
            }
            tokens.push(Token {
                kind: TokenKind::HashRef(hex.to_ascii_lowercase()),
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // Character literal: 'a', '\n', '\t', '\r', '\\', '\'', '\0', or any UTF-8 scalar -> i64 codepoint
        if ch == '\'' {
            pos += 1;
            col += 1;
            if pos >= len {
                return Err(InfimumDiagnostic::new(
                    "E005_INVALID_CHAR_LIT",
                    "lex",
                    "Unterminated character literal",
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Write a single character literal enclosed in single quotes, e.g. 'a' or '\\n'",
                ));
            }
            let c = src[pos..].chars().next().unwrap();
            let resolved_ch = if c == '\\' {
                pos += 1;
                col += 1;
                if pos >= len {
                    return Err(InfimumDiagnostic::new(
                        "E005_INVALID_CHAR_LIT",
                        "lex",
                        "Unterminated escape sequence in character literal",
                        Span {
                            start,
                            end: pos,
                            line: start_line,
                            col: start_col,
                        },
                        "Use a valid escape sequence: '\\n', '\\t', '\\r', '\\\\', '\\'', '\\\"', or '\\0'",
                    ));
                }
                let esc = src[pos..].chars().next().unwrap();
                pos += esc.len_utf8();
                col += 1;
                match esc {
                    'n' => '\n',
                    't' => '\t',
                    'r' => '\r',
                    '\\' => '\\',
                    '\'' => '\'',
                    '"' => '"',
                    '0' => '\0',
                    other => {
                        return Err(InfimumDiagnostic::new(
                            "E005_INVALID_CHAR_LIT",
                            "lex",
                            format!("Unknown escape sequence '\\{other}' in character literal"),
                            Span {
                                start,
                                end: pos,
                                line: start_line,
                                col: start_col,
                            },
                            "Use a supported escape sequence: '\\n', '\\t', '\\r', '\\\\', '\\'', '\\\"', or '\\0'",
                        ));
                    }
                }
            } else if c == '\'' || c == '\n' {
                return Err(InfimumDiagnostic::new(
                    "E005_INVALID_CHAR_LIT",
                    "lex",
                    "Empty or unescaped newline in character literal",
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Provide a single character inside quotes (use '\\n' for newline)",
                ));
            } else {
                pos += c.len_utf8();
                col += 1;
                c
            };

            if pos >= len || bytes[pos] != b'\'' {
                return Err(InfimumDiagnostic::new(
                    "E005_INVALID_CHAR_LIT",
                    "lex",
                    "Character literal must contain exactly one character and close with '\\''",
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Close the character literal with a single quote '\\'' (e.g. 'a')",
                ));
            }
            pos += 1;
            col += 1;
            tokens.push(Token {
                kind: TokenKind::Int(resolved_ch as u32 as i64),
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // String literal
        if ch == '"' {
            pos += 1;
            col += 1;
            let mut buf = String::new();
            let mut closed = false;
            while pos < len {
                let c = src[pos..].chars().next().unwrap();
                let clen = c.len_utf8();
                pos += clen;
                if c == '\n' {
                    line += 1;
                    col = 1;
                } else {
                    col += 1;
                }
                if c == '"' {
                    closed = true;
                    break;
                }
                if c == '\\' {
                    if pos >= len {
                        break;
                    }
                    let esc = src[pos..].chars().next().unwrap();
                    pos += esc.len_utf8();
                    col += 1;
                    match esc {
                        'n' => buf.push('\n'),
                        't' => buf.push('\t'),
                        'r' => buf.push('\r'),
                        '"' => buf.push('"'),
                        '\\' => buf.push('\\'),
                        other => buf.push(other),
                    }
                } else {
                    buf.push(c);
                }
            }
            if !closed {
                return Err(InfimumDiagnostic::new(
                    "E002_UNTERMINATED_STRING",
                    "lex",
                    "Unterminated string literal",
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Close the string literal with a matching double quote '\"'",
                ));
            }
            tokens.push(Token {
                kind: TokenKind::Str(buf),
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // Hex or decimal integer literal (supports 0xFF228833 and underscore separators 10_000)
        if ch.is_ascii_digit() {
            if src[pos..].starts_with("0x") || src[pos..].starts_with("0X") {
                pos += 2;
                col += 2;
                let hex_start = pos;
                while pos < len && (bytes[pos].is_ascii_hexdigit() || bytes[pos] == b'_') {
                    pos += 1;
                    col += 1;
                }
                let clean: String = src[hex_start..pos]
                    .chars()
                    .filter(|&c| c != '_')
                    .collect();
                let uval = u64::from_str_radix(&clean, 16).map_err(|_| {
                    InfimumDiagnostic::new(
                        "E003_INT_OVERFLOW",
                        "lex",
                        format!("Invalid or overflowing hex literal '0x{clean}'"),
                        Span {
                            start,
                            end: pos,
                            line: start_line,
                            col: start_col,
                        },
                        "Use a valid 64-bit hexadecimal literal (e.g. 0xFF228844)",
                    )
                })?;
                tokens.push(Token {
                    kind: TokenKind::Int(uval as i64),
                    span: Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                });
                continue;
            }

            while pos < len && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
                pos += 1;
                col += 1;
            }
            // Fixed-point Q16.16 literal: e.g. 1.5, 0.25, 3.14159 (only when '.' is followed by a digit, never '..')
            if pos + 1 < len && bytes[pos] == b'.' && bytes[pos + 1].is_ascii_digit() {
                let whole_clean: String = src[start..pos].chars().filter(|&c| c != '_').collect();
                let whole = whole_clean.parse::<i128>().map_err(|_| {
                    InfimumDiagnostic::new(
                        "E003_INT_OVERFLOW",
                        "lex",
                        format!("Fixed-point literal '{}' overflows i64", &src[start..pos]),
                        Span {
                            start,
                            end: pos,
                            line: start_line,
                            col: start_col,
                        },
                        "Use a smaller fixed-point value",
                    )
                })?;
                pos += 1; // skip '.'
                col += 1;
                let frac_start = pos;
                while pos < len && (bytes[pos].is_ascii_digit() || bytes[pos] == b'_') {
                    pos += 1;
                    col += 1;
                }
                let mut frac_val: i128 = 0;
                let mut frac_scale: i128 = 1;
                let mut frac_digits = 0u32;
                for c in src[frac_start..pos].chars() {
                    if c == '_' {
                        continue;
                    }
                    if frac_digits < 9 {
                        frac_val = frac_val * 10 + (c as i128 - '0' as i128);
                        frac_scale *= 10;
                        frac_digits += 1;
                    }
                }
                let frac_q16 = (frac_val * 65536 + frac_scale / 2) / frac_scale;
                let total_q16 = whole
                    .checked_mul(65536)
                    .and_then(|w| w.checked_add(frac_q16))
                    .and_then(|v| i64::try_from(v).ok())
                    .ok_or_else(|| {
                        InfimumDiagnostic::new(
                            "E003_INT_OVERFLOW",
                            "lex",
                            format!("Fixed-point literal '{}' overflows Q16.16 i64", &src[start..pos]),
                            Span {
                                start,
                                end: pos,
                                line: start_line,
                                col: start_col,
                            },
                            "Use a fixed-point value within Q47.16 range",
                        )
                    })?;
                tokens.push(Token {
                    kind: TokenKind::Fixed(total_q16),
                    span: Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                });
                continue;
            }
            let clean: String = src[start..pos].chars().filter(|&c| c != '_').collect();
            let val = clean.parse::<i64>().map_err(|_| {
                InfimumDiagnostic::new(
                    "E003_INT_OVERFLOW",
                    "lex",
                    format!("Integer literal '{}' overflows i64", &src[start..pos]),
                    Span {
                        start,
                        end: pos,
                        line: start_line,
                        col: start_col,
                    },
                    "Use an integer within the signed 64-bit range",
                )
            })?;
            tokens.push(Token {
                kind: TokenKind::Int(val),
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // Identifiers & Keywords
        if ch.is_ascii_alphabetic() || ch == '_' || ch == '$' {
            pos += 1;
            col += 1;
            while pos < len && (bytes[pos].is_ascii_alphanumeric() || bytes[pos] == b'_') {
                pos += 1;
                col += 1;
            }
            let word = &src[start..pos];
            let kind = match word {
                "export" => TokenKind::Export,
                "use" => TokenKind::Use,
                "const" => TokenKind::Const,
                "enum" => TokenKind::Enum,
                "layout" => TokenKind::Layout,
                "forma" => TokenKind::Forma,
                "match" => TokenKind::Match,
                "fn" => TokenKind::Fn,
                "let" => TokenKind::Let,
                "mut" => TokenKind::Mut,
                "while" => TokenKind::While,
                "for" => TokenKind::For,
                "break" => TokenKind::Break,
                "continue" => TokenKind::Continue,
                "max" => TokenKind::Max,
                "if" => TokenKind::If,
                "else" => TokenKind::Else,
                "do" => TokenKind::Do,
                "req" => TokenKind::Req,
                "ens" => TokenKind::Ens,
                "true" => TokenKind::True,
                "false" => TokenKind::False,
                "secret" => TokenKind::SecretKw,
                "reveal" => TokenKind::RevealKw,
                "mem_load" => TokenKind::MemLoadKw,
                "mem_store" => TokenKind::MemStoreKw,
                _ => TokenKind::Ident(word.to_string()),
            };
            tokens.push(Token {
                kind,
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        // Single-char punctuation
        let single = match ch {
            '(' => Some(TokenKind::LParen),
            ')' => Some(TokenKind::RParen),
            '{' => Some(TokenKind::LBrace),
            '}' => Some(TokenKind::RBrace),
            '[' => Some(TokenKind::LBracket),
            ']' => Some(TokenKind::RBracket),
            ':' => Some(TokenKind::Colon),
            ',' => Some(TokenKind::Comma),
            ';' => Some(TokenKind::Semi),
            '.' => Some(TokenKind::Dot),
            '=' => Some(TokenKind::Assign),
            '+' => Some(TokenKind::Plus),
            '-' => Some(TokenKind::Minus),
            '*' => Some(TokenKind::Star),
            '/' => Some(TokenKind::Slash),
            '%' => Some(TokenKind::Percent),
            '&' => Some(TokenKind::Amp),
            '|' => Some(TokenKind::Pipe),
            '^' => Some(TokenKind::Caret),
            '!' => Some(TokenKind::Bang),
            '<' => Some(TokenKind::Lt),
            '>' => Some(TokenKind::Gt),
            _ => None,
        };

        if let Some(kind) = single {
            pos += ch_len;
            col += 1;
            tokens.push(Token {
                kind,
                span: Span {
                    start,
                    end: pos,
                    line: start_line,
                    col: start_col,
                },
            });
            continue;
        }

        return Err(InfimumDiagnostic::new(
            "E004_UNEXPECTED_CHAR",
            "lex",
            format!("Unexpected character '{ch}'"),
            Span {
                start,
                end: pos + ch_len,
                line: start_line,
                col: start_col,
            },
            "Remove invalid character or check Infimum token syntax",
        ));
    }

    tokens.push(Token {
        kind: TokenKind::Eof,
        span: Span {
            start: len,
            end: len,
            line,
            col,
        },
    });
    Ok(tokens)
}
