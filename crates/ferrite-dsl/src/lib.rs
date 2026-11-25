#![forbid(unsafe_code)]

//! `.ferrite` E2E test DSL lexer, parser, macro/import compiler, and project configuration loader.

pub mod ast;
pub mod compiler;
pub mod error;
pub mod lexer;
pub mod parser;
pub mod project;

pub use compiler::{compile_file, compile_source};
pub use error::{DslError, Span};
pub use project::{ProjectConfig, compile_path, default_project_scaffold, load_project_config};
