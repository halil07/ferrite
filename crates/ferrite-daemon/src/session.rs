#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use ferrite_cdp::{
    FetchInterceptor, LoadState, MockRouteResponse, NetworkDomain, NetworkMonitor, PageDomain,
    ScreenshotOptions, TargetDomain,
};
use ferrite_core::{CdpTransport, WsRequest, WsResponse};
use ferrite_locator::LocatorEngine;
use ferrite_visual::{BaselinePolicy, DiffOptions, SnapshotStore, VisualAssertionResult};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

const RESTRICTED_URL_PREFIXES: &[&str] = &[
    "chrome://",
    "chrome-extension://",
    "chrome-search://",
    "edge://",
    "devtools://",
    "view-source:",
    "https://chrome.google.com/webstore",
    "https://chromewebstore.google.com",
];

/// Returns `true` when Chrome allows attaching `chrome.debugger` to `url`.
pub fn is_attachable_url(url: &str) -> bool {
    let lower = url.trim().to_ascii_lowercase();
    !RESTRICTED_URL_PREFIXES
        .iter()
        .any(|prefix| lower.starts_with(prefix))
}

struct TabRuntime {
    fetch_interceptor: Arc<FetchInterceptor>,
    fetch_task: Option<JoinHandle<()>>,
    network_domain: Arc<NetworkDomain>,
    network_monitor: Arc<NetworkMonitor>,
    monitor_task: Option<JoinHandle<()>>,
}

/// High-level Playwright-style E2E session orchestrator binding `CdpTransport`, CDP domains
/// (`Page`, `Target`, `Network`, `Fetch`), the deterministic `LocatorEngine`, and `SnapshotStore`.
pub struct E2eSession {
    transport: Arc<dyn CdpTransport>,
    locator: Arc<LocatorEngine>,
    snapshot_store: SnapshotStore,
    active_tab: Mutex<Option<i64>>,
    tab_runtimes: Mutex<HashMap<i64, TabRuntime>>,
}

impl E2eSession {
    pub fn new(transport: Arc<dyn CdpTransport>, snapshots_dir: impl AsRef<Path>) -> Arc<Self> {
        let locator = Arc::new(LocatorEngine::new(transport.clone()));
        Arc::new(Self {
            transport,
            locator,
            snapshot_store: SnapshotStore::new(snapshots_dir),
            active_tab: Mutex::new(None),
            tab_runtimes: Mutex::new(HashMap::new()),
        })
    }

    pub fn transport(&self) -> &Arc<dyn CdpTransport> {
        &self.transport
    }

    pub fn snapshot_store(&self) -> &SnapshotStore {
        &self.snapshot_store
    }

    pub fn set_active_tab(&self, tab_id: i64) {
        *self.active_tab.lock().expect("active_tab mutex poisoned") = Some(tab_id);
    }

    pub async fn resolve_tab(&self, requested: Option<i64>) -> anyhow::Result<i64> {
        if let Some(id) = requested {
            self.set_active_tab(id);
            return Ok(id);
        }

        let cached = *self.active_tab.lock().expect("active_tab mutex poisoned");
        let tabs = self.transport.list_tabs().await?;

        if let Some(id) = cached
            && tabs
                .iter()
                .any(|t| t.tab_id == id && is_attachable_url(&t.url))
        {
            return Ok(id);
        }

        if let Some(tab) = tabs
            .iter()
            .find(|t| t.attached && is_attachable_url(&t.url))
        {
            let id = tab.tab_id;
            self.set_active_tab(id);
            return Ok(id);
        }

        // Do not hijack unrelated user tabs or restricted chrome:// pages; open a clean test tab.
        let created = self.transport.create_tab("about:blank", true).await?;
        self.set_active_tab(created.tab_id);
        Ok(created.tab_id)
    }

    pub async fn goto(
        &self,
        tab_id: i64,
        url: &str,
        wait_until: LoadState,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        self.transport.attach_tab(tab_id).await?;
        let page = PageDomain::new(self.transport.clone(), tab_id);
        page.enable().await?;

        let target = TargetDomain::new(self.transport.clone(), tab_id);
        let _ = target.set_auto_attach(true, false, true).await;

        let frame_id = page
            .navigate(url, wait_until, Duration::from_millis(timeout_ms))
            .await?;
        Ok(json!({ "tabId": tab_id, "frameId": frame_id, "url": url }))
    }

