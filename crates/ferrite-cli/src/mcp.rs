#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

use ferrite_daemon::{
    default_connection_file, read_connection_file, run_suite_over_ws_with_snapshots, send_ws_rpc,
};
use ferrite_dsl::{compile_path, compile_source};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

const PROTOCOL_VERSION_LATEST: &str = "2025-03-26";
const SUPPORTED_PROTOCOL_VERSIONS: &[&str] = &["2025-03-26", "2024-11-05"];

const SERVER_INSTRUCTIONS: &str = "Ferrite MCP gives you direct control over the user's attached Chrome tabs via Chrome DevTools Protocol (CDP). \
Start with `browser_list_tabs` (use `attachedOnly: true` to see tabs the user already attached in the Ferrite extension) to get a `tabId`. \
Use `browser_evaluate` to execute JavaScript (supports top-level `await`) in an attached tab, `browser_snapshot` to get a compact Accessibility Tree with `@e1`, `@e2` refs, \
`browser_click` / `browser_fill` / `browser_press_key` to interact, `browser_screenshot` to visually inspect the page, and `browser_run_ferrite` to run `.ferrite` E2E scripts.";

/// Configuration for the stdio MCP server connecting to the Ferrite Native Messaging daemon.
#[derive(Debug, Clone)]
pub struct McpConfig {
    pub ws_url: Option<String>,
    pub token: Option<String>,
    pub default_tab_id: Option<i64>,
    pub snapshots_dir: Option<PathBuf>,
}

/// Model Context Protocol (MCP) JSON-RPC 2.0 server over stdio backed by the live Ferrite daemon.
pub struct McpServer {
    config: McpConfig,
}

impl McpServer {
    pub fn new(config: McpConfig) -> Self {
        Self { config }
    }

    /// Runs the stdio JSON-RPC 2.0 event loop until stdin closes.
    pub async fn run_stdio(&self) -> anyhow::Result<()> {
        let stdin = tokio::io::stdin();
        let mut stdout = tokio::io::stdout();
        let mut lines = BufReader::new(stdin).lines();

        while let Some(line) = lines.next_line().await? {
            let trimmed = line.trim();
            if trimmed.is_empty() {
                continue;
            }
            if let Some(response) = self.handle_raw_message(trimmed).await {
                let mut encoded = serde_json::to_string(&response)?;
                encoded.push('\n');
                stdout.write_all(encoded.as_bytes()).await?;
                stdout.flush().await?;
            }
        }
        Ok(())
    }

    fn resolve_daemon(&self) -> anyhow::Result<(String, String)> {
        match (&self.config.ws_url, &self.config.token) {
            (Some(u), Some(t)) => Ok((u.clone(), t.clone())),
            (opt_u, opt_t) => {
                let conn_path = default_connection_file();
                let conn = read_connection_file(&conn_path).map_err(|err| {
                    anyhow::anyhow!(
                        "Ferrite Chrome daemon is not reachable (could not read {}: {err}). \
                         Open Chrome with the Ferrite E2E extension enabled (status: Connected) and ensure `ferrite setup --extension-id <id>` was completed.",
                        conn_path.display()
                    )
                })?;
                Ok((
                    opt_u
                        .clone()
                        .unwrap_or_else(|| format!("ws://127.0.0.1:{}", conn.port)),
                    opt_t.clone().unwrap_or(conn.token),
                ))
            }
        }
    }

    async fn call_daemon(&self, action: &str, params: Value) -> anyhow::Result<Value> {
        let (ws_url, token) = self.resolve_daemon()?;
        send_ws_rpc(&ws_url, &token, action, params).await
    }

    fn resolve_tab_arg(&self, args: &Value) -> Option<i64> {
        args.get("tabId")
            .and_then(Value::as_i64)
            .or(self.config.default_tab_id)
    }

