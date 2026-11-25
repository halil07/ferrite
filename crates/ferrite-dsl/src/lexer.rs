#![forbid(unsafe_code)]

use crate::error::{DslError, Span};

/// Sentinel char stored when `\$` is escaped inside `.ferrite` string literals so compiler
/// interpolation treats it as a literal `$` rather than `${...}`.
pub const ESCAPED_DOLLAR_SENTINEL: char = '\u{E001}';

#[derive(Debug, Clone, PartialEq)]
pub enum TokenKind {
    Ident(String),
    Str(String),
    Ref(String),
    DurationMs(u64),
    Int(i64),
    Float(f64),
    LBrace,
    RBrace,
    LParen,
    RParen,
    LBracket,
    RBracket,
    Colon,
    Comma,
    Equals,
    Eof,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Token {
    pub kind: TokenKind,
    pub span: Span,
}

pub fn tokenize(file: &str, src: &str) -> Result<Vec<Token>, DslError> {
    let mut lexer = Lexer {
        file,
        bytes: src.as_bytes(),
        src,
        pos: 0,
        line: 1,
        col: 1,
    };
    lexer.lex_all()
}

struct Lexer<'a> {
    file: &'a str,
    bytes: &'a [u8],
    src: &'a str,
    pos: usize,
    line: usize,
    col: usize,
}

impl<'a> Lexer<'a> {
    fn span(&self) -> Span {
        Span {
            line: self.line,
            col: self.col,
        }
    }

    fn peek(&self) -> Option<char> {
        self.src[self.pos..].chars().next()
    }

    fn starts_with(&self, pat: &str) -> bool {
        self.src[self.pos..].starts_with(pat)
    }

    fn bump(&mut self) -> Option<char> {
        let ch = self.peek()?;
        self.pos += ch.len_utf8();
        if ch == '\n' {
            self.line += 1;
            self.col = 1;
        } else {
            self.col += 1;
        }
        Some(ch)
    }

    fn err(&self, span: Span, message: impl Into<String>) -> DslError {
        DslError::Lex {
            file: self.file.to_string(),
            span,
            message: message.into(),
        }
    }

    fn skip_whitespace_and_comments(&mut self) -> Result<(), DslError> {
        loop {
            while let Some(ch) = self.peek() {
                if ch.is_whitespace() {
                    self.bump();
                } else {
                    break;
                }
            }

            if self.starts_with("//") || self.starts_with("#") {
                while let Some(ch) = self.bump() {
                    if ch == '\n' {
                        break;
                    }
                }
                continue;
            }

            if self.starts_with("/*") {
                let start_span = self.span();
                self.bump();
                self.bump();
                let mut closed = false;
                while self.pos < self.bytes.len() {
                    if self.starts_with("*/") {
                        self.bump();
                        self.bump();
                        closed = true;
                        break;
                    }
                    self.bump();
                }
                if !closed {
                    return Err(self.err(start_span, "unterminated block comment `/* ... */`"));
                }
                continue;
            }

            break;
        }
        Ok(())
    }

    fn lex_all(&mut self) -> Result<Vec<Token>, DslError> {
        let mut tokens = Vec::new();
        loop {
            self.skip_whitespace_and_comments()?;
            let span = self.span();
            let Some(ch) = self.peek() else {
                tokens.push(Token {
                    kind: TokenKind::Eof,
                    span,
                });
                break;
            };

            let kind = match ch {
                '{' => {
                    self.bump();
                    TokenKind::LBrace
                }
                '}' => {
                    self.bump();
                    TokenKind::RBrace
                }
                '(' => {
                    self.bump();
                    TokenKind::LParen
                }
                ')' => {
                    self.bump();
                    TokenKind::RParen
                }
                '[' => {
                    self.bump();
                    TokenKind::LBracket
                }
                ']' => {
                    self.bump();
                    TokenKind::RBracket
                }
                ':' => {
                    self.bump();
                    TokenKind::Colon
                }
                ',' => {
                    self.bump();
                    TokenKind::Comma
                }
                '=' => {
                    self.bump();
                    TokenKind::Equals
                }
                '"' => {
                    if self.starts_with("\"\"\"") {
                        TokenKind::Str(self.lex_triple_string(span)?)
                    } else {
                        TokenKind::Str(self.lex_string(span)?)
                    }
                }
                '@' => TokenKind::Ref(self.lex_ref(span)?),
                '-' | '0'..='9' => self.lex_number_or_duration(span)?,
                c if is_ident_start(c) => TokenKind::Ident(self.lex_ident()),
                other => {
                    return Err(self.err(span, format!("unexpected character `{other}`")));
                }
            };

            tokens.push(Token { kind, span });
        }
        Ok(tokens)
    }

    fn lex_string(&mut self, start_span: Span) -> Result<String, DslError> {
        self.bump(); // opening "
        let mut out = String::new();
        while let Some(ch) = self.bump() {
            match ch {
                '"' => return Ok(out),
                '\n' => {
                    return Err(self.err(
                        start_span,
                        "unterminated string literal (use \"\"\"...\"\"\" for multiline strings)",
                    ));
                }
                '\\' => {
                    let esc_span = self.span();
                    let Some(esc) = self.bump() else {
                        return Err(self.err(start_span, "unterminated escape in string"));
                    };
                    match esc {
                        '"' => out.push('"'),
                        '\\' => out.push('\\'),
                        'n' => out.push('\n'),
                        'r' => out.push('\r'),
                        't' => out.push('\t'),
                        '$' => out.push(ESCAPED_DOLLAR_SENTINEL),
                        other => {
                            return Err(
                                self.err(esc_span, format!("unknown escape sequence `\\{other}`"))
                            );
                        }
                    }
                }
                other => out.push(other),
            }
        }
        Err(self.err(start_span, "unterminated string literal"))
    }

