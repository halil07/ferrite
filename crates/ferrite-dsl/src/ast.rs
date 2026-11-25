#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::PathBuf;

use ferrite_cdp::LoadState;
use serde_json::Value;

use crate::error::Span;

#[derive(Debug, Clone)]
pub struct Program {
    pub suite_name: Option<String>,
    pub imports: Vec<(String, Span)>,
    pub globals: Vec<(String, String, Span)>,
    pub before_each: Vec<Stmt>,
    pub macros: Vec<MacroDef>,
    pub tests: Vec<TestCaseAst>,
}

#[derive(Debug, Clone)]
pub struct MacroDef {
    pub name: String,
    pub params: Vec<String>,
    pub body: Vec<Stmt>,
    pub span: Span,
    pub source_dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct TestCaseAst {
    pub name: String,
    pub body: Vec<Stmt>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum MockBodyAst {
    Inline(String),
    File(String),
}

#[derive(Debug, Clone)]
pub struct MockStmt {
    pub url_substring: String,
    pub status: u16,
    pub headers: HashMap<String, String>,
    pub patterns: Vec<String>,
    pub body: Option<MockBodyAst>,
    pub span: Span,
}

#[derive(Debug, Clone)]
pub enum Stmt {
    Let {
        name: String,
        value: String,
        span: Span,
    },
    Tab {
        tab_id: i64,
        span: Span,
    },
    Use {
        macro_name: String,
        args: Vec<String>,
        span: Span,
    },
    Goto {
        url: String,
        wait_until: LoadState,
        timeout_ms: u64,
        span: Span,
    },
    Snapshot {
        interactive_only: bool,
        span: Span,
    },
    ClickRef {
        ref_id: String,
        span: Span,
    },
    ClickRole {
        role: String,
        name: Option<String>,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
        span: Span,
    },
    FillRef {
        ref_id: String,
        text: String,
        span: Span,
    },
    FillRole {
        role: String,
        name: Option<String>,
        text: String,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
        span: Span,
    },
    DblclickRef {
        ref_id: String,
        span: Span,
    },
    DblclickText {
        text: String,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
        span: Span,
    },
    HoverRef {
        ref_id: String,
        span: Span,
    },
    HoverText {
        text: String,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
        span: Span,
    },
    PressKey {
        key: String,
        span: Span,
    },
    TypeText {
        text: String,
        span: Span,
    },
    Reload {
        ignore_cache: bool,
        span: Span,
    },
    ExpectVisible {
        text: String,
        exact: bool,
        timeout_ms: u64,
        span: Span,
    },
    ExpectHidden {
        text: String,
        exact: bool,
        timeout_ms: u64,
        span: Span,
    },
    ExpectScreenshot {
        name: String,
        max_diff_ratio: Option<f64>,
        threshold: Option<f64>,
        span: Span,
    },
    Wait {
        ms: u64,
        span: Span,
    },
    Cdp {
        method: String,
        params: Value,
        span: Span,
    },
    Mock(MockStmt),
}
