#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrite_core::{CdpError, CdpEvent, CdpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::sync::Notify;
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct NetworkConditions {
    pub offline: bool,
    pub latency_ms: f64,
    pub download_throughput: f64,
    pub upload_throughput: f64,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ResponseBody {
    pub body: String,
    pub base64_encoded: bool,
}

/// Ref-counted CDP `Network` domain wrapper so multiple features (network capture + offline
/// emulation) share `Network.enable` / `Network.disable` cleanly without premature teardown.
pub struct NetworkDomain {
    transport: Arc<dyn CdpTransport>,
    tab_id: i64,
    enable_refs: Mutex<usize>,
}

impl NetworkDomain {
    pub fn new(transport: Arc<dyn CdpTransport>, tab_id: i64) -> Self {
        Self {
            transport,
            tab_id,
            enable_refs: Mutex::new(0),
        }
    }

    pub fn enabled_ref_count(&self) -> usize {
        *self.enable_refs.lock().expect("network mutex poisoned")
    }

    pub async fn enable(&self) -> Result<(), CdpError> {
        let should_send = {
            let mut count = self.enable_refs.lock().expect("network mutex poisoned");
            *count += 1;
            *count == 1
        };

        if should_send {
            if let Err(err) = self
                .transport
                .send_cdp(
                    self.tab_id,
                    "Network.enable",
                    json!({ "maxPostDataSize": 65_536 }),
                    None,
                )
                .await
            {
                let mut count = self.enable_refs.lock().expect("network mutex poisoned");
                *count = count.saturating_sub(1);
                return Err(err);
            }
        }
        Ok(())
    }

    pub async fn disable(&self) -> Result<(), CdpError> {
        let should_send = {
            let mut count = self.enable_refs.lock().expect("network mutex poisoned");
            if *count == 0 {
                return Ok(());
            }
            *count -= 1;
            *count == 0
        };

        if should_send {
            self.transport
                .send_cdp(self.tab_id, "Network.disable", json!({}), None)
                .await?;
        }
        Ok(())
    }

    pub async fn set_extra_http_headers(
        &self,
        headers: HashMap<String, String>,
    ) -> Result<(), CdpError> {
        self.transport
            .send_cdp(
                self.tab_id,
                "Network.setExtraHTTPHeaders",
                json!({ "headers": headers }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn set_blocked_urls(&self, urls: Vec<String>) -> Result<(), CdpError> {
        self.transport
            .send_cdp(
                self.tab_id,
                "Network.setBlockedURLs",
                json!({ "urls": urls }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn emulate_network_conditions(
        &self,
        conditions: &NetworkConditions,
    ) -> Result<(), CdpError> {
        self.transport
            .send_cdp(
                self.tab_id,
                "Network.emulateNetworkConditions",
                json!({
                    "offline": conditions.offline,
                    "latency": conditions.latency_ms,
                    "downloadThroughput": conditions.download_throughput,
                    "uploadThroughput": conditions.upload_throughput,
                }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn get_response_body(&self, request_id: &str) -> Result<ResponseBody, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Network.getResponseBody",
                json!({ "requestId": request_id }),
                None,
            )
            .await?;
        Ok(ResponseBody {
            body: res
                .get("body")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_string(),
            base64_encoded: res
                .get("base64Encoded")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CapturedExchange {
    pub request_id: String,
    pub url: String,
    pub method: String,
    pub resource_type: String,
    pub status: Option<u16>,
    pub mime_type: Option<String>,
    pub encoded_data_length: usize,
    pub finished: bool,
    pub failed_error: Option<String>,
}

#[derive(Default)]
struct MonitorState {
    inflight: HashMap<String, CapturedExchange>,
    completed: Vec<CapturedExchange>,
    last_activity: Option<Instant>,
}

/// Captures network request/response lifecycles and supports `wait_for_network_idle`.
pub struct NetworkMonitor {
    state: Mutex<MonitorState>,
    activity_notify: Notify,
}

impl NetworkMonitor {
    pub fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(MonitorState::default()),
            activity_notify: Notify::new(),
        })
    }

    pub fn spawn_listener(self: &Arc<Self>, transport: Arc<dyn CdpTransport>, tab_id: i64) -> JoinHandle<()> {
        let mut rx = transport.subscribe_events();
        let monitor = self.clone();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.tab_id == tab_id => monitor.ingest_event(&ev),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }

    pub fn ingest_event(&self, event: &CdpEvent) {
        let mut guard = self.state.lock().expect("network monitor mutex poisoned");
        let now = Instant::now();
        match event.method.as_str() {
            "Network.requestWillBeSent" => {
                let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                let req = &event.params["request"];
                let url = req.get("url").and_then(Value::as_str).unwrap_or_default();
                let method = req.get("method").and_then(Value::as_str).unwrap_or("GET");
                let resource_type = event
                    .params
                    .get("type")
                    .and_then(Value::as_str)
                    .unwrap_or("Other");

                guard.inflight.insert(
                    request_id.to_string(),
                    CapturedExchange {
                        request_id: request_id.to_string(),
                        url: url.to_string(),
                        method: method.to_string(),
                        resource_type: resource_type.to_string(),
                        status: None,
                        mime_type: None,
                        encoded_data_length: 0,
                        finished: false,
                        failed_error: None,
                    },
                );
                guard.last_activity = Some(now);
            }
            "Network.responseReceived" => {
                let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(entry) = guard.inflight.get_mut(request_id) {
                    let resp = &event.params["response"];
                    entry.status = resp.get("status").and_then(Value::as_u64).map(|s| s as u16);
                    entry.mime_type = resp
                        .get("mimeType")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    guard.last_activity = Some(now);
                }
            }
            "Network.loadingFinished" => {
                let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(mut entry) = guard.inflight.remove(request_id) {
                    entry.encoded_data_length = event
                        .params
                        .get("encodedDataLength")
                        .and_then(Value::as_u64)
                        .unwrap_or(0) as usize;
                    entry.finished = true;
                    guard.completed.push(entry);
                    guard.last_activity = Some(now);
                }
            }
            "Network.loadingFailed" => {
                let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
                    return;
                };
                if let Some(mut entry) = guard.inflight.remove(request_id) {
                    entry.failed_error = event
                        .params
                        .get("errorText")
                        .and_then(Value::as_str)
                        .map(str::to_string);
                    entry.finished = true;
                    guard.completed.push(entry);
                    guard.last_activity = Some(now);
                }
            }
            _ => return,
        }
        drop(guard);
        self.activity_notify.notify_waiters();
    }

    pub fn completed_exchanges(&self) -> Vec<CapturedExchange> {
        self.state
            .lock()
            .expect("network monitor mutex poisoned")
            .completed
            .clone()
    }

    pub fn inflight_count(&self) -> usize {
        self.state
            .lock()
            .expect("network monitor mutex poisoned")
            .inflight
            .len()
    }

    pub async fn wait_for_network_idle(
        &self,
        idle_window: Duration,
        timeout: Duration,
    ) -> Result<(), CdpError> {
        let deadline = tokio::time::Instant::now() + timeout;
        loop {
            let (inflight, since_last) = {
                let guard = self.state.lock().expect("network monitor mutex poisoned");
                let since = guard
                    .last_activity
                    .map(|t| t.elapsed())
                    .unwrap_or(idle_window);
                (guard.inflight.len(), since)
            };

            if inflight == 0 && since_last >= idle_window {
                return Ok(());
            }

            let now = tokio::time::Instant::now();
            if now >= deadline {
                return Err(CdpError::Timeout(timeout.as_millis() as u64));
            }

            let remaining = deadline - now;
            let wait_step = if inflight == 0 {
                idle_window.saturating_sub(since_last).min(remaining)
            } else {
                remaining
            };

            tokio::select! {
                _ = self.activity_notify.notified() => {}
                _ = tokio::time::sleep(wait_step.max(Duration::from_millis(5))) => {}
            }
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct MockRouteResponse {
    pub url_substring: String,
    #[serde(default)]
    pub method: Option<String>,
    pub status: u16,
    #[serde(default)]
    pub headers: HashMap<String, String>,
    #[serde(default)]
    pub body: String,
    #[serde(default)]
    pub fail_reason: Option<String>,
}

/// Manages `Fetch.enable` + `Fetch.requestPaused` interception for deterministic API mocking.
pub struct FetchInterceptor {
    transport: Arc<dyn CdpTransport>,
    tab_id: i64,
    routes: Arc<Mutex<Vec<MockRouteResponse>>>,
    patterns: Mutex<Vec<String>>,
}

impl FetchInterceptor {
    pub fn new(transport: Arc<dyn CdpTransport>, tab_id: i64) -> Self {
        Self {
            transport,
            tab_id,
            routes: Arc::new(Mutex::new(Vec::new())),
            patterns: Mutex::new(Vec::new()),
        }
    }

    pub fn add_route(&self, route: MockRouteResponse) {
        self.routes
            .lock()
            .expect("fetch routes mutex poisoned")
            .push(route);
    }

    pub fn clear_routes(&self) {
        self.routes
            .lock()
            .expect("fetch routes mutex poisoned")
            .clear();
        self.patterns
            .lock()
            .expect("fetch patterns mutex poisoned")
            .clear();
    }

    pub async fn enable(&self, url_patterns: Vec<String>) -> Result<(), CdpError> {
        let patterns: Vec<Value> = {
            let mut stored = self.patterns.lock().expect("fetch patterns mutex poisoned");
            if url_patterns.is_empty() {
                if !stored.iter().any(|p| p == "*") {
                    stored.push("*".to_string());
                }
            } else {
                for pat in url_patterns {
                    if !stored.iter().any(|existing| existing == &pat) {
                        stored.push(pat);
                    }
                }
            }

            stored
                .iter()
                .map(|p| json!({ "urlPattern": p, "requestStage": "Request" }))
                .collect()
        };

        self.transport
            .send_cdp(
                self.tab_id,
                "Fetch.enable",
                json!({ "patterns": patterns }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn disable(&self) -> Result<(), CdpError> {
        self.transport
            .send_cdp(self.tab_id, "Fetch.disable", json!({}), None)
            .await?;
        Ok(())
    }

    pub async fn handle_paused_event(&self, event: &CdpEvent) -> Result<bool, CdpError> {
        if event.tab_id != self.tab_id || event.method != "Fetch.requestPaused" {
            return Ok(false);
        }
        let Some(request_id) = event.params.get("requestId").and_then(Value::as_str) else {
            return Ok(false);
        };
        let req_obj = event.params.get("request");
        let url = req_obj
            .and_then(|r| r.get("url"))
            .and_then(Value::as_str)
            .unwrap_or_default();
        let req_method = req_obj
            .and_then(|r| r.get("method"))
            .and_then(Value::as_str)
            .unwrap_or("GET");

        let matched = {
            let routes = self.routes.lock().expect("fetch routes mutex poisoned");
            // Reverse search so a later mock overrides an earlier mock for the same endpoint.
            routes
                .iter()
                .rev()
                .find(|r| {
                    let method_ok = match r.method.as_deref() {
                        Some(m) if !m.is_empty() && m != "*" && !m.eq_ignore_ascii_case("ANY") => {
                            m.eq_ignore_ascii_case(req_method)
                        }
                        _ => true,
                    };
                    method_ok && url.contains(&r.url_substring)
                })
                .cloned()
        };

        let session_id = event.session_id.as_deref();
        match matched {
            Some(route) if route.fail_reason.is_some() => {
                self.transport
                    .send_cdp(
                        self.tab_id,
                        "Fetch.failRequest",
                        json!({
                            "requestId": request_id,
                            "errorReason": route.fail_reason.unwrap_or_else(|| "Failed".to_string()),
                        }),
                        session_id,
                    )
                    .await?;
            }
            Some(route) => {
                let mut headers = route.headers;
                let has_acao = headers
                    .keys()
                    .any(|k| k.eq_ignore_ascii_case("access-control-allow-origin"));
                if !has_acao
                    && let Some(origin) = req_obj
                        .and_then(|r| r.get("headers"))
                        .and_then(Value::as_object)
                        .and_then(|h| {
                            h.iter()
                                .find(|(k, _)| k.eq_ignore_ascii_case("origin"))
                                .and_then(|(_, v)| v.as_str())
                        })
                {
                    headers.insert("access-control-allow-origin".to_string(), origin.to_string());
                    headers.insert(
                        "access-control-allow-credentials".to_string(),
                        "true".to_string(),
                    );
                }

                let response_headers: Vec<Value> = headers
                    .into_iter()
                    .map(|(name, value)| json!({ "name": name, "value": value }))
                    .collect();
                self.transport
                    .send_cdp(
                        self.tab_id,
                        "Fetch.fulfillRequest",
                        json!({
                            "requestId": request_id,
                            "responseCode": route.status,
                            "responseHeaders": response_headers,
                            "body": STANDARD.encode(route.body.as_bytes()),
                        }),
                        session_id,
                    )
                    .await?;
            }
            None => {
                self.transport
                    .send_cdp(
                        self.tab_id,
                        "Fetch.continueRequest",
                        json!({ "requestId": request_id }),
                        session_id,
                    )
                    .await?;
            }
        }

        Ok(true)
    }

    pub fn spawn_interceptor(self: Arc<Self>) -> JoinHandle<()> {
        let mut rx = self.transport.subscribe_events();
        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) => {
                        if let Err(err) = self.handle_paused_event(&ev).await {
                            tracing::warn!("FetchInterceptor error: {err}");
                        }
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::mock::MockCdpTransport;

    #[tokio::test]
    async fn refcounts_enable_and_disable() {
        let mock = MockCdpTransport::new();
        let net = NetworkDomain::new(mock.clone(), 1);

        net.enable().await.unwrap();
        net.enable().await.unwrap();
        assert_eq!(net.enabled_ref_count(), 2);

        net.disable().await.unwrap();
        assert_eq!(net.enabled_ref_count(), 1);
        assert_eq!(
            mock.recorded_calls()
                .iter()
                .filter(|c| c.method == "Network.enable")
                .count(),
            1
        );
        assert_eq!(
            mock.recorded_calls()
                .iter()
                .filter(|c| c.method == "Network.disable")
                .count(),
            0
        );

        net.disable().await.unwrap();
        assert_eq!(net.enabled_ref_count(), 0);
        assert_eq!(
            mock.recorded_calls()
                .iter()
                .filter(|c| c.method == "Network.disable")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn monitor_records_exchanges_and_fetch_mocks_paused_request() {
        let mock = MockCdpTransport::new();
        let monitor = NetworkMonitor::new();

        monitor.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Network.requestWillBeSent".to_string(),
            params: json!({
                "requestId": "req-1",
                "type": "XHR",
                "request": { "url": "https://api.example/users", "method": "GET" }
            }),
            session_id: None,
        });
        assert_eq!(monitor.inflight_count(), 1);

        monitor.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Network.responseReceived".to_string(),
            params: json!({
                "requestId": "req-1",
                "response": { "status": 200, "mimeType": "application/json" }
            }),
            session_id: None,
        });
        monitor.ingest_event(&CdpEvent {
            tab_id: 1,
            method: "Network.loadingFinished".to_string(),
            params: json!({ "requestId": "req-1", "encodedDataLength": 128 }),
            session_id: None,
        });
        assert_eq!(monitor.inflight_count(), 0);
        assert_eq!(monitor.completed_exchanges()[0].status, Some(200));

        let interceptor = FetchInterceptor::new(mock.clone(), 1);
        interceptor.add_route(MockRouteResponse {
            url_substring: "/api/profile".to_string(),
            method: Some("GET".to_string()),
            status: 200,
            headers: HashMap::from([("content-type".to_string(), "application/json".to_string())]),
            body: r#"{"name":"Ada"}"#.to_string(),
            fail_reason: None,
        });

        // A POST to /api/profile should NOT match a GET-only mock and must continue unmodified.
        interceptor
            .handle_paused_event(&CdpEvent {
                tab_id: 1,
                method: "Fetch.requestPaused".to_string(),
                params: json!({
                    "requestId": "pause-post",
                    "request": { "url": "https://app.example/api/profile", "method": "POST" }
                }),
                session_id: None,
            })
            .await
            .unwrap();

        let handled = interceptor
            .handle_paused_event(&CdpEvent {
                tab_id: 1,
                method: "Fetch.requestPaused".to_string(),
                params: json!({
                    "requestId": "pause-9",
                    "request": { "url": "https://app.example/api/profile", "method": "GET" }
                }),
                session_id: None,
            })
            .await
            .unwrap();
        assert!(handled);

        let calls = mock.recorded_calls();
        assert_eq!(calls[0].method, "Fetch.continueRequest");
        assert_eq!(calls[1].method, "Fetch.fulfillRequest");
        assert_eq!(calls[1].params["responseCode"], 200);
        assert_eq!(
            calls[1].params["body"],
            STANDARD.encode(br#"{"name":"Ada"}"#)
        );
    }

    #[tokio::test]
    async fn fetch_enable_merges_and_deduplicates_patterns_across_calls() {
        let mock = MockCdpTransport::new();
        let interceptor = FetchInterceptor::new(mock.clone(), 1);

        interceptor
            .enable(vec!["*://example.com/login*".to_string()])
            .await
            .unwrap();
        interceptor
            .enable(vec![
                "*://example.com/login*".to_string(),
                "*://*/api/*".to_string(),
            ])
            .await
            .unwrap();

        let calls = mock.recorded_calls();
        assert_eq!(calls.len(), 2);
        let second_patterns = calls[1].params["patterns"].as_array().unwrap();
        assert_eq!(second_patterns.len(), 2);
        assert_eq!(second_patterns[0]["urlPattern"], "*://example.com/login*");
        assert_eq!(second_patterns[1]["urlPattern"], "*://*/api/*");
    }
}
