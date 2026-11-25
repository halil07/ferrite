#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::fs;
use std::path::{Path, PathBuf};

use ferrite_cdp::ConsoleLevel;
use ferrite_daemon::{TestCase, TestStep, TestSuite, is_blocked_cdp_method};
use serde_json::Value;

use crate::ast::{MacroDef, MockBodyAst, Stmt};
use crate::error::{DslError, Span};
use crate::lexer::ESCAPED_DOLLAR_SENTINEL;
use crate::parser::parse_program;

const MAX_MACRO_DEPTH: usize = 16;

#[derive(Default)]
struct ModuleSymbols {
    globals: HashMap<String, String>,
    macros: HashMap<String, MacroDef>,
    visited: HashSet<PathBuf>,
    visiting: HashSet<PathBuf>,
}

pub fn compile_file(path: &Path) -> Result<TestSuite, DslError> {
    let canonical = fs::canonicalize(path).map_err(|source| DslError::Io {
        path: path.to_path_buf(),
        source,
    })?;
    let src = fs::read_to_string(&canonical).map_err(|source| DslError::Io {
        path: canonical.clone(),
        source,
    })?;
    let base_dir = canonical
        .parent()
        .unwrap_or_else(|| Path::new("."))
        .to_path_buf();
    let file_label = canonical.to_string_lossy().into_owned();
    let default_name = canonical
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ferrite-suite")
        .to_string();

    let mut symbols = ModuleSymbols::default();
    symbols.visiting.insert(canonical.clone());
    compile_with_symbols(&file_label, &base_dir, &default_name, &src, &mut symbols)
}

pub fn compile_source(file_name: &str, base_dir: &Path, src: &str) -> Result<TestSuite, DslError> {
    let default_name = Path::new(file_name)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or("ferrite-suite")
        .to_string();
    let mut symbols = ModuleSymbols::default();
    compile_with_symbols(file_name, base_dir, &default_name, src, &mut symbols)
}

fn load_imported_file(
    rel_path: &str,
    from_dir: &Path,
    symbols: &mut ModuleSymbols,
) -> Result<(), DslError> {
    let joined = from_dir.join(rel_path);
    let canonical = fs::canonicalize(&joined).map_err(|source| DslError::Io {
        path: joined,
        source,
    })?;

    if symbols.visiting.contains(&canonical) {
        return Err(DslError::CircularImport(canonical));
    }
    if symbols.visited.contains(&canonical) {
        return Ok(());
    }

    symbols.visiting.insert(canonical.clone());
    let src = fs::read_to_string(&canonical).map_err(|source| DslError::Io {
        path: canonical.clone(),
        source,
    })?;
    let dir = canonical.parent().unwrap_or_else(|| Path::new("."));
    let label = canonical.to_string_lossy().into_owned();
    let prog = parse_program(&label, dir, &src)?;

    for (imp_rel, _) in &prog.imports {
        load_imported_file(imp_rel, dir, symbols)?;
    }

    for (name, raw_val, val_span) in &prog.globals {
        let resolved = interpolate(raw_val, &symbols.globals, &label, *val_span)?;
        symbols.globals.insert(name.clone(), resolved);
    }

    for m in prog.macros {
        symbols.macros.insert(m.name.clone(), m);
    }

    symbols.visiting.remove(&canonical);
    symbols.visited.insert(canonical);
    Ok(())
}