    /// Processes a single JSON-RPC 2.0 message line and returns an optional JSON-RPC response
    /// (`None` for notifications).
    pub async fn handle_raw_message(&self, raw: &str) -> Option<Value> {
        let msg: Value = match serde_json::from_str(raw) {
            Ok(v) => v,
            Err(err) => {
                return Some(jsonrpc_error(
                    Value::Null,
                    -32700,
                    &format!("Parse error: {err}"),
                ));
            }
        };

        let id = msg.get("id").cloned();
        let Some(method) = msg.get("method").and_then(Value::as_str) else {
            return id.map(|req_id| jsonrpc_error(req_id, -32600, "Invalid Request: missing `method`"));
        };
        let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));

        // JSON-RPC notifications (no `id`) must never receive a response.
        if id.is_none() || method.starts_with("notifications/") {
            tracing::debug!(method, "received MCP notification");
            return None;
        }
        let req_id = id.expect("checked is_some");

        let response = match method {
            "initialize" => self.handle_initialize(&params),
            "ping" => Ok(json!({})),
            "tools/list" => Ok(json!({ "tools": tool_definitions() })),
            "tools/call" => Ok(self.handle_tool_call(&params).await),
            "resources/list" => Ok(json!({
                "resources": [
                    {
                        "uri": "ferrite://tabs",
                        "name": "Chrome Tabs",
                        "description": "Live list of Chrome tabs with tabId, URL, title, and debugger attachment status.",
                        "mimeType": "application/json"
                    }
                ]
            })),
            "resources/templates/list" => Ok(json!({
                "resourceTemplates": [
                    {
                        "uriTemplate": "ferrite://tabs/{tabId}/snapshot",
                        "name": "Tab Accessibility Snapshot",
                        "description": "Compact Accessibility Tree snapshot with @eN refs for a specific Chrome tabId.",
                        "mimeType": "text/plain"
                    }
                ]
            })),
            "resources/read" => self.handle_resource_read(&params).await,
            other => Err((-32601, format!("Method not found: {other}"))),
        };

        match response {
            Ok(result) => Some(json!({
                "jsonrpc": "2.0",
                "id": req_id,
                "result": result,
            })),
            Err((code, message)) => Some(jsonrpc_error(req_id, code, &message)),
        }
    }

    fn handle_initialize(&self, params: &Value) -> Result<Value, (i32, String)> {
        let requested = params
            .get("protocolVersion")
            .and_then(Value::as_str)
            .unwrap_or(PROTOCOL_VERSION_LATEST);
        let negotiated = if SUPPORTED_PROTOCOL_VERSIONS.contains(&requested) {
            requested
        } else {
            PROTOCOL_VERSION_LATEST
        };

        Ok(json!({
            "protocolVersion": negotiated,
            "capabilities": {
                "tools": { "listChanged": false },
                "resources": { "subscribe": false, "listChanged": false }
            },
            "serverInfo": {
                "name": "ferrite-mcp",
                "version": env!("CARGO_PKG_VERSION")
            },
            "instructions": SERVER_INSTRUCTIONS
        }))
    }

    async fn handle_resource_read(&self, params: &Value) -> Result<Value, (i32, String)> {
        let uri = params
            .get("uri")
            .and_then(Value::as_str)
            .ok_or_else(|| (-32602, "missing `uri`".to_string()))?;

        if uri == "ferrite://tabs" {
            let res = self
                .call_daemon("tab.list", json!({}))
                .await
                .map_err(|e| (-32000, e.to_string()))?;
            let text = serde_json::to_string_pretty(&res).unwrap_or_else(|_| "{}".to_string());
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "application/json",
                    "text": text
                }]
            }));
        }

        if let Some(rest) = uri.strip_prefix("ferrite://tabs/")
            && let Some(tab_str) = rest.strip_suffix("/snapshot")
            && let Ok(tab_id) = tab_str.parse::<i64>()
        {
            let res = self
                .call_daemon(
                    "snapshot",
                    json!({ "tabId": tab_id, "interactiveOnly": true }),
                )
                .await
                .map_err(|e| (-32000, e.to_string()))?;
            let compact = res
                .get("compact")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string();
            return Ok(json!({
                "contents": [{
                    "uri": uri,
                    "mimeType": "text/plain",
                    "text": compact
                }]
            }));
        }

        Err((-32602, format!("unsupported resource URI: {uri}")))
    }

    async fn handle_tool_call(&self, params: &Value) -> Value {
        let Some(name) = params.get("name").and_then(Value::as_str) else {
            return tool_error("missing tool `name`");
        };
        let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));

        match self.dispatch_tool(name, &args).await {
            Ok(result) => result,
            Err(err) => tool_error(&err.to_string()),
        }
    }

    async fn dispatch_tool(&self, name: &str, args: &Value) -> anyhow::Result<Value> {
        let tab_id = self.resolve_tab_arg(args);
        match name {
            "browser_list_tabs" => {
                let attached_only = args
                    .get("attachedOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut res = self.call_daemon("tab.list", json!({})).await?;
                if let Some(tabs) = res.get_mut("tabs").and_then(Value::as_array_mut) {
                    if attached_only {
                        tabs.retain(|t| t.get("attached").and_then(Value::as_bool).unwrap_or(false));
                    }
                    for tab in tabs.iter_mut() {
                        let attachable = ferrite_daemon::is_attachable_url(
                            tab.get("url").and_then(Value::as_str).unwrap_or_default(),
                        );
                        if let Some(obj) = tab.as_object_mut() {
                            obj.insert("attachable".to_string(), Value::Bool(attachable));
                        }
                    }
                }
                Ok(tool_json(&res))
            }

            "browser_attach_tab" => {
                if let Some(create_url) = args.get("createUrl").and_then(Value::as_str) {
                    let active = args.get("active").and_then(Value::as_bool).unwrap_or(true);
                    let res = self
                        .call_daemon("tab.create", json!({ "url": create_url, "active": active }))
                        .await?;
                    return Ok(tool_json(&res));
                }
                let res = self
                    .call_daemon("tab.attach", json!({ "tabId": tab_id }))
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_detach_tab" => {
                let res = self
                    .call_daemon("tab.detach", json!({ "tabId": tab_id }))
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_navigate" => {
                let url = req_str(args, "url")?;
                let wait_until = args
                    .get("waitUntil")
                    .and_then(Value::as_str)
                    .unwrap_or("load");
                let timeout_ms = args
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(15_000);
                let res = self
                    .call_daemon(
                        "goto",
                        json!({
                            "tabId": tab_id,
                            "url": url,
                            "waitUntil": wait_until,
                            "timeoutMs": timeout_ms,
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_reload" => {
                let ignore_cache = args
                    .get("ignoreCache")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let res = self
                    .call_daemon(
                        "reload",
                        json!({ "tabId": tab_id, "ignoreCache": ignore_cache }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_evaluate" => {
                let expression = req_str(args, "expression")?;
                let await_promise = args
                    .get("awaitPromise")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let isolated_world = args
                    .get("isolatedWorld")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let max_output_chars = args
                    .get("maxOutputChars")
                    .and_then(Value::as_u64)
                    .unwrap_or(32_000);
                let res = self
                    .call_daemon(
                        "evaluate",
                        json!({
                            "tabId": tab_id,
                            "expression": expression,
                            "awaitPromise": await_promise,
                            "isolatedWorld": isolated_world,
                            "maxOutputChars": max_output_chars,
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_snapshot" => {
                let interactive_only = args
                    .get("interactiveOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let res = self
                    .call_daemon(
                        "snapshot",
                        json!({ "tabId": tab_id, "interactiveOnly": interactive_only }),
                    )
                    .await?;
                let resolved_tab = res.get("tabId").and_then(Value::as_i64).unwrap_or(0);
                let compact = res
                    .get("compact")
                    .and_then(Value::as_str)
                    .unwrap_or_default();
                let header = format!(
                    "Tab {resolved_tab} Accessibility Snapshot (interactiveOnly={interactive_only}):\n"
                );
                let body = if compact.is_empty() {
                    "(empty accessibility tree)".to_string()
                } else {
                    compact.to_string()
                };
                Ok(tool_text(&format!("{header}{body}")))
            }

            "browser_click" => {
                let double_click = args
                    .get("doubleClick")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                if let Some(selector) = args.get("css").and_then(Value::as_str) {
                    let action = if double_click { "dblclick_css" } else { "click_css" };
                    let res = self
                        .call_daemon(
                            action,
                            json!({
                                "tabId": tab_id,
                                "selector": selector,
                                "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                                "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                            }),
                        )
                        .await?;
                    return Ok(tool_json(&res));
                }
                if let Some(ref_id) = args.get("refId").and_then(Value::as_str) {
                    let action = if double_click { "dblclick_ref" } else { "click_ref" };
                    let res = self
                        .call_daemon(action, json!({ "tabId": tab_id, "refId": ref_id }))
                        .await?;
                    return Ok(tool_json(&res));
                }
                if double_click && let Some(text) = args.get("text").and_then(Value::as_str) {
                    let res = self
                        .call_daemon(
                            "dblclick_text",
                            json!({
                                "tabId": tab_id,
                                "text": text,
                                "exact": args.get("exact").and_then(Value::as_bool).unwrap_or(false),
                                "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                                "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                            }),
                        )
                        .await?;
                    return Ok(tool_json(&res));
                }
                let role = req_str(args, "role")?;
                let res = self
                    .call_daemon(
                        "click_role",
                        json!({
                            "tabId": tab_id,
                            "role": role,
                            "name": args.get("name").and_then(Value::as_str),
                            "exact": args.get("exact").and_then(Value::as_bool).unwrap_or(false),
                            "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                            "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_fill" => {
                let text = req_str(args, "text")?;
                if let Some(selector) = args.get("css").and_then(Value::as_str) {
                    let res = self
                        .call_daemon(
                            "fill_css",
                            json!({
                                "tabId": tab_id,
                                "selector": selector,
                                "text": text,
                                "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                                "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                            }),
                        )
                        .await?;
                    return Ok(tool_json(&res));
                }
                if let Some(ref_id) = args.get("refId").and_then(Value::as_str) {
                    let res = self
                        .call_daemon(
                            "fill_ref",
                            json!({ "tabId": tab_id, "refId": ref_id, "text": text }),
                        )
                        .await?;
                    return Ok(tool_json(&res));
                }
                let role = req_str(args, "role")?;
                let res = self
                    .call_daemon(
                        "fill_role",
                        json!({
                            "tabId": tab_id,
                            "role": role,
                            "name": args.get("name").and_then(Value::as_str),
                            "text": text,
                            "exact": args.get("exact").and_then(Value::as_bool).unwrap_or(false),
                            "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                            "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_hover" => {
                if let Some(selector) = args.get("css").and_then(Value::as_str) {
                    let res = self
                        .call_daemon(
                            "hover_css",
                            json!({
                                "tabId": tab_id,
                                "selector": selector,
                                "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                                "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                            }),
                        )
                        .await?;
                    return Ok(tool_json(&res));
                }
                if let Some(ref_id) = args.get("refId").and_then(Value::as_str) {
                    let res = self
                        .call_daemon("hover_ref", json!({ "tabId": tab_id, "refId": ref_id }))
                        .await?;
                    return Ok(tool_json(&res));
                }
                let text = req_str(args, "text")?;
                let res = self
                    .call_daemon(
                        "hover_text",
                        json!({
                            "tabId": tab_id,
                            "text": text,
                            "exact": args.get("exact").and_then(Value::as_bool).unwrap_or(false),
                            "nth": args.get("nth").and_then(Value::as_u64).unwrap_or(0),
                            "timeoutMs": args.get("timeoutMs").and_then(Value::as_u64).unwrap_or(5_000),
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_press_key" => {
                if let Some(type_text) = args.get("typeText").and_then(Value::as_str) {
                    let res = self
                        .call_daemon("type_text", json!({ "tabId": tab_id, "text": type_text }))
                        .await?;
                    return Ok(tool_json(&res));
                }
                let key = req_str(args, "key")?;
                let res = self
                    .call_daemon("press_key", json!({ "tabId": tab_id, "key": key }))
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_screenshot" => {
                let full_page = args
                    .get("fullPage")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let res = self
                    .call_daemon(
                        "screenshot",
                        json!({ "tabId": tab_id, "fullPage": full_page }),
                    )
                    .await?;
                let b64 = res
                    .get("dataBase64")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("daemon omitted screenshot dataBase64"))?;
                let bytes = res.get("bytes").and_then(Value::as_u64).unwrap_or(0);
                let resolved_tab = res.get("tabId").and_then(Value::as_i64).unwrap_or(0);
                Ok(json!({
                    "content": [
                        {
                            "type": "image",
                            "data": b64,
                            "mimeType": "image/png"
                        },
                        {
                            "type": "text",
                            "text": format!("Captured PNG screenshot of tab {resolved_tab} ({bytes} bytes, fullPage={full_page}).")
                        }
                    ]
                }))
            }

            "browser_console" => {
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("list");
                let res = match action {
                    "start" => {
                        self.call_daemon("console.start", json!({ "tabId": tab_id }))
                            .await?
                    }
                    "clear" => {
                        self.call_daemon("console.clear", json!({ "tabId": tab_id }))
                            .await?
                    }
                    "list" => {
                        self.call_daemon(
                            "console.list",
                            json!({
                                "tabId": tab_id,
                                "errorsOnly": args.get("errorsOnly").and_then(Value::as_bool).unwrap_or(false),
                                "limit": args.get("limit").and_then(Value::as_u64).unwrap_or(100),
                            }),
                        )
                        .await?
                    }
                    other => anyhow::bail!("invalid console action `{other}`; expected start|list|clear"),
                };
                Ok(tool_json(&res))
            }

            "browser_network" => {
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("list");
                let res = match action {
                    "start" => {
                        self.call_daemon("network.monitor_start", json!({ "tabId": tab_id }))
                            .await?
                    }
                    "list" => {
                        self.call_daemon(
                            "network.list",
                            json!({
                                "tabId": tab_id,
                                "urlContains": args.get("urlContains").and_then(Value::as_str),
                                "limit": args.get("limit").and_then(Value::as_u64).unwrap_or(100),
                            }),
                        )
                        .await?
                    }
                    other => anyhow::bail!("invalid network action `{other}`; expected start|list"),
                };
                Ok(tool_json(&res))
            }

            "browser_mock" => {
                let action = args
                    .get("action")
                    .and_then(Value::as_str)
                    .unwrap_or("add");
                if action == "clear" {
                    let res = self
                        .call_daemon("mock.clear", json!({ "tabId": tab_id }))
                        .await?;
                    return Ok(tool_json(&res));
                }
                if action != "add" {
                    anyhow::bail!("invalid mock action `{action}`; expected add|clear");
                }

                let url_substring = req_str(args, "url")?;
                let method = args
                    .get("method")
                    .and_then(Value::as_str)
                    .map(|m| m.to_ascii_uppercase());
                let status = args.get("status").and_then(Value::as_u64).unwrap_or(200) as u16;
                if !(100..=599).contains(&status) {
                    anyhow::bail!("mock HTTP status must be between 100 and 599, got {status}");
                }

                let mut headers: std::collections::HashMap<String, String> = args
                    .get("headers")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();

                let body = if let Some(file_path) = args.get("file").and_then(Value::as_str) {
                    let p = Path::new(file_path);
                    if !headers
                        .keys()
                        .any(|k| k.eq_ignore_ascii_case("content-type"))
                        && p.extension()
                            .and_then(|e| e.to_str())
                            .is_some_and(|e| e.eq_ignore_ascii_case("json"))
                    {
                        headers.insert(
                            "content-type".to_string(),
                            "application/json; charset=utf-8".to_string(),
                        );
                    }
                    std::fs::read_to_string(p)?
                } else {
                    args.get("body")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string()
                };

                let patterns: Vec<String> = args
                    .get("patterns")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();

                let params_map: std::collections::HashMap<String, String> = args
                    .get("params")
                    .and_then(Value::as_object)
                    .map(|m| {
                        m.iter()
                            .map(|(k, v)| {
                                let s = match v {
                                    Value::String(s) => s.clone(),
                                    other => other.to_string(),
                                };
                                (k.clone(), s)
                            })
                            .collect()
                    })
                    .unwrap_or_default();

                let data = args.get("data").cloned();
                let delay_ms = args.get("delayMs").and_then(Value::as_u64).unwrap_or(0);
                let fail_reason = args.get("fail").and_then(Value::as_str).map(str::to_string);

                let res = self
                    .call_daemon(
                        "intercept_network",
                        json!({
                            "tabId": tab_id,
                            "route": {
                                "urlSubstring": url_substring,
                                "method": method,
                                "params": params_map,
                                "data": data,
                                "status": status,
                                "headers": headers,
                                "body": body,
                                "delayMs": delay_ms,
                                "failReason": fail_reason,
                            },
                            "patterns": patterns,
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            "browser_run_ferrite" => {
                let update_baselines = args
                    .get("updateBaselines")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let mut suites = if let Some(source) = args.get("source").and_then(Value::as_str) {
                    vec![compile_source("mcp-inline.ferrite", Path::new("."), source)?]
                } else if let Some(path_str) = args.get("path").and_then(Value::as_str) {
                    compile_path(Path::new(path_str))?
                        .into_iter()
                        .map(|(_, s)| s)
                        .collect()
                } else {
                    anyhow::bail!("provide either `source` (inline .ferrite DSL) or `path` (.ferrite file/dir)")
                };

                if let Some(tid) = tab_id {
                    for suite in &mut suites {
                        for case in &mut suite.cases {
                            if case.tab_id.is_none() {
                                case.tab_id = Some(tid);
                            }
                        }
                    }
                }

                let (ws_url, token) = self.resolve_daemon()?;
                let mut reports = Vec::with_capacity(suites.len());
                for suite in &suites {
                    let report = run_suite_over_ws_with_snapshots(
                        &ws_url,
                        &token,
                        suite,
                        update_baselines,
                        self.config.snapshots_dir.as_deref(),
                    )
                    .await?;
                    reports.push(report);
                }

                let all_passed = reports.iter().all(|r| r.passed);
                let payload = json!({
                    "passed": all_passed,
                    "suites": reports,
                });
                let mut out = tool_json(&payload);
                if !all_passed && let Some(obj) = out.as_object_mut() {
                    obj.insert("isError".to_string(), Value::Bool(true));
                }
                Ok(out)
            }

            "browser_cdp" => {
                let method = req_str(args, "method")?;
                let cdp_params = args.get("params").cloned().unwrap_or_else(|| json!({}));
                let session_id = args.get("sessionId").and_then(Value::as_str);
                let res = self
                    .call_daemon(
                        "raw_cdp",
                        json!({
                            "tabId": tab_id,
                            "method": method,
                            "params": cdp_params,
                            "sessionId": session_id,
                        }),
                    )
                    .await?;
                Ok(tool_json(&res))
            }

            other => anyhow::bail!("unknown MCP tool `{other}`"),
        }
    }
}

fn req_str<'a>(args: &'a Value, field: &str) -> anyhow::Result<&'a str> {
    args.get(field)
        .and_then(Value::as_str)
        .ok_or_else(|| anyhow::anyhow!("missing required string argument `{field}`"))
}

fn jsonrpc_error(id: Value, code: i32, message: &str) -> Value {
    json!({
        "jsonrpc": "2.0",
        "id": id,
        "error": {
            "code": code,
            "message": message,
        }
    })
}

fn tool_text(text: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": text }]
    })
}

fn tool_json(val: &Value) -> Value {
    let text = serde_json::to_string_pretty(val).unwrap_or_else(|_| val.to_string());
    tool_text(&text)
}

fn tool_error(message: &str) -> Value {
    json!({
        "content": [{ "type": "text", "text": message }],
        "isError": true
    })
}

fn tool_definitions() -> Value {
    json!([
        {
            "name": "browser_list_tabs",
            "description": "List open Chrome tabs with their tabId, title, URL, active state, debugger attachment state, and whether chrome.debugger can attach to them.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "attachedOnly": {
                        "type": "boolean",
                        "description": "When true, only return tabs currently attached to the Ferrite debugger."
                    }
                }
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "browser_attach_tab",
            "description": "Attach Chrome debugger to an existing tabId, or create and attach a new tab when `createUrl` is provided.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer", "description": "Existing Chrome tabId to attach." },
                    "createUrl": { "type": "string", "description": "If provided, creates a new Chrome tab at this URL and attaches it." },
                    "active": { "type": "boolean", "description": "Whether the newly created tab should become active (default true)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "browser_detach_tab",
            "description": "Detach Chrome debugger from a tab and clean up its background listeners.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer", "description": "Chrome tabId to detach (defaults to active attached tab)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false, "idempotentHint": true }
        },
        {
            "name": "browser_navigate",
            "description": "Navigate an attached Chrome tab to a URL and wait for the requested load state.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "url": { "type": "string", "description": "Destination URL." },
                    "tabId": { "type": "integer", "description": "Target tabId (defaults to active attached tab)." },
                    "waitUntil": { "type": "string", "enum": ["commit", "domContentLoaded", "load"], "description": "Load state to wait for (default: load)." },
                    "timeoutMs": { "type": "integer", "description": "Navigation timeout in milliseconds (default: 15000)." }
                },
                "required": ["url"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_reload",
            "description": "Reload an attached Chrome tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "ignoreCache": { "type": "boolean", "description": "Bypass browser cache when reloading (default false)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_evaluate",
            "description": "Execute JavaScript code inside an attached Chrome tab and return its JSON-serialized result. Supports top-level `await` (replMode) and returns detailed error messages on exceptions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "expression": { "type": "string", "description": "JavaScript code or expression to execute in the tab (e.g. `document.title` or `await fetch('/api/me').then(r => r.json())`)." },
                    "tabId": { "type": "integer", "description": "Target attached tabId (defaults to active attached tab)." },
                    "awaitPromise": { "type": "boolean", "description": "Await returned promises (default true)." },
                    "isolatedWorld": { "type": "boolean", "description": "Run in an isolated V8 world so page globals are untouched (default false, meaning page globals/React/window are accessible)." },
                    "maxOutputChars": { "type": "integer", "description": "Maximum serialized output characters before truncation (default 32000)." }
                },
                "required": ["expression"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_snapshot",
            "description": "Capture a compact Accessibility Tree snapshot with deterministic `@e1`, `@e2` element refs for fast, low-token page understanding.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "interactiveOnly": { "type": "boolean", "description": "When true (default), include only interactive/landmark controls." }
                }
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "browser_click",
            "description": "Click (or double-click) an element by CSS selector (`css`, querySelectorAll + nth), `@eN` refId from `browser_snapshot`, ARIA role + accessible name, or visible text when `doubleClick` is true.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "css": { "type": "string", "description": "CSS selector (e.g. `#submit-btn`, `.todo-list li .toggle`)." },
                    "refId": { "type": "string", "description": "Ref ID from browser_snapshot, e.g. `e1`." },
                    "role": { "type": "string", "description": "ARIA role (e.g. `button`, `link`, `checkbox`)." },
                    "name": { "type": "string", "description": "Accessible name filter when using `role`." },
                    "text": { "type": "string", "description": "Visible text target when `doubleClick: true`." },
                    "exact": { "type": "boolean" },
                    "nth": { "type": "integer", "description": "Zero-based match index (default 0)." },
                    "doubleClick": { "type": "boolean", "description": "Perform a double-click instead of a single click." },
                    "timeoutMs": { "type": "integer", "description": "Auto-wait timeout in ms (default 5000)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_fill",
            "description": "Focus, clear, and fill an input/textbox by CSS selector (`css`), `@eN` refId, or ARIA role + accessible name.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "css": { "type": "string", "description": "CSS selector (e.g. `#email`, `input.new-todo`)." },
                    "refId": { "type": "string", "description": "Ref ID from browser_snapshot, e.g. `e2`." },
                    "role": { "type": "string", "description": "ARIA role (usually `textbox`)." },
                    "name": { "type": "string", "description": "Accessible name or placeholder." },
                    "text": { "type": "string", "description": "Text to enter into the input." },
                    "exact": { "type": "boolean" },
                    "nth": { "type": "integer" },
                    "timeoutMs": { "type": "integer" }
                },
                "required": ["text"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_hover",
            "description": "Hover the mouse pointer over an element by CSS selector (`css`), `@eN` refId, or visible text.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "css": { "type": "string", "description": "CSS selector (e.g. `.todo-list li`)." },
                    "refId": { "type": "string" },
                    "text": { "type": "string" },
                    "exact": { "type": "boolean" },
                    "nth": { "type": "integer" },
                    "timeoutMs": { "type": "integer" }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_press_key",
            "description": "Press a keyboard key (e.g. `Enter`, `Escape`, `Tab`, `Backspace`, `Control+a`) or insert raw text into the focused element.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "key": { "type": "string", "description": "Key name to press." },
                    "typeText": { "type": "string", "description": "If provided, inserts raw text directly without clearing." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_screenshot",
            "description": "Capture a PNG screenshot of an attached Chrome tab and return it as an image for visual inspection.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "fullPage": { "type": "boolean", "description": "Capture beyond the viewport (default false)." }
                }
            },
            "annotations": { "readOnlyHint": true, "destructiveHint": false }
        },
        {
            "name": "browser_console",
            "description": "Start capturing, list, or clear console messages and uncaught JS exceptions on an attached tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "action": { "type": "string", "enum": ["start", "list", "clear"], "description": "Operation to perform (default: list)." },
                    "errorsOnly": { "type": "boolean", "description": "When listing, return only errors and uncaught exceptions." },
                    "limit": { "type": "integer", "description": "Max entries to return (default 100)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_network",
            "description": "Start monitoring or list captured HTTP/XHR/Fetch network exchanges on an attached tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "action": { "type": "string", "enum": ["start", "list"], "description": "Operation to perform (default: list)." },
                    "urlContains": { "type": "string", "description": "Filter completed requests whose URL contains this substring." },
                    "limit": { "type": "integer", "description": "Max exchanges to return (default 100)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_mock",
            "description": "Intercept and mock matching HTTP/XHR/Fetch requests on an attached tab (e.g. redirect `GET real_service` to a local `mock.json` file or inline body), or clear active mocks.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "action": { "type": "string", "enum": ["add", "clear"], "description": "Add a mock route or clear all mocks on the tab (default: add)." },
                    "method": { "type": "string", "description": "Optional HTTP method filter: GET, POST, PUT, PATCH, DELETE." },
                    "url": { "type": "string", "description": "URL substring or `:param` path pattern (e.g. `/api/users/:id/orders`)." },
                    "params": { "type": "object", "description": "Path `:param` values and query parameter subset to match." },
                    "data": { "type": "object", "description": "Optional JSON request body subset to match against POST/PUT/PATCH body." },
                    "file": { "type": "string", "description": "Path to a pure response fixture file (e.g. `mock.json`) to serve as response body." },
                    "body": { "type": "string", "description": "Inline response body string (used when `file` is omitted)." },
                    "status": { "type": "integer", "description": "HTTP status code (default 200)." },
                    "delayMs": { "type": "integer", "description": "Optional artificial network latency in milliseconds." },
                    "fail": { "type": "string", "description": "Optional CDP network error reason to fail the request (e.g. `TimedOut`, `Aborted`, `Failed`)." },
                    "headers": { "type": "object", "description": "Optional response headers map." },
                    "patterns": { "type": "array", "items": { "type": "string" }, "description": "Optional CDP Fetch URL wildcard patterns (default `[*]`)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_run_ferrite",
            "description": "Compile and execute inline `.ferrite` DSL automation code or a `.ferrite` file/project against the attached Chrome tab.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "source": { "type": "string", "description": "Inline `.ferrite` DSL source code." },
                    "path": { "type": "string", "description": "Path to a `.ferrite` file or project directory." },
                    "tabId": { "type": "integer", "description": "Optional tabId to bind to test cases without an explicit `tab` statement." },
                    "updateBaselines": { "type": "boolean", "description": "Update screenshot baselines during execution (default false)." }
                }
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        },
        {
            "name": "browser_cdp",
            "description": "Send a raw Chrome DevTools Protocol (CDP) command to an attached tab (destructive profile-wide commands like clearing cookies/cache or closing Chrome are blocked).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "tabId": { "type": "integer" },
                    "method": { "type": "string", "description": "CDP method name, e.g. `DOM.getDocument` or `Emulation.setDeviceMetricsOverride`." },
                    "params": { "type": "object", "description": "CDP method parameters object." },
                    "sessionId": { "type": "string", "description": "Optional child target sessionId." }
                },
                "required": ["method"]
            },
            "annotations": { "readOnlyHint": false, "destructiveHint": false }
        }
    ])
}

#[cfg(test)]
mod tests {
    use std::fs;

    use ferrite_cdp::MockCdpTransport;
    use ferrite_core::TabInfo;
    use ferrite_daemon::{E2eSession, WsServer, generate_session_token};

    use super::*;

    async fn setup_test_mcp() -> (McpServer, std::sync::Arc<MockCdpTransport>, WsServer, PathBuf) {
        let mock = MockCdpTransport::new();
        mock.set_tabs(vec![
            TabInfo {
                tab_id: 7,
                window_id: 1,
                url: "https://app.example.com/dashboard".to_string(),
                title: "Dashboard".to_string(),
                active: true,
                attached: true,
            },
            TabInfo {
                tab_id: 8,
                window_id: 1,
                url: "chrome://extensions/".to_string(),
                title: "Extensions".to_string(),
                active: false,
                attached: false,
            },
        ]);

        let temp_dir = std::env::temp_dir().join(format!(
            "ferrite-mcp-test-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        let session = E2eSession::new(mock.clone(), &temp_dir);
        let token = generate_session_token().unwrap();
        let ws = WsServer::bind(0, session, token.clone()).await.unwrap();
        let server = McpServer::new(McpConfig {
            ws_url: Some(ws.ws_url()),
            token: Some(token),
            default_tab_id: None,
            snapshots_dir: Some(temp_dir.clone()),
        });
        (server, mock, ws, temp_dir)
    }

    #[tokio::test]
    async fn handles_mcp_initialize_tools_list_and_notifications() {
        let (server, _mock, _ws, temp_dir) = setup_test_mcp().await;

        let init = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-03-26","capabilities":{},"clientInfo":{"name":"test","version":"1.0"}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(init["result"]["protocolVersion"], "2025-03-26");
        assert_eq!(init["result"]["serverInfo"]["name"], "ferrite-mcp");

        // Notifications must not produce a response.
        assert!(
            server
                .handle_raw_message(r#"{"jsonrpc":"2.0","method":"notifications/initialized"}"#)
                .await
                .is_none()
        );

        let listed = server
            .handle_raw_message(r#"{"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}"#)
            .await
            .unwrap();
        let tools = listed["result"]["tools"].as_array().unwrap();
        assert!(tools.len() >= 15);
        assert!(tools.iter().any(|t| t["name"] == "browser_evaluate"));
        assert!(tools.iter().any(|t| t["name"] == "browser_run_ferrite"));

        let _ = fs::remove_dir_all(&temp_dir);
    }

    #[tokio::test]
    async fn lists_attached_tabs_evaluates_js_and_reads_resources() {
        let (server, mock, _ws, temp_dir) = setup_test_mcp().await;

        mock.add_handler(|_tab_id, method, params, _session| match method {
            "Runtime.evaluate" => {
                let expr = params["expression"].as_str().unwrap_or_default();
                if expr.contains("throw") {
                    Some(Ok(json!({
                        "exceptionDetails": {
                            "text": "Uncaught",
                            "lineNumber": 0,
                            "columnNumber": 1,
                            "exception": { "description": "Error: boom" }
                        }
                    })))
                } else {
                    Some(Ok(json!({
                        "result": { "type": "object", "value": { "title": "Dashboard", "count": 42 } }
                    })))
                }
            }
            "Accessibility.getFullAXTree" => Some(Ok(json!({
                "nodes": [
                    {
                        "nodeId": "1",
                        "role": { "type": "role", "value": "RootWebArea" },
                        "childIds": ["2"]
                    },
                    {
                        "nodeId": "2",
                        "parentId": "1",
                        "role": { "type": "role", "value": "button" },
                        "name": { "type": "computedString", "value": "Run Query" },
                        "backendDOMNodeId": 99,
                        "childIds": []
                    }
                ]
            }))),
            "Page.captureScreenshot" => Some(Ok(json!({ "data": "iVBORw0KGgo=" }))),
            _ => None,
        });

        // 1. Filter attached-only tabs.
        let tabs_resp = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":10,"method":"tools/call","params":{"name":"browser_list_tabs","arguments":{"attachedOnly":true}}}"#,
            )
            .await
            .unwrap();
        let tabs_text = tabs_resp["result"]["content"][0]["text"].as_str().unwrap();
        let tabs_json: Value = serde_json::from_str(tabs_text).unwrap();
        assert_eq!(tabs_json["tabs"].as_array().unwrap().len(), 1);
        assert_eq!(tabs_json["tabs"][0]["tabId"], 7);
        assert_eq!(tabs_json["tabs"][0]["attachable"], true);

        // 2. Evaluate JS on attached tab 7.
        let eval_resp = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":11,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"tabId":7,"expression":"({ title: document.title, count: 42 })"}}}"#,
            )
            .await
            .unwrap();
        assert!(eval_resp["result"].get("isError").is_none());
        let eval_json: Value =
            serde_json::from_str(eval_resp["result"]["content"][0]["text"].as_str().unwrap())
                .unwrap();
        assert_eq!(eval_json["value"]["count"], 42);

        // 3. JS exception returns MCP tool isError: true with details.
        let err_resp = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":12,"method":"tools/call","params":{"name":"browser_evaluate","arguments":{"tabId":7,"expression":"throw new Error('boom')"}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(err_resp["result"]["isError"], true);
        assert!(
            err_resp["result"]["content"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Error: boom")
        );

        // 4. Screenshot returns native MCP image content block.
        let shot_resp = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":13,"method":"tools/call","params":{"name":"browser_screenshot","arguments":{"tabId":7}}}"#,
            )
            .await
            .unwrap();
        assert_eq!(shot_resp["result"]["content"][0]["type"], "image");
        assert_eq!(shot_resp["result"]["content"][0]["mimeType"], "image/png");

        // 5. Resource read for ferrite://tabs/7/snapshot returns compact AX tree.
        let res_resp = server
            .handle_raw_message(
                r#"{"jsonrpc":"2.0","id":14,"method":"resources/read","params":{"uri":"ferrite://tabs/7/snapshot"}}"#,
            )
            .await
            .unwrap();
        assert!(
            res_resp["result"]["contents"][0]["text"]
                .as_str()
                .unwrap()
                .contains("Run Query")
        );

        let _ = fs::remove_dir_all(&temp_dir);
    }
}
