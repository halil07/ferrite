#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;

use ferrite_cdp::LoadState;
use serde_json::{Map, Value};

use crate::ast::{MacroDef, MockBodyAst, MockStmt, Program, Stmt, TestCaseAst};
use crate::error::{DslError, Span};
use crate::lexer::{Token, TokenKind, tokenize};

pub fn parse_program(file: &str, source_dir: &Path, src: &str) -> Result<Program, DslError> {
    let tokens = tokenize(file, src)?;
    let mut parser = Parser {
        file,
        source_dir,
        tokens,
        pos: 0,
    };
    parser.parse_program()
}

struct Parser<'a> {
    file: &'a str,
    source_dir: &'a Path,
    tokens: Vec<Token>,
    pos: usize,
}

impl<'a> Parser<'a> {
    fn current(&self) -> &Token {
        self.tokens
            .get(self.pos)
            .unwrap_or_else(|| self.tokens.last().expect("tokens always ends with Eof"))
    }

    fn peek_kind(&self) -> &TokenKind {
        &self.current().kind
    }

    fn peek_span(&self) -> Span {
        self.current().span
    }

    fn bump(&mut self) -> Token {
        let tok = self.current().clone();
        if self.pos + 1 < self.tokens.len() {
            self.pos += 1;
        }
        tok
    }

    fn err(&self, span: Span, message: impl Into<String>) -> DslError {
        DslError::Parse {
            file: self.file.to_string(),
            span,
            message: message.into(),
        }
    }

    fn at_ident(&self, expected: &str) -> bool {
        matches!(self.peek_kind(), TokenKind::Ident(s) if s == expected)
    }

    fn eat_ident(&mut self, expected: &str) -> bool {
        if self.at_ident(expected) {
            self.bump();
            true
        } else {
            false
        }
    }

