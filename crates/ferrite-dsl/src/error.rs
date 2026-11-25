#![forbid(unsafe_code)]

use std::path::PathBuf;

use thiserror::Error;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Span {
    pub line: usize,
    pub col: usize,
}

#[derive(Debug, Error)]
pub enum DslError {
    #[error("{file}:{}:{}: lex error: {message}", span.line, span.col)]
    Lex {
        file: String,
        span: Span,
        message: String,
    },

    #[error("{file}:{}:{}: syntax error: {message}", span.line, span.col)]
    Parse {
        file: String,
        span: Span,
        message: String,
    },

    #[error("{file}:{}:{}: compile error: {message}", span.line, span.col)]
    Compile {
        file: String,
        span: Span,
        message: String,
    },

    #[error("circular import detected involving {0}")]
    CircularImport(PathBuf),

    #[error("I/O error reading {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },
}
