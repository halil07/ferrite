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
            Stmt::ClickCss {
                selector,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::ClickCss {
                    selector: css,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::FillCss {
                selector,
                text,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::FillCss {
                    selector: css,
                    text: interpolate(text, scope, file, *span)?,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::DblclickCss {
                selector,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::DblclickCss {
                    selector: css,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::HoverCss {
                selector,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::HoverCss {
                    selector: css,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ExpectCssVisible {
                selector,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::AssertCssVisible {
                    selector: css,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ExpectCssHidden {
                selector,
                nth,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, resolved_nth) = resolve_jquery_css(&raw, *nth, file, *span)?;
                out_steps.push(TestStep::AssertCssHidden {
                    selector: css,
                    nth: resolved_nth,
                    timeout_ms: *timeout_ms,
                });
            }
            Stmt::ExpectCssCount {
                selector,
                expected,
                timeout_ms,
                span,
            } => {
                let raw = interpolate(selector, scope, file, *span)?;
                let (css, _) = resolve_jquery_css(&raw, 0, file, *span)?;
                out_steps.push(TestStep::AssertCssCount {
                    selector: css,
                    expected: *expected,
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
                if mock.fail_reason.is_none() && !(100..=599).contains(&mock.status) {
                    return Err(DslError::Compile {
                        file: file.to_string(),
                        span: mock.span,
                        message: format!(
                            "mock HTTP status must be between 100 and 599, got {}",
                            mock.status
                        ),
                    });
                }
                if let Some(ref reason) = mock.fail_reason {
                    validate_fail_reason(reason, file, mock.span)?;
                }

                let mut params = HashMap::with_capacity(mock.params.len());
                for (k, v) in &mock.params {
                    params.insert(
                        interpolate(k, scope, file, mock.span)?,
                        interpolate(v, scope, file, mock.span)?,
                    );
                }

                let data = mock
                    .data
                    .as_ref()
                    .map(|val| interpolate_json(val, scope, file, mock.span))
                    .transpose()?;

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
                        if !headers
                            .keys()
                            .any(|k| k.eq_ignore_ascii_case("content-type"))
                            && let Some(ct) = infer_content_type(&target)
                        {
                            headers.insert("content-type".to_string(), ct.to_string());
                        }
                        fs::read_to_string(&target).map_err(|source| DslError::Io {
                            path: target,
                            source,
                        })?
                    }
                    None => String::new(),
                };

                out_steps.push(TestStep::MockRoute {
                    url_substring: interpolate(&mock.url_substring, scope, file, mock.span)?,
                    method: mock.method.clone(),
                    params,
                    data,
                    status: mock.status,
                    headers,
                    body,
                    delay_ms: mock.delay_ms,
                    fail_reason: mock.fail_reason.clone(),
                    patterns,
                });
            }
            Stmt::MockLoad { file_path, span } => {
                let rel = interpolate(file_path, scope, file, *span)?;
                let scenario_path = source_dir.join(&rel);
                let loaded = compile_mock_scenario_file(&scenario_path, scope, file, *span)?;
                out_steps.extend(loaded);
            }
            Stmt::MockClear { .. } => {
                out_steps.push(TestStep::MockClear);
            }
        }
    }
    Ok(())
}

fn resolve_jquery_css(
    raw: &str,
    default_nth: usize,
    file: &str,
    span: Span,
) -> Result<(String, usize), DslError> {
    let trimmed = raw.trim();
    if let Some(base) = trimmed.strip_suffix(":first") {
        let clean = base.trim();
        if clean.is_empty() {
            return Err(DslError::Compile {
                file: file.to_string(),
                span,
                message: "CSS selector before `:first` cannot be empty".to_string(),
            });
        }
        return Ok((clean.to_string(), 0));
    }
    if let Some(eq_pos) = trimmed.rfind(":eq(")
        && trimmed.ends_with(')')
    {
        let base = trimmed[..eq_pos].trim();
        let idx_str = trimmed[eq_pos + 4..trimmed.len() - 1].trim();
        let idx: usize = idx_str.parse().map_err(|_| DslError::Compile {
            file: file.to_string(),
            span,
            message: format!("invalid `:eq({idx_str})` index in CSS selector `{raw}`"),
        })?;
        if base.is_empty() {
            return Err(DslError::Compile {
                file: file.to_string(),
                span,
                message: format!("CSS selector before `:eq({idx})` cannot be empty"),
            });
        }
        return Ok((base.to_string(), idx));
    }
    Ok((trimmed.to_string(), default_nth))
}

const VALID_FAIL_REASONS: &[&str] = &[
    "Failed",
    "Aborted",
    "TimedOut",
    "AccessDenied",
    "ConnectionClosed",
    "ConnectionReset",
    "ConnectionRefused",
    "ConnectionAborted",
    "ConnectionFailed",
    "NameNotResolved",
    "InternetDisconnected",
    "BlockedByClient",
    "BlockedByResponse",
];

fn validate_fail_reason(reason: &str, file: &str, span: Span) -> Result<(), DslError> {
    if !VALID_FAIL_REASONS.contains(&reason) {
        return Err(DslError::Compile {
            file: file.to_string(),
            span,
            message: format!(
                "invalid mock fail reason `{reason}`; expected one of: {}",
                VALID_FAIL_REASONS.join(", ")
            ),
        });
    }
    Ok(())
}

fn compile_mock_scenario_file(
    scenario_path: &Path,
    scope: &HashMap<String, String>,
    file: &str,
    span: Span,
) -> Result<Vec<TestStep>, DslError> {
    let raw = fs::read_to_string(scenario_path).map_err(|source| DslError::Io {
        path: scenario_path.to_path_buf(),
        source,
    })?;
    let parsed: Value = serde_json::from_str(&raw).map_err(|err| DslError::Compile {
        file: file.to_string(),
        span,
        message: format!("invalid mock scenario JSON `{}`: {err}", scenario_path.display()),
    })?;

    let entries = if let Some(arr) = parsed.as_array() {
        arr.clone()
    } else if let Some(arr) = parsed.get("routes").and_then(Value::as_array) {
        arr.clone()
    } else {
        return Err(DslError::Compile {
            file: file.to_string(),
            span,
            message: format!(
                "mock scenario file `{}` must be a JSON array `[...]` or `{{\"routes\": [...]}}`",
                scenario_path.display()
            ),
        });
    };

    let scenario_dir = scenario_path.parent().unwrap_or_else(|| Path::new("."));
    let mut steps = Vec::with_capacity(entries.len());

    for (idx, item) in entries.iter().enumerate() {
        let Some(obj) = item.as_object() else {
            return Err(DslError::Compile {
                file: file.to_string(),
                span,
                message: format!(
                    "mock scenario `{}` entry #{idx} must be a JSON object",
                    scenario_path.display()
                ),
            });
        };

        let raw_url = obj
            .get("url")
            .and_then(Value::as_str)
            .ok_or_else(|| DslError::Compile {
                file: file.to_string(),
                span,
                message: format!(
                    "mock scenario `{}` entry #{idx} is missing required string `url`",
                    scenario_path.display()
                ),
            })?;
        let url_substring = interpolate(raw_url, scope, file, span)?;

        let method = obj
            .get("method")
            .and_then(Value::as_str)
            .map(|m| m.to_ascii_uppercase())
            .filter(|m| m != "ANY" && m != "*");

        let mut params = HashMap::new();
        if let Some(pmap) = obj.get("params").and_then(Value::as_object) {
            for (k, v) in pmap {
                let raw_val = match v {
                    Value::String(s) => s.clone(),
                    other => other.to_string(),
                };
                params.insert(
                    interpolate(k, scope, file, span)?,
                    interpolate(&raw_val, scope, file, span)?,
                );
            }
        }

        let data = obj
            .get("data")
            .map(|v| interpolate_json(v, scope, file, span))
            .transpose()?;

        let status = obj.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
        let fail_reason = obj.get("fail").and_then(Value::as_str).map(str::to_string);
        if let Some(ref reason) = fail_reason {
            validate_fail_reason(reason, file, span)?;
        } else if !(100..=599).contains(&status) {
            return Err(DslError::Compile {
                file: file.to_string(),
                span,
                message: format!(
                    "mock scenario `{}` entry #{idx} has invalid HTTP status {status} (expected 100..=599)",
                    scenario_path.display()
                ),
            });
        }

        let delay_ms = obj
            .get("delayMs")
            .or_else(|| obj.get("delay"))
            .and_then(Value::as_u64)
            .unwrap_or(0);

        let mut headers = HashMap::new();
        if let Some(hmap) = obj.get("headers").and_then(Value::as_object) {
            for (k, v) in hmap {
                if let Some(vs) = v.as_str() {
                    headers.insert(
                        interpolate(k, scope, file, span)?,
                        interpolate(vs, scope, file, span)?,
                    );
                }
            }
        }

        let mut patterns = Vec::new();
        if let Some(pat) = obj.get("pattern").and_then(Value::as_str) {
            patterns.push(interpolate(pat, scope, file, span)?);
        }
        if let Some(arr) = obj.get("patterns").and_then(Value::as_array) {
            for p in arr {
                if let Some(s) = p.as_str() {
                    patterns.push(interpolate(s, scope, file, span)?);
                }
            }
        }

        let body = if let Some(resp_rel) = obj
            .get("response")
            .or_else(|| obj.get("file"))
            .and_then(Value::as_str)
        {
            let resolved_rel = interpolate(resp_rel, scope, file, span)?;
            let target = scenario_dir.join(&resolved_rel);
            if !headers
                .keys()
                .any(|k| k.eq_ignore_ascii_case("content-type"))
                && let Some(ct) = infer_content_type(&target)
            {
                headers.insert("content-type".to_string(), ct.to_string());
            }
            fs::read_to_string(&target).map_err(|source| DslError::Io {
                path: target,
                source,
            })?
        } else {
            String::new()
        };

        steps.push(TestStep::MockRoute {
            url_substring,
            method,
            params,
            data,
            status,
            headers,
            body,
            delay_ms,
            fail_reason,
            patterns,
        });
    }

    Ok(steps)
}

fn infer_content_type(path: &Path) -> Option<&'static str> {
    let ext = path.extension()?.to_str()?.to_ascii_lowercase();
    match ext.as_str() {
        "json" => Some("application/json; charset=utf-8"),
        "html" | "htm" => Some("text/html; charset=utf-8"),
        "xml" => Some("application/xml; charset=utf-8"),
        "txt" => Some("text/plain; charset=utf-8"),
        "js" | "mjs" => Some("application/javascript; charset=utf-8"),
        "css" => Some("text/css; charset=utf-8"),
        _ => None,
    }
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
            let api_user = "ada";

            macro setup_portal(domain) {
                await mock("GET", "/api/session", {
                    status: 200,
                    pattern: "*://${domain}/*",
                    headers: { "content-type": "application/json" },
                    body: "{\"user\":\"${api_user}\"}"
                });
                await mock("GET", "/login", {
                    status: 200,
                    pattern: "*://${domain}/*",
                    headers: { "content-type": "text/html; charset=utf-8" },
                    file: "../fixtures/login.html"
                });
            }
            "#,
        )
        .unwrap();

        let main_file = dir.join("login.ferrite");
        fs::write(
            &main_file,
            r#"
            suite("portal-smoke");
            import "./helpers/auth.ferrite";

            let base_url = "https://example.com";

            beforeEach {
                await setup_portal("example.com");
                await goto("${base_url}/login", "load", 15s);
            }

            test("user signs in") {
                await snapshot("interactive");
                await $(@e1).fill("${api_user}@example.com");
                await $role("button", "Sign In").exact().click();
                await $text("Dashboard Ready").expect().visible(5s);
                await screenshot("dashboard-ready", 0.1, 0.001);
                await cdp("Page.bringToFront", { "extra": "${api_user}" });
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
        fs::write(&a, "import \"./b.ferrite\";\n").unwrap();
        fs::write(&b, "import \"./a.ferrite\";\n").unwrap();

        assert!(matches!(compile_file(&a), Err(DslError::CircularImport(_))));

        let bad_var = dir.join("bad_var.ferrite");
        fs::write(&bad_var, "test(\"x\") {\n  await goto(\"https://${missing}/a\");\n}\n").unwrap();
        let err = compile_file(&bad_var).unwrap_err();
        assert!(err.to_string().contains("undefined variable `missing`"));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compiles_extended_interactions_and_hidden_assertions() {
        let src = r#"
            suite("interactions");
            test("todomvc-like") {
                await $role("textbox").timeout(4s).fill("Buy milk").press("Enter");
                await $role("checkbox").eq(1).click(3s);
                await $text("Buy milk").exact().dblclick()
                    .press("Control+a")
                    .press("Backspace")
                    .type("Buy oat milk")
                    .press("Enter");
                await $text("Buy oat milk").hover();
                await $role("button", "").exact().click();
                await $text("Buy oat milk").expect().hidden(2s);
                await reload(true);
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
            suite("console-suite");
            test("checks-console") {
                await console.start();
                await console.clear();
                await console.expect("booted", "info", 2s);
                await console.noErrors();
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
                r#"test("bad-threshold") { await screenshot("home", 1.5); }"#,
                "threshold",
            ),
            (
                r#"test("bad-maxdiff") { await screenshot("home", { maxDiff: 2.0 }); }"#,
                "maxDiff",
            ),
            (
                r#"test("bad-name") { await screenshot("../escape"); }"#,
                "invalid screenshot baseline name",
            ),
            (
                r#"test("bad-status") { await mock("GET", "/api", { status: 999, body: "{}" }); }"#,
                "100 and 599",
            ),
            (
                r#"test("bad-level") { await console.expect("hi", "fatal"); }"#,
                "invalid console level",
            ),
            (
                r#"test("blocked-cdp") { await cdp("Storage.clearCookies", {}); }"#,
                "blocked for browser profile safety",
            ),
            (
                r#"test("bad-eq") { await $(".item:eq(abc)").click(); }"#,
                "invalid `:eq(abc)`",
            ),
            (
                r#"test("missing-semicolon") { await sleep(1s) }"#,
                "`;`",
            ),
            (
                r#"test("missing-expect") { await $("h1").visible(); }"#,
                "must be called after `.expect()`",
            ),
            (
                r#"test("action-after-expect") { await $("button").expect().click(); }"#,
                "cannot be called after `.expect()`",
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

    #[test]
    fn compiles_fluent_mock_when_and_external_mock_load() {
        let dir = std::env::temp_dir().join(format!("ferrite-dsl-mock-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(dir.join("mocks/responses")).unwrap();
        fs::write(
            dir.join("mocks/responses/orders.json"),
            r#"{"orders":[{"id":101}]}"#,
        )
        .unwrap();
        fs::write(
            dir.join("mocks/responses/login-ok.json"),
            r#"{"token":"jwt-123"}"#,
        )
        .unwrap();
        fs::write(
            dir.join("mocks/checkout.mock.json"),
            r#"[
              {
                "method": "POST",
                "url": "/api/login",
                "data": { "email": "admin@test.com" },
                "status": 200,
                "delayMs": 150,
                "response": "./responses/login-ok.json"
              },
              {
                "method": "GET",
                "url": "/api/down",
                "fail": "TimedOut",
                "delayMs": 50
              }
            ]"#,
        )
        .unwrap();

        let src = r#"
            suite("api-mock");
            test("fluent-and-scenario-mocks") {
                await mock.when("GET", "/api/users/:id/orders")
                    .params({ id: "42", page: "1" })
                    .header("x-mock", "true")
                    .delay(300ms)
                    .reply(200, "./mocks/responses/orders.json");
                await mock.when("GET", "/api/health").abort();
                await mock.load("./mocks/checkout.mock.json");
                await mock.clear();
            }
        "#;
        let suite = compile_source("mock.ferrite", &dir, src).unwrap();
        let steps = &suite.cases[0].steps;
        assert_eq!(steps.len(), 5);
        match &steps[0] {
            TestStep::MockRoute {
                url_substring,
                method,
                params,
                status,
                headers,
                body,
                delay_ms,
                fail_reason,
                ..
            } => {
                assert_eq!(url_substring, "/api/users/:id/orders");
                assert_eq!(method.as_deref(), Some("GET"));
                assert_eq!(params.get("id").map(String::as_str), Some("42"));
                assert_eq!(params.get("page").map(String::as_str), Some("1"));
                assert_eq!(*status, 200);
                assert_eq!(*delay_ms, 300);
                assert!(fail_reason.is_none());
                assert_eq!(
                    headers.get("content-type").map(String::as_str),
                    Some("application/json; charset=utf-8")
                );
                assert_eq!(headers.get("x-mock").map(String::as_str), Some("true"));
                assert!(body.contains("101"));
            }
            other => panic!("expected MockRoute, got {other:?}"),
        }
        assert!(matches!(
            &steps[1],
            TestStep::MockRoute {
                fail_reason: Some(r),
                ..
            } if r == "Aborted"
        ));
        assert!(matches!(
            &steps[2],
            TestStep::MockRoute {
                url_substring,
                delay_ms: 150,
                data: Some(_),
                body,
                ..
            } if url_substring == "/api/login" && body.contains("jwt-123")
        ));
        assert!(matches!(
            &steps[3],
            TestStep::MockRoute {
                url_substring,
                fail_reason: Some(r),
                delay_ms: 50,
                ..
            } if url_substring == "/api/down" && r == "TimedOut"
        ));
        assert!(matches!(&steps[4], TestStep::MockClear));

        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn compiles_jquery_css_selectors_eq_chaining_and_sleep() {
        let src = r##"
            suite("jquery-selectors");
            test("uses-jquery-style") {
                await $("input.new-todo").timeout(4s).val("Buy milk").press("Enter");
                await $("#submit-btn").click(3s);
                await $(".todo-list li label:eq(1)").dblclick();
                await $(".todo-list li:first").hover();
                await sleep(5s);
                await $(".todo-list li").expect().visible(3s);
                await $(".loading-spinner").expect().hidden(2s);
                await $(".todo-list li").expect().count(2, 4s);
            }
        "##;
        let suite = compile_source("css.ferrite", Path::new("."), src).unwrap();
        let steps = &suite.cases[0].steps;
        assert_eq!(steps.len(), 9);
        assert!(matches!(
            &steps[0],
            TestStep::FillCss { selector, text, timeout_ms: 4_000, .. }
                if selector == "input.new-todo" && text == "Buy milk"
        ));
        assert!(matches!(&steps[1], TestStep::PressKey { key } if key == "Enter"));
        assert!(matches!(
            &steps[2],
            TestStep::ClickCss { selector, nth: 0, timeout_ms: 3_000 } if selector == "#submit-btn"
        ));
        assert!(matches!(
            &steps[3],
            TestStep::DblclickCss { selector, nth: 1, .. } if selector == ".todo-list li label"
        ));
        assert!(matches!(
            &steps[4],
            TestStep::HoverCss { selector, nth: 0, .. } if selector == ".todo-list li"
        ));
        assert!(matches!(&steps[5], TestStep::WaitMs { ms: 5_000 }));
        assert!(matches!(
            &steps[6],
            TestStep::AssertCssVisible { selector, timeout_ms: 3_000, .. } if selector == ".todo-list li"
        ));
        assert!(matches!(
            &steps[7],
            TestStep::AssertCssHidden { selector, timeout_ms: 2_000, .. } if selector == ".loading-spinner"
        ));
        assert!(matches!(
            &steps[8],
            TestStep::AssertCssCount { selector, expected: 2, timeout_ms: 4_000 }
                if selector == ".todo-list li"
        ));
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