    fn expect_ident(&mut self) -> Result<(String, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Ident(name) => Ok((name, tok.span)),
            other => Err(self.err(tok.span, format!("expected identifier, found {other:?}"))),
        }
    }

    fn expect_string(&mut self) -> Result<(String, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Str(val) => Ok((val, tok.span)),
            other => Err(self.err(tok.span, format!("expected string literal, found {other:?}"))),
        }
    }

    fn expect_scalar_string(&mut self) -> Result<(String, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Str(s) => Ok((s, tok.span)),
            TokenKind::Int(i) => Ok((i.to_string(), tok.span)),
            TokenKind::Float(f) => Ok((f.to_string(), tok.span)),
            TokenKind::Ref(r) => Ok((r, tok.span)),
            TokenKind::DurationMs(ms) => Ok((ms.to_string(), tok.span)),
            TokenKind::Ident(b) if b == "true" || b == "false" => Ok((b, tok.span)),
            other => Err(self.err(tok.span, format!("expected scalar value, found {other:?}"))),
        }
    }

    fn expect_kind(&mut self, expected: &TokenKind, label: &str) -> Result<Span, DslError> {
        let tok = self.bump();
        if &tok.kind == expected {
            Ok(tok.span)
        } else {
            Err(self.err(tok.span, format!("expected {label}, found {:?}", tok.kind)))
        }
    }

    fn expect_u64_int(&mut self) -> Result<(u64, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Int(n) if n >= 0 => Ok((n as u64, tok.span)),
            other => Err(self.err(
                tok.span,
                format!("expected non-negative integer, found {other:?}"),
            )),
        }
    }

    fn expect_f64(&mut self) -> Result<(f64, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Float(f) => Ok((f, tok.span)),
            TokenKind::Int(i) => Ok((i as f64, tok.span)),
            other => Err(self.err(tok.span, format!("expected number, found {other:?}"))),
        }
    }

    fn parse_program(&mut self) -> Result<Program, DslError> {
        let mut prog = Program {
            suite_name: None,
            imports: Vec::new(),
            globals: Vec::new(),
            before_each: Vec::new(),
            macros: Vec::new(),
            tests: Vec::new(),
        };

        while *self.peek_kind() != TokenKind::Eof {
            let (kw, span) = self.expect_ident()?;
            match kw.as_str() {
                "suite" => {
                    let (name, _) = self.expect_string()?;
                    if prog.suite_name.is_some() {
                        return Err(self.err(span, "duplicate `suite` declaration"));
                    }
                    prog.suite_name = Some(name);
                }
                "import" => {
                    let (path, p_span) = self.expect_string()?;
                    prog.imports.push((path, p_span));
                }
                "let" => {
                    let (name, _) = self.expect_ident()?;
                    self.expect_kind(&TokenKind::Equals, "`=`")?;
                    let (val, val_span) = self.expect_scalar_string()?;
                    prog.globals.push((name, val, val_span));
                }
                "beforeEach" => {
                    if !prog.before_each.is_empty() {
                        return Err(self.err(span, "duplicate `beforeEach` block"));
                    }
                    prog.before_each = self.parse_block()?;
                }
                "macro" => {
                    prog.macros.push(self.parse_macro(span)?);
                }
                "test" => {
                    let (name, _) = self.expect_string()?;
                    let body = self.parse_block()?;
                    prog.tests.push(TestCaseAst { name, body, span });
                }
                other => {
                    return Err(self.err(
                        span,
                        format!(
                            "unexpected top-level keyword `{other}` (expected `suite`, `import`, `let`, `beforeEach`, `macro`, or `test`)"
                        ),
                    ));
                }
            }
        }

        Ok(prog)
    }

    fn parse_macro(&mut self, span: Span) -> Result<MacroDef, DslError> {
        let (name, _) = self.expect_ident()?;
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let mut params = Vec::new();
        while *self.peek_kind() != TokenKind::RParen {
            let (param, _) = self.expect_ident()?;
            params.push(param);
            if *self.peek_kind() == TokenKind::Comma {
                self.bump();
            } else {
                break;
            }
        }
        self.expect_kind(&TokenKind::RParen, "`)`")?;
        let body = self.parse_block()?;
        Ok(MacroDef {
            name,
            params,
            body,
            span,
            source_dir: self.source_dir.to_path_buf(),
        })
    }

    fn parse_block(&mut self) -> Result<Vec<Stmt>, DslError> {
        self.expect_kind(&TokenKind::LBrace, "`{`")?;
        let mut stmts = Vec::new();
        while *self.peek_kind() != TokenKind::RBrace {
            if *self.peek_kind() == TokenKind::Eof {
                return Err(self.err(self.peek_span(), "unclosed `{` block"));
            }
            stmts.push(self.parse_stmt()?);
        }
        self.bump(); // }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Stmt, DslError> {
        let (kw, span) = self.expect_ident()?;
        match kw.as_str() {
            "let" => {
                let (name, _) = self.expect_ident()?;
                self.expect_kind(&TokenKind::Equals, "`=`")?;
                let (value, _) = self.expect_scalar_string()?;
                Ok(Stmt::Let { name, value, span })
            }
            "tab" => {
                let tok = self.bump();
                match tok.kind {
                    TokenKind::Int(tab_id) => Ok(Stmt::Tab { tab_id, span }),
                    other => Err(self.err(tok.span, format!("expected tab ID integer, found {other:?}"))),
                }
            }
            "use" => {
                let (macro_name, _) = self.expect_ident()?;
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let mut args = Vec::new();
                while *self.peek_kind() != TokenKind::RParen {
                    let (arg, _) = self.expect_scalar_string()?;
                    args.push(arg);
                    if *self.peek_kind() == TokenKind::Comma {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(Stmt::Use {
                    macro_name,
                    args,
                    span,
                })
            }
            "goto" => {
                let (url, _) = self.expect_string()?;
                let mut wait_until = LoadState::Load;
                let mut timeout_ms = 15_000u64;
                loop {
                    if self.eat_ident("wait") {
                        let (mode, mode_span) = self.expect_ident()?;
                        wait_until = match mode.as_str() {
                            "load" => LoadState::Load,
                            "domcontentloaded" => LoadState::DomContentLoaded,
                            "commit" => LoadState::Commit,
                            other => {
                                return Err(self.err(
                                    mode_span,
                                    format!("unknown wait state `{other}` (use `load`, `domcontentloaded`, or `commit`)"),
                                ));
                            }
                        };
                    } else if self.eat_ident("timeout") {
                        timeout_ms = self.expect_duration_ms()?;
                    } else {
                        break;
                    }
                }
                Ok(Stmt::Goto {
                    url,
                    wait_until,
                    timeout_ms,
                    span,
                })
            }
            "snapshot" => {
                let interactive_only = if self.eat_ident("full") {
                    false
                } else {
                    let _ = self.eat_ident("interactive");
                    true
                };
                Ok(Stmt::Snapshot {
                    interactive_only,
                    span,
                })
            }
            "click" => {
                if self.eat_ident("role") {
                    let (role, _) = self.expect_string()?;
                    let name = if matches!(self.peek_kind(), TokenKind::Str(_)) {
                        Some(self.expect_string()?.0)
                    } else {
                        None
                    };
                    let (exact, nth, timeout_ms) = self.parse_match_modifiers(5_000, true)?;
                    Ok(Stmt::ClickRole {
                        role,
                        name,
                        exact,
                        nth,
                        timeout_ms,
                        span,
                    })
                } else {
                    let ref_id = self.expect_ref()?;
                    Ok(Stmt::ClickRef { ref_id, span })
                }
            }
            "fill" => {
                if self.eat_ident("role") {
                    let (role, _) = self.expect_string()?;
                    let first = self.expect_string()?.0;
                    let (name, text) = if matches!(self.peek_kind(), TokenKind::Str(_)) {
                        (Some(first), self.expect_string()?.0)
                    } else {
                        (None, first)
                    };
                    let (exact, nth, timeout_ms) = self.parse_match_modifiers(5_000, true)?;
                    Ok(Stmt::FillRole {
                        role,
                        name,
                        text,
                        exact,
                        nth,
                        timeout_ms,
                        span,
                    })
                } else {
                    let ref_id = self.expect_ref()?;
                    let (text, _) = self.expect_string()?;
                    Ok(Stmt::FillRef { ref_id, text, span })
                }
            }
            "dblclick" => {
                if self.eat_ident("text") {
                    let (text, _) = self.expect_string()?;
                    let (exact, nth, timeout_ms) = self.parse_match_modifiers(5_000, true)?;
                    Ok(Stmt::DblclickText {
                        text,
                        exact,
                        nth,
                        timeout_ms,
                        span,
                    })
                } else {
                    let ref_id = self.expect_ref()?;
                    Ok(Stmt::DblclickRef { ref_id, span })
                }
            }
            "hover" => {
                if self.eat_ident("text") {
                    let (text, _) = self.expect_string()?;
                    let (exact, nth, timeout_ms) = self.parse_match_modifiers(5_000, true)?;
                    Ok(Stmt::HoverText {
                        text,
                        exact,
                        nth,
                        timeout_ms,
                        span,
                    })
                } else {
                    let ref_id = self.expect_ref()?;
                    Ok(Stmt::HoverRef { ref_id, span })
                }
            }
            "press" => {
                let (key, _) = self.expect_string()?;
                Ok(Stmt::PressKey { key, span })
            }
            "type" => {
                let (text, _) = self.expect_string()?;
                Ok(Stmt::TypeText { text, span })
            }
            "reload" => {
                let ignore_cache = self.eat_ident("ignoreCache") || self.eat_ident("ignore_cache");
                Ok(Stmt::Reload { ignore_cache, span })
            }
            "expect" => {
                let (kind, k_span) = self.expect_ident()?;
                match kind.as_str() {
                    "visible" => {
                        let (text, _) = self.expect_string()?;
                        let (exact, _, timeout_ms) = self.parse_match_modifiers(5_000, false)?;
                        Ok(Stmt::ExpectVisible {
                            text,
                            exact,
                            timeout_ms,
                            span,
                        })
                    }
                    "hidden" => {
                        let (text, _) = self.expect_string()?;
                        let (exact, _, timeout_ms) = self.parse_match_modifiers(5_000, false)?;
                        Ok(Stmt::ExpectHidden {
                            text,
                            exact,
                            timeout_ms,
                            span,
                        })
                    }
                    "screenshot" => {
                        let (name, _) = self.expect_string()?;
                        let mut threshold = None;
                        let mut max_diff_ratio = None;
                        loop {
                            if self.eat_ident("threshold") {
                                threshold = Some(self.expect_f64()?.0);
                            } else if self.eat_ident("maxDiff") || self.eat_ident("max_diff") {
                                max_diff_ratio = Some(self.expect_f64()?.0);
                            } else {
                                break;
                            }
                        }
                        Ok(Stmt::ExpectScreenshot {
                            name,
                            max_diff_ratio,
                            threshold,
                            span,
                        })
                    }
                    other => Err(self.err(
                        k_span,
                        format!("unknown `expect` assertion `{other}` (use `visible`, `hidden`, or `screenshot`)"),
                    )),
                }
            }
            "wait" => {
                let ms = self.expect_duration_ms()?;
                Ok(Stmt::Wait { ms, span })
            }
            "cdp" => {
                let (method, _) = self.expect_string()?;
                let params = match self.peek_kind() {
                    TokenKind::LBrace | TokenKind::LBracket => self.parse_json_value()?,
                    TokenKind::Str(_) => {
                        let (raw_json, j_span) = self.expect_string()?;
                        serde_json::from_str(&raw_json).map_err(|err| {
                            self.err(j_span, format!("invalid JSON params for `cdp`: {err}"))
                        })?
                    }
                    _ => Value::Object(Map::new()),
                };
                Ok(Stmt::Cdp {
                    method,
                    params,
                    span,
                })
            }
            "mock" => self.parse_mock(span),
            other => Err(self.err(span, format!("unknown statement keyword `{other}`"))),
        }
    }

    fn parse_match_modifiers(
        &mut self,
        default_timeout_ms: u64,
        allow_nth: bool,
    ) -> Result<(bool, usize, u64), DslError> {
        let mut exact = false;
        let mut nth = 0usize;
        let mut timeout_ms = default_timeout_ms;
        loop {
            if self.eat_ident("exact") {
                exact = true;
            } else if allow_nth && self.eat_ident("nth") {
                let (val, _) = self.expect_u64_int()?;
                nth = val as usize;
            } else if self.eat_ident("timeout") {
                timeout_ms = self.expect_duration_ms()?;
            } else {
                break;
            }
        }
        Ok((exact, nth, timeout_ms))
    }

    fn expect_ref(&mut self) -> Result<String, DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Ref(id) => Ok(id),
            other => Err(self.err(
                tok.span,
                format!("expected element ref like `@e1`, found {other:?}"),
            )),
        }
    }

    fn expect_duration_ms(&mut self) -> Result<u64, DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::DurationMs(ms) => Ok(ms),
            TokenKind::Int(ms) if ms >= 0 => Ok(ms as u64),
            other => Err(self.err(
                tok.span,
                format!("expected duration like `500ms` or `15s`, found {other:?}"),
            )),
        }
    }

    fn parse_mock(&mut self, span: Span) -> Result<Stmt, DslError> {
        let (url_substring, _) = self.expect_string()?;
        let mut status: u16 = 200;
        let mut headers = HashMap::new();
        let mut patterns = Vec::new();
        let mut body: Option<MockBodyAst> = None;

        loop {
            if self.eat_ident("status") {
                let (code, code_span) = self.expect_u64_int()?;
                status = u16::try_from(code)
                    .map_err(|_| self.err(code_span, format!("invalid HTTP status code {code}")))?;
            } else if self.eat_ident("pattern") {
                let (pat, _) = self.expect_string()?;
                patterns.push(pat);
            } else {
                break;
            }
        }

        if *self.peek_kind() == TokenKind::LBrace {
            self.bump();
            while *self.peek_kind() != TokenKind::RBrace {
                if *self.peek_kind() == TokenKind::Eof {
                    return Err(self.err(self.peek_span(), "unclosed `mock` block"));
                }
                let (prop, prop_span) = self.expect_ident()?;
                match prop.as_str() {
                    "status" => {
                        let (code, code_span) = self.expect_u64_int()?;
                        status = u16::try_from(code).map_err(|_| {
                            self.err(code_span, format!("invalid HTTP status code {code}"))
                        })?;
                    }
                    "pattern" => {
                        let (pat, _) = self.expect_string()?;
                        patterns.push(pat);
                    }
                    "header" => {
                        let (key, _) = self.expect_string()?;
                        self.expect_kind(&TokenKind::Equals, "`=`")?;
                        let (val, _) = self.expect_string()?;
                        headers.insert(key, val);
                    }
                    "body" => {
                        if self.eat_ident("file") {
                            let (rel, _) = self.expect_string()?;
                            body = Some(MockBodyAst::File(rel));
                        } else {
                            let (inline, _) = self.expect_string()?;
                            body = Some(MockBodyAst::Inline(inline));
                        }
                    }
                    other => {
                        return Err(self.err(
                            prop_span,
                            format!(
                                "unknown `mock` property `{other}` (use `status`, `pattern`, `header`, or `body`)"
                            ),
                        ));
                    }
                }
            }
            self.bump(); // }
        }

        Ok(Stmt::Mock(MockStmt {
            url_substring,
            status,
            headers,
            patterns,
            body,
            span,
        }))
    }

    fn parse_json_value(&mut self) -> Result<Value, DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::LBrace => {
                let mut map = Map::new();
                while *self.peek_kind() != TokenKind::RBrace {
                    if *self.peek_kind() == TokenKind::Eof {
                        return Err(self.err(self.peek_span(), "unclosed JSON `{` object"));
                    }
                    let key = match self.bump() {
                        Token {
                            kind: TokenKind::Str(s) | TokenKind::Ident(s),
                            ..
                        } => s,
                        other => {
                            return Err(self.err(
                                other.span,
                                format!("expected JSON key, found {:?}", other.kind),
                            ));
                        }
                    };
                    self.expect_kind(&TokenKind::Colon, "`:`")?;
                    let val = self.parse_json_value()?;
                    map.insert(key, val);
                    if *self.peek_kind() == TokenKind::Comma {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.expect_kind(&TokenKind::RBrace, "`}`")?;
                Ok(Value::Object(map))
            }
            TokenKind::LBracket => {
                let mut arr = Vec::new();
                while *self.peek_kind() != TokenKind::RBracket {
                    if *self.peek_kind() == TokenKind::Eof {
                        return Err(self.err(self.peek_span(), "unclosed JSON `[` array"));
                    }
                    arr.push(self.parse_json_value()?);
                    if *self.peek_kind() == TokenKind::Comma {
                        self.bump();
                    } else {
                        break;
                    }
                }
                self.expect_kind(&TokenKind::RBracket, "`]`")?;
                Ok(Value::Array(arr))
            }
            TokenKind::Str(s) => Ok(Value::String(s)),
            TokenKind::Int(i) => Ok(Value::Number(i.into())),
            TokenKind::Float(f) => serde_json::Number::from_f64(f)
                .map(Value::Number)
                .ok_or_else(|| self.err(tok.span, "invalid JSON float")),
            TokenKind::Ident(id) => match id.as_str() {
                "true" => Ok(Value::Bool(true)),
                "false" => Ok(Value::Bool(false)),
                "null" => Ok(Value::Null),
                other => Err(self.err(tok.span, format!("unexpected JSON identifier `{other}`"))),
            },
            other => Err(self.err(tok.span, format!("expected JSON value, found {other:?}"))),
        }
    }
}
