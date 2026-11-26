#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::time::{Duration, Instant};

use ferrite_cdp::{LoadState, MockRouteResponse};
use ferrite_core::{WsRequest, WsResponse};
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::client::IntoClientRequest;

use crate::session::E2eSession;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestSuite {
    pub name: String,
    #[serde(default)]
    pub cases: Vec<TestCase>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct TestCase {
    pub name: String,
    #[serde(default)]
    pub tab_id: Option<i64>,
    #[serde(default)]
    pub steps: Vec<TestStep>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "step", rename_all = "snake_case")]
pub enum TestStep {
    Goto {
        url: String,
        #[serde(default = "default_load_state")]
        wait_until: LoadState,
        #[serde(default = "default_timeout_ms")]
        timeout_ms: u64,
    },
    Snapshot {
        #[serde(default = "default_true")]
        interactive_only: bool,
    },
    ClickRef {
        ref_id: String,
    },
    ClickRole {
        role: String,
        #[serde(default)]
        name: Option<String>,
        #[serde(default)]
        exact: bool,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    FillRef {
        ref_id: String,
        text: String,
    },
    FillRole {
        role: String,
        #[serde(default)]
        name: Option<String>,
        text: String,
        #[serde(default)]
        exact: bool,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    DblclickRef {
        ref_id: String,
    },
    DblclickText {
        text: String,
        #[serde(default)]
        exact: bool,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    HoverRef {
        ref_id: String,
    },
    HoverText {
        text: String,
        #[serde(default)]
        exact: bool,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    PressKey {
        key: String,
    },
    TypeText {
        text: String,
    },
    Reload {
        #[serde(default)]
        ignore_cache: bool,
    },
    AssertVisible {
        text: String,
        #[serde(default)]
        exact: bool,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    AssertHidden {
        text: String,
        #[serde(default)]
        exact: bool,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    ClickCss {
        selector: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    FillCss {
        selector: String,
        text: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    DblclickCss {
        selector: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    HoverCss {
        selector: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    AssertCssVisible {
        selector: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    AssertCssHidden {
        selector: String,
        #[serde(default)]
        nth: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    AssertCssCount {
        selector: String,
        expected: usize,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    ConsoleStart,
    ConsoleClear,
    AssertNoConsoleErrors,
    AssertConsole {
        text: String,
        #[serde(default)]
        level: Option<String>,
        #[serde(default = "default_action_timeout_ms")]
        timeout_ms: u64,
    },
    MockRoute {
        url_substring: String,
        #[serde(default)]
        method: Option<String>,
        #[serde(default)]
        params: HashMap<String, String>,
        #[serde(default)]
        data: Option<Value>,
        status: u16,
        #[serde(default)]
        headers: HashMap<String, String>,
        #[serde(default)]
        body: String,
        #[serde(default)]
        delay_ms: u64,
        #[serde(default)]
        fail_reason: Option<String>,
        #[serde(default)]
        patterns: Vec<String>,
    },
    MockClear,
    AssertScreenshot {
        name: String,
        #[serde(default)]
        max_diff_ratio: Option<f64>,
        #[serde(default)]
        threshold: Option<f64>,
    },
    WaitMs {
        ms: u64,
    },
    RawCdp {
        method: String,
        #[serde(default)]
        params: Value,
    },
}

fn default_load_state() -> LoadState {
    LoadState::Load
}

fn default_timeout_ms() -> u64 {
    15_000
}

fn default_action_timeout_ms() -> u64 {
    5_000
}

fn default_true() -> bool {
    true
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct StepResult {
    pub index: usize,
    pub action: String,
    pub passed: bool,
    pub duration_ms: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct CaseReport {
    pub name: String,
    pub passed: bool,
    pub duration_ms: u64,
    pub steps: Vec<StepResult>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SuiteReport {
    pub suite: String,
    pub passed: bool,
    pub total_cases: usize,
    pub passed_cases: usize,
    pub failed_cases: usize,
    pub duration_ms: u64,
    pub cases: Vec<CaseReport>,
}

fn step_to_rpc(
    step: &TestStep,
    tab_id: Option<i64>,
    update_baselines: bool,
    snapshots_dir: Option<&Path>,
) -> (String, Value) {
    match step {
        TestStep::Goto {
            url,
            wait_until,
            timeout_ms,
        } => (
            "goto".to_string(),
            json!({
                "tabId": tab_id,
                "url": url,
                "waitUntil": wait_until,
                "timeoutMs": timeout_ms,
            }),
        ),
        TestStep::Snapshot { interactive_only } => (
            "snapshot".to_string(),
            json!({ "tabId": tab_id, "interactiveOnly": interactive_only }),
        ),
        TestStep::ClickRef { ref_id } => (
            "click_ref".to_string(),
            json!({ "tabId": tab_id, "refId": ref_id }),
        ),
        TestStep::ClickRole {
            role,
            name,
            exact,
            nth,
            timeout_ms,
        } => (
            "click_role".to_string(),
            json!({
                "tabId": tab_id,
                "role": role,
                "name": name,
                "exact": exact,
                "nth": nth,
                "timeoutMs": timeout_ms
            }),
        ),
        TestStep::FillRef { ref_id, text } => (
            "fill_ref".to_string(),
            json!({ "tabId": tab_id, "refId": ref_id, "text": text }),
        ),
        TestStep::FillRole {
            role,
            name,
            text,
            exact,
            nth,
            timeout_ms,
        } => (
            "fill_role".to_string(),
            json!({
                "tabId": tab_id,
                "role": role,
                "name": name,
                "text": text,
                "exact": exact,
                "nth": nth,
                "timeoutMs": timeout_ms
            }),
        ),
        TestStep::DblclickRef { ref_id } => (
            "dblclick_ref".to_string(),
            json!({ "tabId": tab_id, "refId": ref_id }),
        ),
        TestStep::DblclickText {
            text,
            exact,
            nth,
            timeout_ms,
        } => (
            "dblclick_text".to_string(),
            json!({
                "tabId": tab_id,
                "text": text,
                "exact": exact,
                "nth": nth,
                "timeoutMs": timeout_ms
            }),
        ),
        TestStep::HoverRef { ref_id } => (
            "hover_ref".to_string(),
            json!({ "tabId": tab_id, "refId": ref_id }),
        ),
        TestStep::HoverText {
            text,
            exact,
            nth,
            timeout_ms,
        } => (
            "hover_text".to_string(),
            json!({
                "tabId": tab_id,
                "text": text,
                "exact": exact,
                "nth": nth,
                "timeoutMs": timeout_ms
            }),
        ),
        TestStep::PressKey { key } => (
            "press_key".to_string(),
            json!({ "tabId": tab_id, "key": key }),
        ),
        TestStep::TypeText { text } => (
            "type_text".to_string(),
            json!({ "tabId": tab_id, "text": text }),
        ),
        TestStep::Reload { ignore_cache } => (
            "reload".to_string(),
            json!({ "tabId": tab_id, "ignoreCache": ignore_cache }),
        ),
        TestStep::AssertVisible {
            text,
            exact,
            timeout_ms,
        } => (
            "assert_text".to_string(),
            json!({ "tabId": tab_id, "text": text, "exact": exact, "timeoutMs": timeout_ms }),
        ),
        TestStep::AssertHidden {
            text,
            exact,
            timeout_ms,
        } => (
            "assert_hidden".to_string(),
            json!({ "tabId": tab_id, "text": text, "exact": exact, "timeoutMs": timeout_ms }),
        ),
        TestStep::ClickCss {
            selector,
            nth,
            timeout_ms,
        } => (
            "click_css".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::FillCss {
            selector,
            text,
            nth,
            timeout_ms,
        } => (
            "fill_css".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "text": text, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::DblclickCss {
            selector,
            nth,
            timeout_ms,
        } => (
            "dblclick_css".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::HoverCss {
            selector,
            nth,
            timeout_ms,
        } => (
            "hover_css".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::AssertCssVisible {
            selector,
            nth,
            timeout_ms,
        } => (
            "assert_css_visible".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::AssertCssHidden {
            selector,
            nth,
            timeout_ms,
        } => (
            "assert_css_hidden".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "nth": nth, "timeoutMs": timeout_ms }),
        ),
        TestStep::AssertCssCount {
            selector,
            expected,
            timeout_ms,
        } => (
            "assert_css_count".to_string(),
            json!({ "tabId": tab_id, "selector": selector, "expected": expected, "timeoutMs": timeout_ms }),
        ),
        TestStep::ConsoleStart => ("console.start".to_string(), json!({ "tabId": tab_id })),
        TestStep::ConsoleClear => ("console.clear".to_string(), json!({ "tabId": tab_id })),
        TestStep::AssertNoConsoleErrors => (
            "assert_no_console_errors".to_string(),
            json!({ "tabId": tab_id }),
        ),
        TestStep::AssertConsole {
            text,
            level,
            timeout_ms,
        } => (
            "assert_console".to_string(),
            json!({
                "tabId": tab_id,
                "text": text,
                "level": level,
                "timeoutMs": timeout_ms
            }),
        ),
        TestStep::MockRoute {
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
        } => (
            "intercept_network".to_string(),
            json!({
                "tabId": tab_id,
                "route": MockRouteResponse {
                    url_substring: url_substring.clone(),
                    method: method.clone(),
                    params: params.clone(),
                    data: data.clone(),
                    status: *status,
                    headers: headers.clone(),
                    body: body.clone(),
                    delay_ms: *delay_ms,
                    fail_reason: fail_reason.clone(),
                },
                "patterns": patterns,
            }),
        ),
        TestStep::MockClear => ("mock.clear".to_string(), json!({ "tabId": tab_id })),
        TestStep::AssertScreenshot {
            name,
            max_diff_ratio,
            threshold,
        } => (
            "assert_screenshot".to_string(),
            json!({
                "tabId": tab_id,
                "name": name,
                "updateBaselines": update_baselines,
                "maxDiffRatio": max_diff_ratio,
                "threshold": threshold,
                "snapshotsDir": snapshots_dir.map(|p| p.to_string_lossy().into_owned()),
            }),
        ),
        TestStep::WaitMs { ms } => ("wait_ms".to_string(), json!({ "ms": ms })),
        TestStep::RawCdp { method, params } => (
            "raw_cdp".to_string(),
            json!({ "tabId": tab_id, "method": method, "params": params }),
        ),
    }
}

/// Executes a declarative E2E test suite directly against an in-process [`E2eSession`].
pub async fn run_suite(
    session: &E2eSession,
    suite: &TestSuite,
    update_baselines: bool,
) -> SuiteReport {
    let suite_start = Instant::now();
    let mut case_reports = Vec::with_capacity(suite.cases.len());
    let mut req_id = 1u64;

    for case in &suite.cases {
        let case_start = Instant::now();
        let mut step_results = Vec::new();
        let mut case_passed = true;

        for (idx, step) in case.steps.iter().enumerate() {
            let step_start = Instant::now();
            if let TestStep::WaitMs { ms } = step {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                step_results.push(StepResult {
                    index: idx,
                    action: "wait_ms".to_string(),
                    passed: true,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    output: Some(json!({ "sleptMs": ms })),
                    error: None,
                });
                continue;
            }

            let (action, params) = step_to_rpc(step, case.tab_id, update_baselines, None);
            let req = WsRequest {
                id: req_id,
                action: action.clone(),
                params,
            };
            req_id += 1;

            let resp = session.dispatch_rpc(&req).await;
            let passed = resp.ok;
            step_results.push(StepResult {
                index: idx,
                action,
                passed,
                duration_ms: step_start.elapsed().as_millis() as u64,
                output: resp.result,
                error: resp.error,
            });
            if !passed {
                case_passed = false;
                break;
            }
        }

        case_reports.push(CaseReport {
            name: case.name.clone(),
            passed: case_passed,
            duration_ms: case_start.elapsed().as_millis() as u64,
            steps: step_results,
        });
    }

    let passed_cases = case_reports.iter().filter(|c| c.passed).count();
    let total_cases = case_reports.len();
    SuiteReport {
        suite: suite.name.clone(),
        passed: passed_cases == total_cases,
        total_cases,
        passed_cases,
        failed_cases: total_cases - passed_cases,
        duration_ms: suite_start.elapsed().as_millis() as u64,
        cases: case_reports,
    }
}

/// Connects to a running `ferrite-daemon` WebSocket gateway with bearer authentication and
/// executes the declarative [`TestSuite`] remotely.
pub async fn run_suite_over_ws(
    ws_url: &str,
    token: &str,
    suite: &TestSuite,
    update_baselines: bool,
) -> anyhow::Result<SuiteReport> {
    run_suite_over_ws_with_snapshots(ws_url, token, suite, update_baselines, None).await
}

/// Same as [`run_suite_over_ws`], allowing the caller to specify where screenshot baselines
/// and diff artifacts should be stored.
pub async fn run_suite_over_ws_with_snapshots(
    ws_url: &str,
    token: &str,
    suite: &TestSuite,
    update_baselines: bool,
    snapshots_dir: Option<&Path>,
) -> anyhow::Result<SuiteReport> {
    let mut request = ws_url.into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);

    let (mut ws_stream, _) = connect_async(request).await?;
    let suite_start = Instant::now();
    let mut case_reports = Vec::with_capacity(suite.cases.len());
    let mut req_id = 1u64;

    for case in &suite.cases {
        let case_start = Instant::now();
        let mut step_results = Vec::new();
        let mut case_passed = true;

        for (idx, step) in case.steps.iter().enumerate() {
            let step_start = Instant::now();
            if let TestStep::WaitMs { ms } = step {
                tokio::time::sleep(Duration::from_millis(*ms)).await;
                step_results.push(StepResult {
                    index: idx,
                    action: "wait_ms".to_string(),
                    passed: true,
                    duration_ms: step_start.elapsed().as_millis() as u64,
                    output: Some(json!({ "sleptMs": ms })),
                    error: None,
                });
                continue;
            }

            let (action, params) = step_to_rpc(step, case.tab_id, update_baselines, snapshots_dir);
            let current_id = req_id;
            req_id += 1;
            let req = WsRequest {
                id: current_id,
                action: action.clone(),
                params,
            };

            ws_stream
                .send(Message::Text(serde_json::to_string(&req)?.into()))
                .await?;

            let resp = loop {
                match ws_stream.next().await {
                    Some(Ok(Message::Text(txt))) => {
                        if let Ok(parsed) = serde_json::from_str::<WsResponse>(&txt)
                            && parsed.id == current_id
                        {
                            break parsed;
                        }
                    }
                    Some(Ok(Message::Close(_))) | None => {
                        anyhow::bail!("WebSocket closed before response for request {current_id}");
                    }
                    Some(Ok(_)) => continue,
                    Some(Err(err)) => return Err(err.into()),
                }
            };

            let passed = resp.ok;
            step_results.push(StepResult {
                index: idx,
                action,
                passed,
                duration_ms: step_start.elapsed().as_millis() as u64,
                output: resp.result,
                error: resp.error,
            });
            if !passed {
                case_passed = false;
                break;
            }
        }

        case_reports.push(CaseReport {
            name: case.name.clone(),
            passed: case_passed,
            duration_ms: case_start.elapsed().as_millis() as u64,
            steps: step_results,
        });
    }

    let _ = ws_stream.close(None).await;

    let passed_cases = case_reports.iter().filter(|c| c.passed).count();
    let total_cases = case_reports.len();
    Ok(SuiteReport {
        suite: suite.name.clone(),
        passed: passed_cases == total_cases,
        total_cases,
        passed_cases,
        failed_cases: total_cases - passed_cases,
        duration_ms: suite_start.elapsed().as_millis() as u64,
        cases: case_reports,
    })
}

/// Sends a single authenticated RPC call to a running `ferrite-daemon` WebSocket server.
pub async fn send_ws_rpc(
    ws_url: &str,
    token: &str,
    action: &str,
    params: Value,
) -> anyhow::Result<Value> {
    let mut request = ws_url.into_client_request()?;
    request
        .headers_mut()
        .insert("Authorization", format!("Bearer {token}").parse()?);

    let (mut ws_stream, _) = connect_async(request).await?;
    let req = WsRequest {
        id: 1,
        action: action.to_string(),
        params,
    };
    ws_stream
        .send(Message::Text(serde_json::to_string(&req)?.into()))
        .await?;

    let resp = loop {
        match ws_stream.next().await {
            Some(Ok(Message::Text(txt))) => {
                if let Ok(parsed) = serde_json::from_str::<WsResponse>(&txt)
                    && parsed.id == 1
                {
                    break parsed;
                }
            }
            Some(Ok(Message::Close(_))) | None => {
                anyhow::bail!("WebSocket closed before response for `{action}`");
            }
            Some(Ok(_)) => continue,
            Some(Err(err)) => return Err(err.into()),
        }
    };
    let _ = ws_stream.close(None).await;

    if resp.ok {
        Ok(resp.result.unwrap_or(Value::Null))
    } else {
        anyhow::bail!(
            "{}",
            resp.error.unwrap_or_else(|| format!("RPC `{action}` failed"))
        )
    }
}