fn compile_with_symbols(
    file: &str,
    base_dir: &Path,
    default_name: &str,
    src: &str,
    symbols: &mut ModuleSymbols,
) -> Result<TestSuite, DslError> {
    let prog = parse_program(file, base_dir, src)?;

    for (imp_rel, _) in &prog.imports {
        load_imported_file(imp_rel, base_dir, symbols)?;
    }

    for (name, raw_val, val_span) in &prog.globals {
        let resolved = interpolate(raw_val, &symbols.globals, file, *val_span)?;
        symbols.globals.insert(name.clone(), resolved);
    }

    for m in prog.macros {
        symbols.macros.insert(m.name.clone(), m);
    }

    let suite_name = match prog.suite_name {
        Some(raw) => interpolate(&raw, &symbols.globals, file, Span { line: 1, col: 1 })?,
        None => default_name.to_string(),
    };

    let mut cases = Vec::with_capacity(prog.tests.len());
    for tc in prog.tests {
        let mut scope = symbols.globals.clone();
        let mut tab_id: Option<i64> = None;
        let mut steps = Vec::new();

        expand_stmts(
            &prog.before_each,
            file,
            base_dir,
            &symbols.macros,
            &mut scope,
            &mut tab_id,
            &mut steps,
            0,
        )?;
        expand_stmts(
            &tc.body,
            file,
            base_dir,
            &symbols.macros,
            &mut scope,
            &mut tab_id,
            &mut steps,
            0,
        )?;

        let case_name = interpolate(&tc.name, &scope, file, tc.span)?;
        cases.push(TestCase {
            name: case_name,
            tab_id,
            steps,
        });
    }

    Ok(TestSuite {
        name: suite_name,
        cases,
    })
}