    fn lex_triple_string(&mut self, start_span: Span) -> Result<String, DslError> {
        self.bump();
        self.bump();
        self.bump(); // """

        // Trim optional immediate newline after opening delimiter.
        if self.starts_with("\r\n") {
            self.bump();
            self.bump();
        } else if self.starts_with("\n") {
            self.bump();
        }

        let mut raw = String::new();
        let mut closed = false;
        while self.pos < self.bytes.len() {
            if self.starts_with("\"\"\"") {
                self.bump();
                self.bump();
                self.bump();
                closed = true;
                break;
            }
            if self.starts_with("\\\"\"\"") {
                self.bump();
                self.bump();
                self.bump();
                self.bump();
                raw.push_str("\"\"\"");
                continue;
            }
            if self.starts_with("\\$") {
                self.bump();
                self.bump();
                raw.push(ESCAPED_DOLLAR_SENTINEL);
                continue;
            }
            if let Some(ch) = self.bump() {
                raw.push(ch);
            }
        }

        if !closed {
            return Err(self.err(start_span, "unterminated multiline `\"\"\"` string"));
        }

        Ok(dedent_multiline(&raw))
    }

    fn lex_ref(&mut self, start_span: Span) -> Result<String, DslError> {
        self.bump(); // @
        let mut id = String::new();
        while let Some(ch) = self.peek() {
            if ch.is_ascii_alphanumeric() || ch == '_' || ch == '-' {
                id.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        if id.is_empty() {
            return Err(self.err(start_span, "expected element reference name after `@` (e.g. `@e1`)"));
        }
        Ok(id)
    }

    fn lex_ident(&mut self) -> String {
        let mut out = String::new();
        while let Some(ch) = self.peek() {
            if is_ident_continue(ch) {
                out.push(ch);
                self.bump();
            } else {
                break;
            }
        }
        out
    }

    fn lex_number_or_duration(&mut self, start_span: Span) -> Result<TokenKind, DslError> {
        let mut num = String::new();
        if self.peek() == Some('-') {
            num.push('-');
            self.bump();
            if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                return Err(self.err(start_span, "unexpected `-` without digits"));
            }
        }

        let mut is_float = false;
        while let Some(ch) = self.peek() {
            if ch.is_ascii_digit() {
                num.push(ch);
                self.bump();
            } else if ch == '.' && !is_float {
                is_float = true;
                num.push('.');
                self.bump();
                if !self.peek().is_some_and(|c| c.is_ascii_digit()) {
                    return Err(self.err(start_span, "expected digits after decimal point"));
                }
            } else {
                break;
            }
        }

        // Optional duration suffix on non-negative integers: ms, s, m
        if !is_float && !num.starts_with('-') {
            let mut suffix = String::new();
            while let Some(ch) = self.peek() {
                if ch.is_ascii_alphabetic() {
                    suffix.push(ch);
                    self.bump();
                } else {
                    break;
                }
            }
            if !suffix.is_empty() {
                let base: u64 = num
                    .parse()
                    .map_err(|_| self.err(start_span, format!("invalid duration `{num}{suffix}`")))?;
                let mult = match suffix.as_str() {
                    "ms" => 1,
                    "s" => 1_000,
                    "m" => 60_000,
                    other => {
                        return Err(self.err(
                            start_span,
                            format!("unknown duration suffix `{other}` (use `ms`, `s`, or `m`)"),
                        ));
                    }
                };
                return Ok(TokenKind::DurationMs(base.saturating_mul(mult)));
            }
        }

        if is_float {
            let val: f64 = num
                .parse()
                .map_err(|_| self.err(start_span, format!("invalid float `{num}`")))?;
            Ok(TokenKind::Float(val))
        } else {
            let val: i64 = num
                .parse()
                .map_err(|_| self.err(start_span, format!("invalid integer `{num}`")))?;
            Ok(TokenKind::Int(val))
        }
    }
}

fn is_ident_start(c: char) -> bool {
    c.is_ascii_alphabetic() || c == '_'
}

fn is_ident_continue(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_'
}

fn dedent_multiline(raw: &str) -> String {
    let trimmed_end = raw.trim_end_matches([' ', '\t', '\r', '\n']);
    let lines: Vec<&str> = trimmed_end.split('\n').map(|l| l.trim_end_matches('\r')).collect();
    let min_indent = lines
        .iter()
        .filter(|l| !l.trim().is_empty())
        .map(|l| l.bytes().take_while(|b| *b == b' ' || *b == b'\t').count())
        .min()
        .unwrap_or(0);

    lines
        .into_iter()
        .map(|l| {
            if l.len() >= min_indent && !l.trim().is_empty() {
                &l[min_indent..]
            } else if l.trim().is_empty() {
                ""
            } else {
                l
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lexes_comments_refs_durations_and_multiline_strings() {
        let src = r#"
            // single-line comment
            # hash comment
            /* block
               comment */
            click @e12
            wait 250ms
            goto "https://example.com" timeout 15s
            mock "/login" {
                body """
                    <h1>Hello</h1>
                """
            }
        "#;
        let tokens = tokenize("test.ferrite", src).unwrap();
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Ref("e12".into())));
        assert!(tokens.iter().any(|t| t.kind == TokenKind::DurationMs(250)));
        assert!(tokens.iter().any(|t| t.kind == TokenKind::DurationMs(15_000)));
        assert!(tokens.iter().any(|t| t.kind == TokenKind::Str("<h1>Hello</h1>".into())));
    }
}
