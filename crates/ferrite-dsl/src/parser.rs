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

#[derive(Debug, Clone)]
enum SelectorKind {
    Css(String),
    Text(String),
    Role { role: String, name: Option<String> },
    Ref(String),
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

    fn eat_kind(&mut self, expected: &TokenKind) -> bool {
        if self.peek_kind() == expected {
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

    fn expect_bool(&mut self) -> Result<(bool, Span), DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::Ident(ref s) if s == "true" => Ok((true, tok.span)),
            TokenKind::Ident(ref s) if s == "false" => Ok((false, tok.span)),
            other => Err(self.err(tok.span, format!("expected boolean, found {other:?}"))),
        }
    }

    fn expect_duration_ms(&mut self) -> Result<u64, DslError> {
        let tok = self.bump();
        match tok.kind {
            TokenKind::DurationMs(ms) => Ok(ms),
            TokenKind::Int(ms) if ms >= 0 => Ok(ms as u64),
            other => Err(self.err(
                tok.span,
                format!("expected duration like `500ms` or `5s`, found {other:?}"),
            )),
        }
    }

    /// Parses either `("str")` or `"str"`.
    fn expect_optional_paren_string(&mut self) -> Result<(String, Span), DslError> {
        if self.eat_kind(&TokenKind::LParen) {
            let s = self.expect_string()?;
            self.expect_kind(&TokenKind::RParen, "`)`")?;
            Ok(s)
        } else {
            self.expect_string()
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
                    let (name, _) = self.expect_optional_paren_string()?;
                    self.expect_kind(&TokenKind::Semicolon, "`;` after `suite` declaration")?;
                    if prog.suite_name.is_some() {
                        return Err(self.err(span, "duplicate `suite` declaration"));
                    }
                    prog.suite_name = Some(name);
                }
                "import" => {
                    let (path, p_span) = self.expect_optional_paren_string()?;
                    self.expect_kind(&TokenKind::Semicolon, "`;` after `import` statement")?;
                    prog.imports.push((path, p_span));
                }
                "let" | "const" => {
                    let (name, _) = self.expect_ident()?;
                    self.expect_kind(&TokenKind::Equals, "`=`")?;
                    let (val, val_span) = self.expect_scalar_string()?;
                    self.expect_kind(&TokenKind::Semicolon, "`;` after variable declaration")?;
                    prog.globals.push((name, val, val_span));
                }
                "beforeEach" => {
                    if self.eat_kind(&TokenKind::LParen) {
                        self.expect_kind(&TokenKind::RParen, "`)`")?;
                    }
                    if !prog.before_each.is_empty() {
                        return Err(self.err(span, "duplicate `beforeEach` block"));
                    }
                    prog.before_each = self.parse_block()?;
                }
                "macro" | "fn" | "function" => {
                    prog.macros.push(self.parse_macro(span)?);
                }
                "test" => {
                    let (name, _) = self.expect_optional_paren_string()?;
                    let body = self.parse_block()?;
                    prog.tests.push(TestCaseAst { name, body, span });
                }
                other => {
                    return Err(self.err(
                        span,
                        format!(
                            "unexpected top-level keyword `{other}` (expected `suite`, `import`, `let`, `beforeEach`, `macro`/`fn`, or `test`)"
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
            if !self.eat_kind(&TokenKind::Comma) {
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
            stmts.extend(self.parse_stmt()?);
            self.expect_kind(&TokenKind::Semicolon, "`;` at end of statement")?;
        }
        self.bump(); // }
        Ok(stmts)
    }

    fn parse_stmt(&mut self) -> Result<Vec<Stmt>, DslError> {
        // Optional `await` prefix for familiar async JS / jQuery readability.
        let _ = self.eat_ident("await");

        // Direct `@e1.click()` ref chain shorthand.
        if let TokenKind::Ref(ref_id) = self.peek_kind().clone() {
            let span = self.peek_span();
            self.bump();
            return self.parse_selector_chain(SelectorKind::Ref(ref_id), span);
        }

        let (kw, span) = self.expect_ident()?;
        match kw.as_str() {
            "$" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let sel = match self.peek_kind().clone() {
                    TokenKind::Ref(ref_id) => {
                        self.bump();
                        SelectorKind::Ref(ref_id)
                    }
                    _ => {
                        let (css, _) = self.expect_string()?;
                        SelectorKind::Css(css)
                    }
                };
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                self.parse_selector_chain(sel, span)
            }
            "$text" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let (text, _) = self.expect_string()?;
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                self.parse_selector_chain(SelectorKind::Text(text), span)
            }
            "$role" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let (role, _) = self.expect_string()?;
                let name = if self.eat_kind(&TokenKind::Comma) {
                    Some(self.expect_string()?.0)
                } else {
                    None
                };
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                self.parse_selector_chain(SelectorKind::Role { role, name }, span)
            }
            "let" | "const" => {
                let (name, _) = self.expect_ident()?;
                self.expect_kind(&TokenKind::Equals, "`=`")?;
                let (value, _) = self.expect_scalar_string()?;
                Ok(vec![Stmt::Let { name, value, span }])
            }
            "tab" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let tok = self.bump();
                let tab_id = match tok.kind {
                    TokenKind::Int(id) => id,
                    other => {
                        return Err(
                            self.err(tok.span, format!("expected tab ID integer, found {other:?}"))
                        );
                    }
                };
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::Tab { tab_id, span }])
            }
            "sleep" | "wait" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let ms = self.expect_duration_ms()?;
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::Wait { ms, span }])
            }
            "goto" => Ok(vec![self.parse_goto(span)?]),
            "reload" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let ignore_cache = if *self.peek_kind() != TokenKind::RParen {
                    self.expect_bool()?.0
                } else {
                    false
                };
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::Reload { ignore_cache, span }])
            }
            "press" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let (key, _) = self.expect_string()?;
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::PressKey { key, span }])
            }
            "type" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let (text, _) = self.expect_string()?;
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::TypeText { text, span }])
            }
            "snapshot" => {
                self.expect_kind(&TokenKind::LParen, "`(`")?;
                let interactive_only = if *self.peek_kind() != TokenKind::RParen {
                    let (mode, mode_span) = self.expect_string()?;
                    match mode.as_str() {
                        "full" => false,
                        "interactive" => true,
                        other => {
                            return Err(self.err(
                                mode_span,
                                format!("unknown snapshot mode `{other}` (use \"interactive\" or \"full\")"),
                            ));
                        }
                    }
                } else {
                    true
                };
                self.expect_kind(&TokenKind::RParen, "`)`")?;
                Ok(vec![Stmt::Snapshot {
                    interactive_only,
                    span,
                }])
            }
            "screenshot" => Ok(vec![self.parse_screenshot(span)?]),
            "console" => Ok(vec![self.parse_console(span)?]),
            "mock" => Ok(vec![self.parse_mock(span)?]),
            "cdp" => Ok(vec![self.parse_cdp(span)?]),
            "use" => {
                let (macro_name, _) = self.expect_ident()?;
                let args = self.parse_call_scalar_args()?;
                Ok(vec![Stmt::Use {
                    macro_name,
                    args,
                    span,
                }])
            }
            macro_name => {
                // Direct JS-style macro call: `await add_todo("Buy milk")`
                if *self.peek_kind() == TokenKind::LParen {
                    let args = self.parse_call_scalar_args()?;
                    Ok(vec![Stmt::Use {
                        macro_name: macro_name.to_string(),
                        args,
                        span,
                    }])
                } else {
                    Err(self.err(
                        span,
                        format!(
                            "unexpected statement `{macro_name}`; use jQuery selectors like `$(\"...\").click()`, built-ins like `goto(...)` / `sleep(5s)`, or call a macro `{macro_name}(...)`"
                        ),
                    ))
                }
            }
        }
    }

    fn parse_call_scalar_args(&mut self) -> Result<Vec<String>, DslError> {
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let mut args = Vec::new();
        while *self.peek_kind() != TokenKind::RParen {
            let (arg, _) = self.expect_scalar_string()?;
            args.push(arg);
            if !self.eat_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(args)
    }

    fn parse_selector_chain(
        &mut self,
        selector: SelectorKind,
        span: Span,
    ) -> Result<Vec<Stmt>, DslError> {
        let mut nth: usize = 0;
        let mut exact = false;
        let mut timeout_ms: u64 = 5_000;
        let mut in_expect = false;
        let mut expect_emitted = false;
        let mut emitted: Vec<Stmt> = Vec::new();

        while self.eat_kind(&TokenKind::Dot) {
            let (method, m_span) = self.expect_ident()?;
            self.expect_kind(&TokenKind::LParen, "`(`")?;

            match method.as_str() {
                "expect" => {
                    if in_expect {
                        return Err(self.err(m_span, "duplicate `.expect()` in selector chain"));
                    }
                    timeout_ms = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    in_expect = true;
                    expect_emitted = false;
                }
                "eq" | "nth" => {
                    if in_expect {
                        return Err(self.err(
                            m_span,
                            "call `.eq()` before `.expect()` (e.g. `$(\"...\").eq(0).expect().visible()`)",
                        ));
                    }
                    let (idx, _) = self.expect_u64_int()?;
                    nth = idx as usize;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                }
                "first" => {
                    if in_expect {
                        return Err(self.err(
                            m_span,
                            "call `.first()` before `.expect()` (e.g. `$(\"...\").first().expect().visible()`)",
                        ));
                    }
                    nth = 0;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                }
                "exact" => {
                    if in_expect {
                        return Err(self.err(
                            m_span,
                            "call `.exact()` before `.expect()` (e.g. `$text(\"...\").exact().expect().visible()`)",
                        ));
                    }
                    exact = if *self.peek_kind() != TokenKind::RParen {
                        self.expect_bool()?.0
                    } else {
                        true
                    };
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                }
                "timeout" => {
                    timeout_ms = self.expect_duration_ms()?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                }
                "click" | "dblclick" | "hover" | "fill" | "val" | "press" | "type" | "sleep"
                | "wait"
                    if in_expect =>
                {
                    return Err(self.err(
                        m_span,
                        format!(
                            "action `.{method}()` cannot be called after `.expect()`; call `.{method}()` directly on the selector"
                        ),
                    ));
                }
                "click" => {
                    let t = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::ClickCss {
                            selector: css.clone(),
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Role { role, name } => emitted.push(Stmt::ClickRole {
                            role: role.clone(),
                            name: name.clone(),
                            exact,
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Ref(ref_id) => emitted.push(Stmt::ClickRef {
                            ref_id: ref_id.clone(),
                            span: m_span,
                        }),
                        SelectorKind::Text(_) => {
                            return Err(self.err(
                                m_span,
                                "`.click()` is supported on `$(\"css\")`, `$role(...)`, and `$(@ref)` (for text use `.dblclick()`, `.hover()`, or `.expect().visible()` / `.expect().hidden()`)",
                            ));
                        }
                    }
                }
                "dblclick" => {
                    let t = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::DblclickCss {
                            selector: css.clone(),
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Text(text) => emitted.push(Stmt::DblclickText {
                            text: text.clone(),
                            exact,
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Ref(ref_id) => emitted.push(Stmt::DblclickRef {
                            ref_id: ref_id.clone(),
                            span: m_span,
                        }),
                        SelectorKind::Role { .. } => {
                            return Err(self.err(
                                m_span,
                                "`.dblclick()` is supported on `$(\"css\")`, `$text(...)`, and `$(@ref)`",
                            ));
                        }
                    }
                }
                "hover" => {
                    let t = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::HoverCss {
                            selector: css.clone(),
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Text(text) => emitted.push(Stmt::HoverText {
                            text: text.clone(),
                            exact,
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Ref(ref_id) => emitted.push(Stmt::HoverRef {
                            ref_id: ref_id.clone(),
                            span: m_span,
                        }),
                        SelectorKind::Role { .. } => {
                            return Err(self.err(
                                m_span,
                                "`.hover()` is supported on `$(\"css\")`, `$text(...)`, and `$(@ref)`",
                            ));
                        }
                    }
                }
                "fill" | "val" => {
                    let (text, _) = self.expect_string()?;
                    let t = if self.eat_kind(&TokenKind::Comma) {
                        self.expect_duration_ms()?
                    } else {
                        timeout_ms
                    };
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::FillCss {
                            selector: css.clone(),
                            text,
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Role { role, name } => emitted.push(Stmt::FillRole {
                            role: role.clone(),
                            name: name.clone(),
                            text,
                            exact,
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Ref(ref_id) => emitted.push(Stmt::FillRef {
                            ref_id: ref_id.clone(),
                            text,
                            span: m_span,
                        }),
                        SelectorKind::Text(_) => {
                            return Err(self.err(
                                m_span,
                                "`.fill()` / `.val()` is supported on `$(\"css\")`, `$role(...)`, and `$(@ref)`",
                            ));
                        }
                    }
                }
                "press" => {
                    let (key, _) = self.expect_string()?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    emitted.push(Stmt::PressKey { key, span: m_span });
                }
                "type" => {
                    let (text, _) = self.expect_string()?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    emitted.push(Stmt::TypeText { text, span: m_span });
                }
                "sleep" | "wait" => {
                    let ms = self.expect_duration_ms()?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    emitted.push(Stmt::Wait { ms, span: m_span });
                }
                "visible" | "shouldBeVisible" | "isVisible" | "hidden" | "shouldBeHidden"
                | "isHidden" | "count" | "shouldHaveCount"
                    if !in_expect =>
                {
                    return Err(self.err(
                        m_span,
                        format!(
                            "assertion `.{method}()` must be called after `.expect()` (e.g. `.expect().{method}()`)"
                        ),
                    ));
                }
                "visible" | "shouldBeVisible" | "isVisible" => {
                    let t = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    expect_emitted = true;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::ExpectCssVisible {
                            selector: css.clone(),
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Text(text) => emitted.push(Stmt::ExpectVisible {
                            text: text.clone(),
                            exact,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        _ => {
                            return Err(self.err(
                                m_span,
                                "`.expect().visible()` is supported on `$(\"css\")` and `$text(\"...\")`",
                            ));
                        }
                    }
                }
                "hidden" | "shouldBeHidden" | "isHidden" => {
                    let t = self.parse_optional_timeout_arg(timeout_ms)?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    expect_emitted = true;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::ExpectCssHidden {
                            selector: css.clone(),
                            nth,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        SelectorKind::Text(text) => emitted.push(Stmt::ExpectHidden {
                            text: text.clone(),
                            exact,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        _ => {
                            return Err(self.err(
                                m_span,
                                "`.expect().hidden()` is supported on `$(\"css\")` and `$text(\"...\")`",
                            ));
                        }
                    }
                }
                "count" | "shouldHaveCount" => {
                    let (expected, _) = self.expect_u64_int()?;
                    let t = if self.eat_kind(&TokenKind::Comma) {
                        self.expect_duration_ms()?
                    } else {
                        timeout_ms
                    };
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    expect_emitted = true;
                    match &selector {
                        SelectorKind::Css(css) => emitted.push(Stmt::ExpectCssCount {
                            selector: css.clone(),
                            expected: expected as usize,
                            timeout_ms: t,
                            span: m_span,
                        }),
                        _ => {
                            return Err(self.err(
                                m_span,
                                "`.expect().count(n)` is only supported on CSS selectors `$(\"...\")`",
                            ));
                        }
                    }
                }
                other => {
                    let hint = if in_expect {
                        "expected assertion after `.expect()`: `.visible()`, `.hidden()`, or `.count(n)`"
                    } else {
                        "use `.eq()`, `.first()`, `.exact()`, `.timeout()`, `.click()`, `.dblclick()`, `.hover()`, `.fill()`/`.val()`, `.press()`, `.type()`, or `.expect()`"
                    };
                    return Err(self.err(
                        m_span,
                        format!("unknown selector method `.{other}()` ({hint})"),
                    ));
                }
            }
        }

        if in_expect && !expect_emitted {
            return Err(self.err(
                span,
                "`.expect()` must be followed by an assertion like `.visible()`, `.hidden()`, or `.count(n)`",
            ));
        }

        if emitted.is_empty() {
            return Err(self.err(
                span,
                "selector must be followed by an action (`.click()`, `.dblclick()`, `.fill(\"...\")`) or `.expect().visible()` / `.expect().hidden()` / `.expect().count(n)`",
            ));
        }

        Ok(emitted)
    }

    fn parse_optional_timeout_arg(&mut self, default_ms: u64) -> Result<u64, DslError> {
        if *self.peek_kind() != TokenKind::RParen {
            self.expect_duration_ms()
        } else {
            Ok(default_ms)
        }
    }

    fn parse_goto(&mut self, span: Span) -> Result<Stmt, DslError> {
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let (url, _) = self.expect_string()?;
        let mut wait_until = LoadState::Load;
        let mut timeout_ms = 15_000u64;

        if self.eat_kind(&TokenKind::Comma) {
            match self.peek_kind() {
                TokenKind::LBrace => {
                    let opts = self.parse_options_object()?;
                    if let Some((w, w_span)) = opts.get("wait") {
                        wait_until = self.parse_load_state_val(w, *w_span)?;
                    }
                    if let Some((t, t_span)) = opts.get("timeout") {
                        timeout_ms = self.parse_duration_val(t, *t_span)?;
                    }
                }
                TokenKind::Str(_) => {
                    let (mode, m_span) = self.expect_string()?;
                    wait_until = self.parse_load_state_str(&mode, m_span)?;
                    if self.eat_kind(&TokenKind::Comma) {
                        timeout_ms = self.expect_duration_ms()?;
                    }
                }
                TokenKind::DurationMs(_) | TokenKind::Int(_) => {
                    timeout_ms = self.expect_duration_ms()?;
                }
                other => {
                    return Err(self.err(
                        self.peek_span(),
                        format!(
                            "unexpected `goto` option {other:?} (expected wait state string, duration, or `{{ wait, timeout }}`)"
                        ),
                    ));
                }
            }
        }

        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(Stmt::Goto {
            url,
            wait_until,
            timeout_ms,
            span,
        })
    }

    fn parse_load_state_str(&self, mode: &str, span: Span) -> Result<LoadState, DslError> {
        match mode.to_ascii_lowercase().as_str() {
            "load" => Ok(LoadState::Load),
            "domcontentloaded" => Ok(LoadState::DomContentLoaded),
            "commit" => Ok(LoadState::Commit),
            other => Err(self.err(
                span,
                format!("unknown wait state `{other}` (use \"load\", \"domcontentloaded\", or \"commit\")"),
            )),
        }
    }

    fn parse_load_state_val(&self, val: &TokenKind, span: Span) -> Result<LoadState, DslError> {
        match val {
            TokenKind::Str(s) | TokenKind::Ident(s) => self.parse_load_state_str(s, span),
            other => Err(self.err(span, format!("expected wait state string, found {other:?}"))),
        }
    }

    fn parse_duration_val(&self, val: &TokenKind, span: Span) -> Result<u64, DslError> {
        match val {
            TokenKind::DurationMs(ms) => Ok(*ms),
            TokenKind::Int(ms) if *ms >= 0 => Ok(*ms as u64),
            other => Err(self.err(span, format!("expected duration, found {other:?}"))),
        }
    }

    fn parse_f64_val(&self, val: &TokenKind, span: Span) -> Result<f64, DslError> {
        match val {
            TokenKind::Float(f) => Ok(*f),
            TokenKind::Int(i) => Ok(*i as f64),
            other => Err(self.err(span, format!("expected number, found {other:?}"))),
        }
    }

    fn parse_options_object(&mut self) -> Result<HashMap<String, (TokenKind, Span)>, DslError> {
        self.expect_kind(&TokenKind::LBrace, "`{`")?;
        let mut map = HashMap::new();
        while *self.peek_kind() != TokenKind::RBrace {
            if *self.peek_kind() == TokenKind::Eof {
                return Err(self.err(self.peek_span(), "unclosed `{` options object"));
            }
            let key = match self.bump() {
                Token {
                    kind: TokenKind::Ident(k) | TokenKind::Str(k),
                    ..
                } => k,
                other => {
                    return Err(
                        self.err(other.span, format!("expected option key, found {:?}", other.kind))
                    );
                }
            };
            self.expect_kind(&TokenKind::Colon, "`:`")?;
            let val = self.bump();
            map.insert(key, (val.kind, val.span));
            if !self.eat_kind(&TokenKind::Comma) {
                break;
            }
        }
        self.expect_kind(&TokenKind::RBrace, "`}`")?;
        Ok(map)
    }

    fn parse_screenshot(&mut self, span: Span) -> Result<Stmt, DslError> {
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let (name, _) = self.expect_string()?;
        let mut threshold = None;
        let mut max_diff_ratio = None;

        if self.eat_kind(&TokenKind::Comma) {
            if *self.peek_kind() == TokenKind::LBrace {
                let opts = self.parse_options_object()?;
                if let Some((t, s)) = opts.get("threshold") {
                    threshold = Some(self.parse_f64_val(t, *s)?);
                }
                if let Some((m, s)) = opts.get("maxDiff").or_else(|| opts.get("max_diff")) {
                    max_diff_ratio = Some(self.parse_f64_val(m, *s)?);
                }
            } else {
                threshold = Some(self.expect_f64()?.0);
                if self.eat_kind(&TokenKind::Comma) {
                    max_diff_ratio = Some(self.expect_f64()?.0);
                }
            }
        }

        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(Stmt::ExpectScreenshot {
            name,
            max_diff_ratio,
            threshold,
            span,
        })
    }

    fn parse_console(&mut self, span: Span) -> Result<Stmt, DslError> {
        self.expect_kind(&TokenKind::Dot, "`.`")?;
        let (sub, sub_span) = self.expect_ident()?;
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let stmt = match sub.as_str() {
            "start" => Stmt::ConsoleStart { span },
            "clear" => Stmt::ConsoleClear { span },
            "noErrors" | "no_errors" => Stmt::ExpectNoConsoleErrors { span },
            "expect" => {
                let (text, _) = self.expect_string()?;
                let mut level = None;
                let mut timeout_ms = 5_000u64;
                if self.eat_kind(&TokenKind::Comma) {
                    if matches!(self.peek_kind(), TokenKind::Str(_)) {
                        level = Some(self.expect_string()?.0);
                        if self.eat_kind(&TokenKind::Comma) {
                            timeout_ms = self.expect_duration_ms()?;
                        }
                    } else {
                        timeout_ms = self.expect_duration_ms()?;
                    }
                }
                Stmt::ExpectConsole {
                    text,
                    level,
                    timeout_ms,
                    span,
                }
            }
            other => {
                return Err(self.err(
                    sub_span,
                    format!(
                        "unknown `console.{other}()` method (use `console.start()`, `console.clear()`, `console.noErrors()`, or `console.expect(...)`)"
                    ),
                ));
            }
        };
        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(stmt)
    }

    fn parse_cdp(&mut self, span: Span) -> Result<Stmt, DslError> {
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let (method, _) = self.expect_string()?;
        let params = if self.eat_kind(&TokenKind::Comma) {
            self.parse_json_value()?
        } else {
            Value::Object(Map::new())
        };
        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(Stmt::Cdp {
            method,
            params,
            span,
        })
    }

    fn parse_mock(&mut self, span: Span) -> Result<Stmt, DslError> {
        if self.eat_kind(&TokenKind::Dot) {
            let (sub, sub_span) = self.expect_ident()?;
            match sub.as_str() {
                "clear" => {
                    self.expect_kind(&TokenKind::LParen, "`(`")?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    return Ok(Stmt::MockClear { span });
                }
                "load" => {
                    self.expect_kind(&TokenKind::LParen, "`(`")?;
                    let (file_path, _) = self.expect_string()?;
                    self.expect_kind(&TokenKind::RParen, "`)`")?;
                    return Ok(Stmt::MockLoad { file_path, span });
                }
                "when" => {
                    return self.parse_mock_when_chain(span);
                }
                other => {
                    return Err(self.err(
                        sub_span,
                        format!(
                            "unknown `mock.{other}()` method (use `mock.when(...)`, `mock.load(\"...\")`, or `mock.clear()`)"
                        ),
                    ));
                }
            }
        }

        // Concise shorthand: `mock("GET", "real_service", "mock.json"[, 201])`
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let (first, _) = self.expect_string()?;
        let mut method: Option<String> = None;
        let url_substring: String;
        let mut params = HashMap::new();
        let mut data: Option<Value> = None;
        let mut status: u16 = 200;
        let mut headers = HashMap::new();
        let mut patterns = Vec::new();
        let mut body: Option<MockBodyAst> = None;
        let mut delay_ms: u64 = 0;
        let mut fail_reason: Option<String> = None;

        let first_upper = first.to_ascii_uppercase();
        let is_http_method = matches!(
            first_upper.as_str(),
            "GET" | "POST" | "PUT" | "PATCH" | "DELETE" | "HEAD" | "OPTIONS" | "ANY"
        );

        if is_http_method && self.eat_kind(&TokenKind::Comma) {
            if first_upper != "ANY" {
                method = Some(first_upper);
            }
            url_substring = self.expect_string()?.0;
        } else {
            url_substring = first;
        }

        if self.eat_kind(&TokenKind::Comma) {
            if *self.peek_kind() == TokenKind::LBrace {
                self.parse_mock_options_obj(
                    &mut params,
                    &mut data,
                    &mut status,
                    &mut headers,
                    &mut patterns,
                    &mut body,
                    &mut delay_ms,
                    &mut fail_reason,
                )?;
            } else {
                let (rel_file, _) = self.expect_string()?;
                body = Some(MockBodyAst::File(rel_file));
                if self.eat_kind(&TokenKind::Comma) {
                    if *self.peek_kind() == TokenKind::LBrace {
                        self.parse_mock_options_obj(
                            &mut params,
                            &mut data,
                            &mut status,
                            &mut headers,
                            &mut patterns,
                            &mut body,
                            &mut delay_ms,
                            &mut fail_reason,
                        )?;
                    } else {
                        let (code, code_span) = self.expect_u64_int()?;
                        status = u16::try_from(code).map_err(|_| {
                            self.err(code_span, format!("invalid HTTP status code {code}"))
                        })?;
                    }
                }
            }
        }

        self.expect_kind(&TokenKind::RParen, "`)`")?;
        Ok(Stmt::Mock(MockStmt {
            url_substring,
            method,
            params,
            data,
            status,
            headers,
            patterns,
            body,
            delay_ms,
            fail_reason,
            span,
        }))
    }

    fn parse_mock_when_chain(&mut self, span: Span) -> Result<Stmt, DslError> {
        self.expect_kind(&TokenKind::LParen, "`(`")?;
        let (first, _) = self.expect_string()?;
        let (method, url_substring) = if self.eat_kind(&TokenKind::Comma) {
            let upper = first.to_ascii_uppercase();
            let m = if upper == "ANY" || upper == "*" {
                None
            } else {
                Some(upper)
            };
            (m, self.expect_string()?.0)
        } else {
            (None, first)
        };
        self.expect_kind(&TokenKind::RParen, "`)`")?;

        let mut params = HashMap::new();
        let mut data: Option<Value> = None;
        let mut status: u16 = 200;
        let mut headers = HashMap::new();
        let mut patterns = Vec::new();
        let mut body: Option<MockBodyAst> = None;
        let mut delay_ms: u64 = 0;
        let mut fail_reason: Option<String> = None;
        let mut has_terminal = false;

        while self.eat_kind(&TokenKind::Dot) {
            let (m_name, m_span) = self.expect_ident()?;
            self.expect_kind(&TokenKind::LParen, "`(`")?;
            match m_name.as_str() {
                "params" => {
                    let obj = self.parse_json_value()?;
                    let Some(map) = obj.as_object() else {
                        return Err(self.err(m_span, "`.params(...)` expects an object `{ key: value }`"));
                    };
                    for (k, v) in map {
                        let s = match v {
                            Value::String(s) => s.clone(),
                            other => other.to_string(),
                        };
                        params.insert(k.clone(), s);
                    }
                }
                "data" => {
                    data = Some(self.parse_json_value()?);
                }
                "delay" => {
                    delay_ms = self.expect_duration_ms()?;
                }
                "pattern" => {
                    let (pat, _) = self.expect_string()?;
                    patterns.push(pat);
                }
                "header" => {
                    let (k, _) = self.expect_string()?;
                    self.expect_kind(&TokenKind::Comma, "`,`")?;
                    let (v, _) = self.expect_string()?;
                    headers.insert(k, v);
                }
                "headers" => {
                    let obj = self.parse_json_value()?;
                    let Some(map) = obj.as_object() else {
                        return Err(self.err(m_span, "`.headers(...)` expects an object `{ key: value }`"));
                    };
                    for (k, v) in map {
                        if let Some(s) = v.as_str() {
                            headers.insert(k.clone(), s.to_string());
                        }
                    }
                }
                "reply" => {
                    has_terminal = true;
                    match self.peek_kind() {
                        TokenKind::Int(_) => {
                            let (code, code_span) = self.expect_u64_int()?;
                            status = u16::try_from(code).map_err(|_| {
                                self.err(code_span, format!("invalid HTTP status code {code}"))
                            })?;
                            if self.eat_kind(&TokenKind::Comma) {
                                let (rel_file, _) = self.expect_string()?;
                                body = Some(MockBodyAst::File(rel_file));
                            }
                        }
                        TokenKind::Str(_) => {
                            let (rel_file, _) = self.expect_string()?;
                            body = Some(MockBodyAst::File(rel_file));
                        }
                        other => {
                            return Err(self.err(
                                m_span,
                                format!(
                                    "expected status code or response file path in `.reply(...)`, found {other:?}"
                                ),
                            ));
                        }
                    }
                }
                "replyJson" => {
                    has_terminal = true;
                    let (code, code_span) = self.expect_u64_int()?;
                    status = u16::try_from(code).map_err(|_| {
                        self.err(code_span, format!("invalid HTTP status code {code}"))
                    })?;
                    self.expect_kind(&TokenKind::Comma, "`,`")?;
                    let val = self.parse_json_value()?;
                    if !headers
                        .keys()
                        .any(|k| k.eq_ignore_ascii_case("content-type"))
                    {
                        headers.insert(
                            "content-type".to_string(),
                            "application/json; charset=utf-8".to_string(),
                        );
                    }
                    body = Some(MockBodyAst::Inline(val.to_string()));
                }
                "fail" => {
                    has_terminal = true;
                    let reason = if *self.peek_kind() != TokenKind::RParen {
                        self.expect_string()?.0
                    } else {
                        "Failed".to_string()
                    };
                    fail_reason = Some(reason);
                }
                "abort" => {
                    has_terminal = true;
                    fail_reason = Some("Aborted".to_string());
                }
                other => {
                    return Err(self.err(
                        m_span,
                        format!(
                            "unknown `mock.when(...)` method `.{other}()` (use `.params()`, `.data()`, `.reply()`, `.delay()`, `.header()`, `.headers()`, `.pattern()`, `.fail()`, or `.abort()`)"
                        ),
                    ));
                }
            }
            self.expect_kind(&TokenKind::RParen, "`)`")?;
        }

        if !has_terminal {
            return Err(self.err(
                span,
                "`mock.when(...)` must end with `.reply(...)`, `.fail(...)`, or `.abort()`",
            ));
        }

        Ok(Stmt::Mock(MockStmt {
            url_substring,
            method,
            params,
            data,
            status,
            headers,
            patterns,
            body,
            delay_ms,
            fail_reason,
            span,
        }))
    }

    #[allow(clippy::too_many_arguments)]
    fn parse_mock_options_obj(
        &mut self,
        params: &mut HashMap<String, String>,
        data: &mut Option<Value>,
        status: &mut u16,
        headers: &mut HashMap<String, String>,
        patterns: &mut Vec<String>,
        body: &mut Option<MockBodyAst>,
        delay_ms: &mut u64,
        fail_reason: &mut Option<String>,
    ) -> Result<(), DslError> {
        let obj = self.parse_json_value()?;
        let Some(map) = obj.as_object() else {
            return Err(self.err(self.peek_span(), "expected mock options `{ ... }` object"));
        };
        for (k, v) in map {
            match k.as_str() {
                "params" => {
                    if let Some(pmap) = v.as_object() {
                        for (pk, pv) in pmap {
                            let s = match pv {
                                Value::String(s) => s.clone(),
                                other => other.to_string(),
                            };
                            params.insert(pk.clone(), s);
                        }
                    }
                }
                "data" => {
                    *data = Some(v.clone());
                }
                "status" => {
                    let code = v.as_u64().ok_or_else(|| {
                        self.err(self.peek_span(), "mock `status` must be an integer")
                    })?;
                    *status = u16::try_from(code).map_err(|_| {
                        self.err(self.peek_span(), format!("invalid HTTP status code {code}"))
                    })?;
                }
                "delayMs" | "delay" => {
                    *delay_ms = v.as_u64().ok_or_else(|| {
                        self.err(self.peek_span(), "mock `delayMs` must be a non-negative integer")
                    })?;
                }
                "fail" => {
                    *fail_reason = v.as_str().map(str::to_string);
                }
                "pattern" => {
                    if let Some(s) = v.as_str() {
                        patterns.push(s.to_string());
                    }
                }
                "patterns" => {
                    if let Some(arr) = v.as_array() {
                        for item in arr {
                            if let Some(s) = item.as_str() {
                                patterns.push(s.to_string());
                            }
                        }
                    }
                }
                "headers" => {
                    if let Some(hmap) = v.as_object() {
                        for (hk, hv) in hmap {
                            if let Some(hs) = hv.as_str() {
                                headers.insert(hk.clone(), hs.to_string());
                            }
                        }
                    }
                }
                "body" => {
                    if let Some(s) = v.as_str() {
                        *body = Some(MockBodyAst::Inline(s.to_string()));
                    } else {
                        *body = Some(MockBodyAst::Inline(v.to_string()));
                    }
                }
                "response" | "bodyFile" | "file" => {
                    let rel = v.as_str().ok_or_else(|| {
                        self.err(self.peek_span(), "mock `response`/`file` must be a string path")
                    })?;
                    *body = Some(MockBodyAst::File(rel.to_string()));
                }
                other => {
                    return Err(self.err(
                        self.peek_span(),
                        format!(
                            "unknown mock option `{other}` (use `params`, `data`, `status`, `delayMs`, `fail`, `pattern`, `patterns`, `headers`, `body`, or `response`)"
                        ),
                    ));
                }
            }
        }
        Ok(())
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
                    if !self.eat_kind(&TokenKind::Comma) {
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
                    if !self.eat_kind(&TokenKind::Comma) {
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