#[allow(clippy::too_many_arguments)]
fn expand_stmts(
    stmts: &[Stmt],
    file: &str,
    source_dir: &Path,
    macros: &HashMap<String, MacroDef>,
    scope: &mut HashMap<String, String>,
    tab_id: &mut Option<i64>,
    out_steps: &mut Vec<TestStep>,
    depth: usize,
) -> Result<(), DslError> {
    for stmt in stmts {
        match stmt {
            Stmt::Let { name, value, span } => {
                let resolved = interpolate(value, scope, file, *span)?;
                scope.insert(name.clone(), resolved);
            }
            Stmt::Tab { tab_id: id, .. } => {
                *tab_id = Some(*id);
            }
            Stmt::Use {
                macro_name,
                args,
                span,
            } => {
                if depth >= MAX_MACRO_DEPTH {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "macro expansion depth exceeded {MAX_MACRO_DEPTH} calling `{macro_name}`"
                        ),
                    });
                }
                let def = macros.get(macro_name).ok_or_else(|| DslError::Compile {
                    file: file.to_string(),
                    span: *span,
                    message: format!("undefined macro `{macro_name}`"),
                })?;
                if def.params.len() != args.len() {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "macro `{macro_name}` expects {} argument(s), got {}",
                            def.params.len(),
                            args.len()
                        ),
                    });
                }

                let mut macro_scope = scope.clone();
                for (param, raw_arg) in def.params.iter().zip(args.iter()) {
                    let val = interpolate(raw_arg, scope, file, *span)?;
                    macro_scope.insert(param.clone(), val);
                }
                expand_stmts(
                    &def.body,
                    file,
                    &def.source_dir,
                    macros,
                    &mut macro_scope,
                    tab_id,
                    out_steps,
                    depth + 1,
                )?;
            }
            Stmt::Goto {
                url,
                wait_until,
                timeout_ms,
                span,
            } => {
                out_steps.push(TestStep::Goto {
                    url: interpolate(url, scope, file, *span)?,
                    wait_until: *wait_until,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::Snapshot {
                interactive_only, ..
            } => {
                out_steps.push(TestStep::Snapshot {
                    interactive_only: *interactive_only,
                });
            }
            Stmt::ClickRef { ref_id, .. } => {
                out_steps.push(TestStep::ClickRef {
                    ref_id: ref_id.clone(),
                });
            }
            Stmt::ClickRole {
                role,
                name,
                exact,
                nth,
                timeout_ms,
                span,
            } => {
                let resolved_name = name
                    .as_deref()
                    .map(|n| interpolate(n, scope, file, *span))
                    .transpose()?;
                out_steps.push(TestStep::ClickRole {
                    role: interpolate(role, scope, file, *span)?,
                    name: resolved_name,
                    exact: *exact,
                    nth: *nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::FillRef { ref_id, text, span } => {
                out_steps.push(TestStep::FillRef {
                    ref_id: ref_id.clone(),
                    text: interpolate(text, scope, file, *span)?,
                });
            }
            Stmt::FillRole {
                role,
                name,
                text,
                exact,
                nth,
                timeout_ms,
                span,
            } => {
                let resolved_name = name
                    .as_deref()
                    .map(|n| interpolate(n, scope, file, *span))
                    .transpose()?;
                out_steps.push(TestStep::FillRole {
                    role: interpolate(role, scope, file, *span)?,
                    name: resolved_name,
                    text: interpolate(text, scope, file, *span)?,
                    exact: *exact,
                    nth: *nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::DblclickRef { ref_id, .. } => {
                out_steps.push(TestStep::DblclickRef {
                    ref_id: ref_id.clone(),
                });
            }
            Stmt::DblclickText {
                text,
                exact,
                nth,
                timeout_ms,
                span,
            } => {
                out_steps.push(TestStep::DblclickText {
                    text: interpolate(text, scope, file, *span)?,
                    exact: *exact,
                    nth: *nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::HoverRef { ref_id, .. } => {
                out_steps.push(TestStep::HoverRef {
                    ref_id: ref_id.clone(),
                });
            }
            Stmt::HoverText {
                text,
                exact,
                nth,
                timeout_ms,
                span,
            } => {
                out_steps.push(TestStep::HoverText {
                    text: interpolate(text, scope, file, *span)?,
                    exact: *exact,
                    nth: *nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::PressKey { key, span } => {
                out_steps.push(TestStep::PressKey {
                    key: interpolate(key, scope, file, *span)?,
                });
            }
            Stmt::TypeText { text, span } => {
                out_steps.push(TestStep::TypeText {
                    text: interpolate(text, scope, file, *span)?,
                });
            }
            Stmt::Reload { ignore_cache, .. } => {
                out_steps.push(TestStep::Reload {
                    ignore_cache: *ignore_cache,
                });
            }
            Stmt::ExpectVisible {
                text,
                exact,
                timeout_ms,
                span,
            } => {
                out_steps.push(TestStep::AssertVisible {
                    text: interpolate(text, scope, file, *span)?,
                    exact: *exact,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ExpectHidden {
                text,
                exact,
                timeout_ms,
                span,
            } => {
                out_steps.push(TestStep::AssertHidden {
                    text: interpolate(text, scope, file, *span)?,
                    exact: *exact,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ConsoleStart { .. } => {
                out_steps.push(TestStep::ConsoleStart);
            }
            Stmt::ConsoleClear { .. } => {
                out_steps.push(TestStep::ConsoleClear);
            }
            Stmt::ExpectNoConsoleErrors { .. } => {
                out_steps.push(TestStep::AssertNoConsoleErrors);
            }
            Stmt::ExpectConsole {
                text,
                level,
                timeout_ms,
                span,
            } => {
                if let Some(lvl) = level
                    && ConsoleLevel::parse_level(lvl).is_none()
                {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "invalid console level `{lvl}`; expected one of: log, info, warn, error, debug, exception, browser"
                        ),
                    });
                }
                out_steps.push(TestStep::AssertConsole {
                    text: interpolate(text, scope, file, *span)?,
                    level: level.clone(),
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ExpectScreenshot {
                name,
                max_diff_ratio,
                threshold,
                span,
            } => {
                let resolved_name = interpolate(name, scope, file, *span)?;
                let valid_name = !resolved_name.is_empty()
                    && resolved_name.len() <= 128
                    && !resolved_name.starts_with('.')
                    && !resolved_name.contains("..")
                    && resolved_name
                        .bytes()
                        .all(|b| b.is_ascii_alphanumeric() || matches!(b, b'.' | b'_' | b'-'));
                if !valid_name {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "invalid screenshot baseline name `{resolved_name}`; use 1..=128 ASCII letters, digits, '.', '_', or '-' without leading '.' or '..'"
                        ),
                    });
                }
                if let Some(t) = threshold
                    && (!t.is_finite() || !(0.0..=1.0).contains(t))
                {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "screenshot `threshold` must be between 0.0 and 1.0, got {t}"
                        ),
                    });
                }
                if let Some(r) = max_diff_ratio
                    && (!r.is_finite() || !(0.0..=1.0).contains(r))
                {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "screenshot `maxDiff` must be between 0.0 and 1.0, got {r}"
                        ),
                    });
                }
                out_steps.push(TestStep::AssertScreenshot {
                    name: resolved_name,
                    max_diff_ratio: *max_diff_ratio,
                    threshold: *threshold,
                });
            }
            Stmt::Wait { ms, .. } => {
                out_steps.push(TestStep::WaitMs { ms: *ms });
            }
            Stmt::Cdp {
                method,
                params,
                span,
            } => {
                let resolved_method = interpolate(method, scope, file, *span)?;
                if is_blocked_cdp_method(&resolved_method) {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: *span,
                        message: format!(
                            "CDP command `{resolved_method}` is blocked for browser profile safety"
                        ),
                    });
                }
                out_steps.push(TestStep::RawCdp {
                    method: resolved_method,
                    params: interpolate_json(params, scope, file, *span)?,
                });
            }
            Stmt::Mock(mock) => {
                if !(100..=599).contains(&mock.status) {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: mock.span,
                        message: format!(
                            "mock HTTP status must be between 100 and 599, got {}",
                            mock.status
                        ),
                    });
                }
                let mut headers = HashMap::with_capacity(mock.headers.len());
                for (k, v) in &mock.headers {
                    headers.insert(
                        interpolate(k, scope, file, mock.span)?,
                        interpolate(v, scope, file, mock.span)?,
                    );
                }

                let mut patterns = Vec::with_capacity(mock.patterns.len());
                for pat in &mock.patterns {
                    patterns.push(interpolate(pat, scope, file, mock.span)?);
                }

                let body = match &mock.body {
                    Some(MockBodyAst::Inline(raw)) => interpolate(raw, scope, file, mock.span)?,
                    Some(MockBodyAst::File(rel_raw)) => {
                        let rel = interpolate(rel_raw, scope, file, mock.span)?;
                        let target = source_dir.join(&rel);
                        fs::read_to_string(&target).map_err(|source| DslError::Io {
                            path: target,
                            source,
                        })?
                    }
                    None => String::new(),
                };

                out_steps.push(TestStep::MockRoute {
                    url_substring: interpolate(&mock.url_substring, scope, file, mock.span)?,
                    status: mock.status,
                    headers,
                    body,
                    patterns,
                });
            }
        }
    }
    Ok(())
}

fn interpolate_json(
    val: &Value,
    scope: &HashMap<String, String>,
    file: &str,
    span: Span,
) -> Result<Value, DslError> {
    match val {
        Value::String(s) => Ok(Value::String(interpolate(s, scope, file, span)?)),
        Value::Array(items) => items
            .iter()
            .map(|item| interpolate_json(item, scope, file, span))
            .collect::<Result<Vec<_>, _>>()
            .map(Value::Array),
        Value::Object(map) => {
            let mut out = serde_json::Map::with_capacity(map.len());
            for (k, v) in map {
                out.insert(
                    interpolate(k, scope, file, span)?,
                    interpolate_json(v, scope, file, span)?,
                );
            }
            Ok(Value::Object(out))
        }
        other => Ok(other.clone()),
    }
}

pub fn interpolate(
    raw: &str,
    scope: &HashMap<String, String>,
    file: &str,
    span: Span,
) -> Result<String, DslError> {
    let mut out = String::with_capacity(raw.len());
    let mut chars = raw.chars().peekable();

    while let Some(ch) = chars.next() {
        if ch == ESCAPED_DOLLAR_SENTINEL {
            out.push('$');
            continue;
        }
        if ch == '$' && chars.peek() == Some(&'{') {
            chars.next(); // consume '{'
            let mut var_name = String::new();
            let mut closed = false;
            for inner in chars.by_ref() {
                if inner == '}' {
                    closed = true;
                    break;
                }
                var_name.push(inner);
            }
            if !closed {
                return Err(DslError::Compile {
                    file: file.to_string(),
                    span,
                    message: "unclosed `${...}` variable interpolation".to_string(),
                });
            }
            let trimmed = var_name.trim();
            let val = scope.get(trimmed).ok_or_else(|| DslError::Compile {
                file: file.to_string(),
                span,
                message: format!("undefined variable `{trimmed}`"),
            })?;
            out.push_str(val);
        } else {
            out.push(ch);
        }
    }

    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compiles_imports_macros_before_each_and_file_fixtures() {
        let dir = std::env::temp_dir().join(format!("ferrite-dsl-test-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("helpers")).unwrap();
        fs::create_dir_all(dir.join("fixtures")).unwrap();

        fs::write(
            dir.join("fixtures/login.html"),
            "<!doctype html><html><body><button>Sign In</button></body></html>",
        )
        .unwrap();

        fs::write(
            dir.join("helpers/auth.ferrite"),
            r#"
            let api_user = "ada"

            macro setup_portal(domain) {
                mock "/api/session" status 200 pattern "*://${domain}/*" {
                    header "content-type" = "application/json"
                    body "{\"user\":\"${api_user}\"}"
                }
                mock "/login" status 200 pattern "*://${domain}/*" {
                    header "content-type" = "text/html; charset=utf-8"
                    body file "../fixtures/login.html"
                }
            }
            "#,
        )
        .unwrap();

        let main_file = dir.join("login.ferrite");
        fs::write(
            &main_file,
            r#"
            suite "portal-smoke"
            import "./helpers/auth.ferrite"

            let base_url = "https://example.com"

            beforeEach {
                use setup_portal("example.com")
                goto "${base_url}/login" wait load timeout 15s
            }

            test "user signs in" {
                snapshot interactive
                fill @e1 "${api_user}@example.com"
                click role "button" "Sign In" exact
                expect visible "Dashboard Ready" timeout 5s
                expect screenshot "dashboard-ready" threshold 0.1 maxDiff 0.001
                cdp "Page.bringToFront" { "extra": "${api_user}" }
            }
            "#,
        )
        .unwrap();

        let compiled = compile_file(&main_file).unwrap();
        assert_eq!(compiled.suite_name_or_name(), "portal-smoke");
        assert_eq!(compiled.cases.len(), 1);
        let steps = &compiled.cases[0].steps;
        assert_eq!(steps.len(), 9);
        assert!(matches!(
            &steps[1],
            TestStep::MockRoute { body, .. } if body.contains("<button>Sign In</button>")
        ));
        assert!(matches!(
            &steps[2],
            TestStep::Goto { url, timeout_ms: 15_000, .. } if url == "https://example.com/login"
        ));
        assert!(matches!(
            &steps[4],
            TestStep::FillRef { ref_id, text } if ref_id == "e1" && text == "ada@example.com"
        ));
        assert!(matches!(
            &steps[8],
            TestStep::RawCdp { method, params } if method == "Page.bringToFront" && params["extra"] == "ada"
        ));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn detects_circular_imports_and_undefined_variables() {
        let dir = std::env::temp_dir().join(format!("ferrite-dsl-err-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();

        let a = dir.join("a.ferrite");
        let b = dir.join("b.ferrite");
        fs::write(&a, "import \"./b.ferrite\"\n").unwrap();
        fs::write(&b, "import \"./a.ferrite\"\n").unwrap();

        assert!(matches!(compile_file(&a), Err(DslError::CircularImport(_))));

        let bad_var = dir.join("bad_var.ferrite");
        fs::write(&bad_var, "test \"x\" {\n  goto \"https://${missing}/a\"\n}\n").unwrap();
        let err = compile_file(&bad_var).unwrap_err();
        assert!(err.to_string().contains("undefined variable `missing`"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compiles_extended_interactions_and_hidden_assertions() {
        let src = r#"
            suite "interactions"
            test "todomvc-like" {
                fill role "textbox" "Buy milk" timeout 4s
                press "Enter"
                click role "checkbox" nth 1 timeout 3s
                dblclick text "Buy milk" exact
                press "Control+a"
                press "Backspace"
                type "Buy oat milk"
                press "Enter"
                hover text "Buy oat milk"
                click role "button" "" exact
                expect hidden "Buy oat milk" timeout 2s
                reload ignoreCache
            }
        "#;
        let suite = compile_source("interactions.ferrite", Path::new("."), src).unwrap();
        let steps = &suite.cases[0].steps;
        assert_eq!(steps.len(), 12);
        assert!(matches!(
            &steps[0],
            TestStep::FillRole { role, name: None, text, timeout_ms: 4_000, .. }
                if role == "textbox" && text == "Buy milk"
        ));
        assert!(matches!(&steps[1], TestStep::PressKey { key } if key == "Enter"));
        assert!(matches!(&steps[2], TestStep::ClickRole { role, nth: 1, .. } if role == "checkbox"));
        assert!(matches!(&steps[3], TestStep::DblclickText { text, exact: true, .. } if text == "Buy milk"));
        assert!(matches!(&steps[6], TestStep::TypeText { text } if text == "Buy oat milk"));
        assert!(matches!(&steps[8], TestStep::HoverText { text, .. } if text == "Buy oat milk"));
        assert!(matches!(&steps[10], TestStep::AssertHidden { text, timeout_ms: 2_000, .. } if text == "Buy oat milk"));
        assert!(matches!(&steps[11], TestStep::Reload { ignore_cache: true }));
    }

    #[test]
    fn compiles_console_assertions_and_recorded_ferrite_script() {
        let recorded = ferrite_cdp::render_ferrite_script(
            "rec-suite",
            "rec-case",
            &[
                ferrite_cdp::RecordedAction::Navigate {
                    url: "https://example.com".into(),
                },
                ferrite_cdp::RecordedAction::FillRole {
                    role: "textbox".into(),
                    name: Some("Email".into()),
                    value: "ada@example.com".into(),
                },
                ferrite_cdp::RecordedAction::PressKey {
                    key: "Enter".into(),
                },
            ],
        );
        let parsed_rec = compile_source("recorded.ferrite", Path::new("."), &recorded).unwrap();
        assert_eq!(parsed_rec.cases[0].steps.len(), 3);

        let console_src = r#"
            suite "console-suite"
            test "checks-console" {
                console start
                console clear
                expect console "booted" level info timeout 2s
                expect noConsoleErrors
            }
        "#;
        let console_suite = compile_source("console.ferrite", Path::new("."), console_src).unwrap();
        assert_eq!(console_suite.cases[0].steps.len(), 4);
        assert!(matches!(console_suite.cases[0].steps[0], TestStep::ConsoleStart));
        assert!(matches!(console_suite.cases[0].steps[3], TestStep::AssertNoConsoleErrors));
    }

    #[test]
    fn rejects_invalid_semantics_at_compile_time() {
        let cases = [
            (
                r#"test "bad-threshold" { expect screenshot "home" threshold 1.5 }"#,
                "threshold",
            ),
            (
                r#"test "bad-maxdiff" { expect screenshot "home" maxDiff 2.0 }"#,
                "maxDiff",
            ),
            (
                r#"test "bad-name" { expect screenshot "../escape" }"#,
                "invalid screenshot baseline name",
            ),
            (
                r#"test "bad-status" { mock "/api" status 999 { body "{}" } }"#,
                "100 and 599",
            ),
            (
                r#"test "bad-level" { expect console "hi" level fatal }"#,
                "invalid console level",
            ),
            (
                r#"test "blocked-cdp" { cdp "Storage.clearCookies" {} }"#,
                "blocked for browser profile safety",
            ),
        ];

        for (src, expected_substr) in cases {
            let err = compile_source("bad.ferrite", Path::new("."), src).unwrap_err();
            assert!(
                err.to_string().contains(expected_substr),
                "expected error containing {expected_substr:?}, got: {err}"
            );
        }
    }

    trait SuiteExt {
        fn suite_name_or_name(&self) -> &str;
    }
    impl SuiteExt for TestSuite {
        fn suite_name_or_name(&self) -> &str {
            &self.name
        }
    }
}