    pub async fn snapshot(&self, tab_id: i64, interactive_only: bool) -> anyhow::Result<Value> {
        let snap = self.locator.snapshot(tab_id, interactive_only).await?;
        Ok(json!({
            "tabId": tab_id,
            "frameId": snap.frame_id,
            "compact": snap.render_compact(),
            "items": snap.items,
        }))
    }

    pub async fn click_ref(&self, tab_id: i64, ref_id: &str) -> anyhow::Result<Value> {
        let (x, y) = self.locator.click_ref(tab_id, ref_id).await?;
        Ok(json!({ "tabId": tab_id, "refId": ref_id, "x": x, "y": y }))
    }

    pub async fn fill_ref(&self, tab_id: i64, ref_id: &str, text: &str) -> anyhow::Result<Value> {
        self.locator.fill_ref(tab_id, ref_id, text).await?;
        Ok(json!({ "tabId": tab_id, "refId": ref_id, "filled": true }))
    }

    pub async fn click_role(
        &self,
        tab_id: i64,
        role: &str,
        name: Option<&str>,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let mut matches = self.locator.get_by_role(tab_id, role, name, exact);
            if matches.len() <= nth {
                let _ = self.locator.snapshot(tab_id, false).await?;
                matches = self.locator.get_by_role(tab_id, role, name, exact);
            }
            if let Some(target) = matches.into_iter().nth(nth) {
                match self.locator.click_ref(tab_id, &target.ref_id).await {
                    Ok((x, y)) => {
                        return Ok(json!({
                            "tabId": tab_id,
                            "refId": target.ref_id,
                            "role": target.role,
                            "name": target.name,
                            "x": x,
                            "y": y
                        }));
                    }
                    Err(err) if Instant::now() < deadline => {
                        tracing::debug!(?err, "click_role retrying after transient error");
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "no element found with role={role:?} name={name:?} nth={nth} within {timeout_ms}ms"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = self.locator.snapshot(tab_id, false).await?;
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub async fn fill_role(
        &self,
        tab_id: i64,
        role: &str,
        name: Option<&str>,
        text: &str,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let mut matches = self.locator.get_by_role(tab_id, role, name, exact);
            if matches.len() <= nth {
                let _ = self.locator.snapshot(tab_id, false).await?;
                matches = self.locator.get_by_role(tab_id, role, name, exact);
            }
            if let Some(target) = matches.into_iter().nth(nth) {
                match self.locator.fill_ref(tab_id, &target.ref_id, text).await {
                    Ok(()) => {
                        return Ok(json!({
                            "tabId": tab_id,
                            "refId": target.ref_id,
                            "role": target.role,
                            "name": target.name,
                            "filled": true
                        }));
                    }
                    Err(err) if Instant::now() < deadline => {
                        tracing::debug!(?err, "fill_role retrying after transient error");
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "no element found to fill with role={role:?} name={name:?} nth={nth} within {timeout_ms}ms"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
            let _ = self.locator.snapshot(tab_id, false).await?;
        }
    }

    pub async fn dblclick_ref(&self, tab_id: i64, ref_id: &str) -> anyhow::Result<Value> {
        let (x, y) = self.locator.dblclick_ref(tab_id, ref_id).await?;
        Ok(json!({ "tabId": tab_id, "refId": ref_id, "x": x, "y": y }))
    }

    pub async fn dblclick_text(
        &self,
        tab_id: i64,
        text: &str,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        self.run_text_pointer_action(tab_id, text, exact, nth, timeout_ms, true)
            .await
    }

    pub async fn hover_ref(&self, tab_id: i64, ref_id: &str) -> anyhow::Result<Value> {
        let (x, y) = self.locator.hover_ref(tab_id, ref_id).await?;
        Ok(json!({ "tabId": tab_id, "refId": ref_id, "x": x, "y": y }))
    }

    pub async fn hover_text(
        &self,
        tab_id: i64,
        text: &str,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        self.run_text_pointer_action(tab_id, text, exact, nth, timeout_ms, false)
            .await
    }

    async fn run_text_pointer_action(
        &self,
        tab_id: i64,
        text: &str,
        exact: bool,
        nth: usize,
        timeout_ms: u64,
        is_dblclick: bool,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let _ = self.locator.snapshot(tab_id, false).await?;
            let matches = self.locator.get_by_text(tab_id, text, exact);
            if let Some(target) = matches.into_iter().nth(nth) {
                let res = if is_dblclick {
                    self.locator.dblclick_ref(tab_id, &target.ref_id).await
                } else {
                    self.locator.hover_ref(tab_id, &target.ref_id).await
                };
                match res {
                    Ok((x, y)) => {
                        return Ok(json!({
                            "tabId": tab_id,
                            "refId": target.ref_id,
                            "name": target.name,
                            "x": x,
                            "y": y
                        }));
                    }
                    Err(err) if Instant::now() < deadline => {
                        tracing::debug!(?err, "text pointer action retrying");
                    }
                    Err(err) => return Err(err.into()),
                }
            }
            if Instant::now() >= deadline {
                let verb = if is_dblclick { "dblclick" } else { "hover" };
                anyhow::bail!(
                    "no element found to {verb} with text={text:?} nth={nth} within {timeout_ms}ms"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn press_key(&self, tab_id: i64, key: &str) -> anyhow::Result<Value> {
        self.locator.press_key(tab_id, key).await?;
        Ok(json!({ "tabId": tab_id, "key": key }))
    }

    pub async fn type_text(&self, tab_id: i64, text: &str) -> anyhow::Result<Value> {
        self.locator.type_text(tab_id, text).await?;
        Ok(json!({ "tabId": tab_id, "typed": true }))
    }

    pub async fn reload(&self, tab_id: i64, ignore_cache: bool) -> anyhow::Result<Value> {
        let page = PageDomain::new(self.transport.clone(), tab_id);
        page.reload(ignore_cache).await?;
        Ok(json!({ "tabId": tab_id, "reloaded": true }))
    }

    pub async fn assert_text(
        &self,
        tab_id: i64,
        text: &str,
        exact: bool,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let _ = self.locator.snapshot(tab_id, false).await?;
            let matches = self.locator.get_by_text(tab_id, text, exact);
            if let Some(first) = matches.into_iter().next() {
                return Ok(json!({
                    "tabId": tab_id,
                    "matchedRef": first.ref_id,
                    "role": first.role,
                    "name": first.name,
                }));
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "assertion failed: text {text:?} not found in AX tree within {timeout_ms}ms"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn assert_hidden(
        &self,
        tab_id: i64,
        text: &str,
        exact: bool,
        timeout_ms: u64,
    ) -> anyhow::Result<Value> {
        let deadline = Instant::now() + Duration::from_millis(timeout_ms.max(1));
        loop {
            let _ = self.locator.snapshot(tab_id, false).await?;
            let matches = self.locator.get_by_text(tab_id, text, exact);
            if matches.is_empty() {
                return Ok(json!({
                    "tabId": tab_id,
                    "hidden": text,
                }));
            }
            if Instant::now() >= deadline {
                anyhow::bail!(
                    "assertion failed: text {text:?} is still visible after {timeout_ms}ms"
                );
            }
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    pub async fn intercept_network(
        &self,
        tab_id: i64,
        route: MockRouteResponse,
        url_patterns: Vec<String>,
    ) -> anyhow::Result<Value> {
        self.transport.attach_tab(tab_id).await?;
        let interceptor = {
            let mut guard = self.tab_runtimes.lock().expect("tab_runtimes mutex poisoned");
            let runtime = guard.entry(tab_id).or_insert_with(|| TabRuntime {
                fetch_interceptor: Arc::new(FetchInterceptor::new(self.transport.clone(), tab_id)),
                fetch_task: None,
                network_domain: Arc::new(NetworkDomain::new(self.transport.clone(), tab_id)),
                network_monitor: NetworkMonitor::new(),
                monitor_task: None,
            });
            runtime.fetch_interceptor.add_route(route);
            if runtime.fetch_task.is_none() {
                runtime.fetch_task = Some(runtime.fetch_interceptor.clone().spawn_interceptor());
            }
            runtime.fetch_interceptor.clone()
        };

        interceptor.enable(url_patterns).await?;
        Ok(json!({ "tabId": tab_id, "intercepting": true }))
    }

    pub async fn start_network_monitor(&self, tab_id: i64) -> anyhow::Result<Value> {
        let network_domain = {
            let mut guard = self.tab_runtimes.lock().expect("tab_runtimes mutex poisoned");
            let runtime = guard.entry(tab_id).or_insert_with(|| TabRuntime {
                fetch_interceptor: Arc::new(FetchInterceptor::new(self.transport.clone(), tab_id)),
                fetch_task: None,
                network_domain: Arc::new(NetworkDomain::new(self.transport.clone(), tab_id)),
                network_monitor: NetworkMonitor::new(),
                monitor_task: None,
            });
            if runtime.monitor_task.is_none() {
                runtime.monitor_task = Some(
                    runtime
                        .network_monitor
                        .spawn_listener(self.transport.clone(), tab_id),
                );
            }
            runtime.network_domain.clone()
        };
        network_domain.enable().await?;
        Ok(json!({ "tabId": tab_id, "monitoring": true }))
    }

    pub async fn assert_screenshot(
        &self,
        tab_id: i64,
        name: &str,
        update_baselines: bool,
        max_diff_ratio: Option<f64>,
        threshold: Option<f64>,
        snapshots_dir: Option<&Path>,
    ) -> anyhow::Result<VisualAssertionResult> {
        let page = PageDomain::new(self.transport.clone(), tab_id);
        let png_bytes = page.capture_screenshot(&ScreenshotOptions::default()).await?;

        let mut policy = BaselinePolicy {
            update_baselines,
            ..Default::default()
        };
        if let Some(ratio) = max_diff_ratio {
            policy.max_diff_ratio = ratio;
        }
        if let Some(t) = threshold {
            policy.diff_options = DiffOptions {
                threshold: t,
                ..policy.diff_options
            };
        }

        let custom_store;
        let store = match snapshots_dir {
            Some(dir) => {
                custom_store = SnapshotStore::new(dir);
                &custom_store
            }
            None => &self.snapshot_store,
        };

        let result = store.assert_matches(name, &png_bytes, &policy)?;
        Ok(result)
    }

    pub async fn dispatch_rpc(&self, req: &WsRequest) -> WsResponse {
        match self.dispatch_inner(&req.action, &req.params).await {
            Ok(result) => WsResponse {
                id: req.id,
                ok: true,
                result: Some(result),
                error: None,
            },
            Err(err) => WsResponse {
                id: req.id,
                ok: false,
                result: None,
                error: Some(err.to_string()),
            },
        }
    }

    async fn dispatch_inner(&self, action: &str, params: &Value) -> anyhow::Result<Value> {
        let req_tab = params.get("tabId").and_then(Value::as_i64);
        match action {
            "tab.list" => Ok(json!({ "tabs": self.transport.list_tabs().await? })),
            "tab.create" => {
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .unwrap_or("about:blank");
                let active = params
                    .get("active")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                let tab = self.transport.create_tab(url, active).await?;
                self.set_active_tab(tab.tab_id);
                Ok(serde_json::to_value(tab)?)
            }
            "tab.attach" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                self.transport.attach_tab(tab_id).await?;
                Ok(json!({ "tabId": tab_id, "attached": true }))
            }
            "goto" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let url = params
                    .get("url")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `url`"))?;
                let wait_until = params
                    .get("waitUntil")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or(LoadState::Load);
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(15_000);
                self.goto(tab_id, url, wait_until, timeout_ms).await
            }
            "snapshot" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let interactive = params
                    .get("interactiveOnly")
                    .and_then(Value::as_bool)
                    .unwrap_or(true);
                self.snapshot(tab_id, interactive).await
            }
            "click_ref" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let ref_id = params
                    .get("refId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `refId`"))?;
                self.click_ref(tab_id, ref_id).await
            }
            "fill_ref" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let ref_id = params
                    .get("refId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `refId`"))?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                self.fill_ref(tab_id, ref_id, text).await
            }
            "click_role" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let role = params
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `role`"))?;
                let name = params.get("name").and_then(Value::as_str);
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let nth = params.get("nth").and_then(Value::as_u64).unwrap_or(0) as usize;
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.click_role(tab_id, role, name, exact, nth, timeout_ms)
                    .await
            }
            "fill_role" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let role = params
                    .get("role")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `role`"))?;
                let name = params.get("name").and_then(Value::as_str);
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let nth = params.get("nth").and_then(Value::as_u64).unwrap_or(0) as usize;
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.fill_role(tab_id, role, name, text, exact, nth, timeout_ms)
                    .await
            }
            "dblclick_ref" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let ref_id = params
                    .get("refId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `refId`"))?;
                self.dblclick_ref(tab_id, ref_id).await
            }
            "dblclick_text" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let nth = params.get("nth").and_then(Value::as_u64).unwrap_or(0) as usize;
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.dblclick_text(tab_id, text, exact, nth, timeout_ms).await
            }
            "hover_ref" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let ref_id = params
                    .get("refId")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `refId`"))?;
                self.hover_ref(tab_id, ref_id).await
            }
            "hover_text" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let nth = params.get("nth").and_then(Value::as_u64).unwrap_or(0) as usize;
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.hover_text(tab_id, text, exact, nth, timeout_ms).await
            }
            "press_key" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let key = params
                    .get("key")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `key`"))?;
                self.press_key(tab_id, key).await
            }
            "type_text" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                self.type_text(tab_id, text).await
            }
            "reload" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let ignore_cache = params
                    .get("ignoreCache")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                self.reload(tab_id, ignore_cache).await
            }
            "assert_text" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.assert_text(tab_id, text, exact, timeout_ms).await
            }
            "assert_hidden" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let text = params
                    .get("text")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `text`"))?;
                let exact = params
                    .get("exact")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let timeout_ms = params
                    .get("timeoutMs")
                    .and_then(Value::as_u64)
                    .unwrap_or(5_000);
                self.assert_hidden(tab_id, text, exact, timeout_ms).await
            }
            "intercept_network" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let route: MockRouteResponse = serde_json::from_value(
                    params
                        .get("route")
                        .cloned()
                        .ok_or_else(|| anyhow::anyhow!("missing `route`"))?,
                )?;
                let patterns: Vec<String> = params
                    .get("patterns")
                    .cloned()
                    .map(serde_json::from_value)
                    .transpose()?
                    .unwrap_or_default();
                self.intercept_network(tab_id, route, patterns).await
            }
            "network.monitor_start" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                self.start_network_monitor(tab_id).await
            }
            "assert_screenshot" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let name = params
                    .get("name")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `name`"))?;
                let update = params
                    .get("updateBaselines")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);
                let max_diff_ratio = params.get("maxDiffRatio").and_then(Value::as_f64);
                let threshold = params.get("threshold").and_then(Value::as_f64);
                let snapshots_dir = params
                    .get("snapshotsDir")
                    .and_then(Value::as_str)
                    .map(Path::new);
                let res = self
                    .assert_screenshot(
                        tab_id,
                        name,
                        update,
                        max_diff_ratio,
                        threshold,
                        snapshots_dir,
                    )
                    .await?;
                if let VisualAssertionResult::Failed {
                    diff_pixels,
                    diff_ratio,
                    ref diff_path,
                    ..
                } = res
                {
                    return Err(anyhow::anyhow!(
                        "visual regression for {name:?}: {diff_pixels} px ({:.4}%), diff at {}",
                        diff_ratio * 100.0,
                        diff_path.display()
                    ));
                }
                Ok(serde_json::to_value(res)?)
            }
            "raw_cdp" => {
                let tab_id = self.resolve_tab(req_tab).await?;
                let method = params
                    .get("method")
                    .and_then(Value::as_str)
                    .ok_or_else(|| anyhow::anyhow!("missing `method`"))?;
                let cdp_params = params.get("params").cloned().unwrap_or_else(|| json!({}));
                let session_id = params.get("sessionId").and_then(Value::as_str);
                Ok(self
                    .transport
                    .send_cdp(tab_id, method, cdp_params, session_id)
                    .await?)
            }
            other => Err(anyhow::anyhow!("unknown RPC action: {other}")),
        }
    }
}
