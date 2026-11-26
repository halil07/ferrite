#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrite_core::{CdpError, CdpEvent, CdpTransport, TabInfo, classify_cdp_error};
use futures_util::{SinkExt, StreamExt};
use serde_json::{Value, json};
use tokio::io::{AsyncBufReadExt, BufReader};
use tokio::process::{Child, Command};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tokio_tungstenite::connect_async;
use tokio_tungstenite::tungstenite::Message;

const DEFAULT_CDP_TIMEOUT: Duration = Duration::from_secs(60);

/// Configuration for launching a headless Chromium-family browser managed by Ferrite.
#[derive(Debug, Clone)]
pub struct HeadlessOptions {
    /// Optional explicit path to a Chrome, Chromium, Edge, or Brave executable.
    pub chrome_path: Option<PathBuf>,
    /// Initial browser viewport / window size `(width, height)`.
    pub window_size: (u32, u32),
    /// Additional command-line flags appended to the browser invocation.
    pub extra_args: Vec<String>,
    /// Maximum time to wait for Chrome to print its DevTools WebSocket endpoint.
    pub startup_timeout: Duration,
}

impl Default for HeadlessOptions {
    fn default() -> Self {
        Self {
            chrome_path: None,
            window_size: (1280, 800),
            extra_args: Vec::new(),
            startup_timeout: Duration::from_secs(15),
        }
    }
}

#[derive(Debug, Clone)]
struct TabEntry {
    tab_id: i64,
    target_id: String,
    session_id: Option<String>,
    url: String,
    title: String,
    active: bool,
}

#[derive(Default)]
struct TabRegistry {
    next_tab_id: i64,
    tabs: HashMap<i64, TabEntry>,
    target_to_tab: HashMap<String, i64>,
    /// Maps both top-level page `sessionId` and child OOPIF `sessionId` -> `tab_id`.
    session_to_tab: HashMap<String, i64>,
}

impl TabRegistry {
    fn new() -> Self {
        Self {
            next_tab_id: 1,
            tabs: HashMap::new(),
            target_to_tab: HashMap::new(),
            session_to_tab: HashMap::new(),
        }
    }

    fn register_target(&mut self, target_id: String, url: String, title: String, active: bool) -> i64 {
        if let Some(&existing) = self.target_to_tab.get(&target_id) {
            if let Some(entry) = self.tabs.get_mut(&existing) {
                entry.url = url;
                entry.title = title;
                if active {
                    entry.active = true;
                }
            }
            return existing;
        }

        if active {
            for entry in self.tabs.values_mut() {
                entry.active = false;
            }
        }

        let tab_id = self.next_tab_id;
        self.next_tab_id += 1;
        self.target_to_tab.insert(target_id.clone(), tab_id);
        self.tabs.insert(
            tab_id,
            TabEntry {
                tab_id,
                target_id,
                session_id: None,
                url,
                title,
                active,
            },
        );
        tab_id
    }

    fn set_session(&mut self, tab_id: i64, session_id: String) {
        if let Some(entry) = self.tabs.get_mut(&tab_id) {
            if let Some(old) = entry.session_id.take() {
                self.session_to_tab.remove(&old);
            }
            self.session_to_tab.insert(session_id.clone(), tab_id);
            entry.session_id = Some(session_id);
        }
    }

    fn clear_session(&mut self, tab_id: i64) -> Option<String> {
        let entry = self.tabs.get_mut(&tab_id)?;
        let old = entry.session_id.take()?;
        self.session_to_tab.remove(&old);
        Some(old)
    }

    fn register_child_session(&mut self, child_session_id: String, tab_id: i64) {
        self.session_to_tab.insert(child_session_id, tab_id);
    }

    fn remove_tab(&mut self, tab_id: i64) -> Option<TabEntry> {
        let entry = self.tabs.remove(&tab_id)?;
        self.target_to_tab.remove(&entry.target_id);
        self.session_to_tab.retain(|_, v| *v != tab_id);
        Some(entry)
    }
}

/// Direct Chrome DevTools Protocol (CDP) transport over WebSocket backed by a managed
/// headless Chrome/Chromium/Edge process. Requires no Chrome extension or visible window.
pub struct HeadlessCdpTransport {
    ws_tx: mpsc::UnboundedSender<String>,
    pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>>,
    next_id: AtomicU64,
    registry: Arc<Mutex<TabRegistry>>,
    events_tx: broadcast::Sender<CdpEvent>,
    child: Mutex<Option<Child>>,
    user_data_dir: PathBuf,
    tasks: Mutex<Vec<JoinHandle<()>>>,
}

impl HeadlessCdpTransport {
    /// Launches a headless browser process, connects to its browser-level CDP WebSocket,
    /// and attaches to the initial page target as `tab_id = 1`.
    pub async fn launch(options: HeadlessOptions) -> anyhow::Result<Arc<Self>> {
        let binary = find_chrome_binary(options.chrome_path.as_deref())?;
        let user_data_dir = create_temp_profile_dir()?;

        let mut cmd = Command::new(&binary);
        cmd.arg("--headless=new")
            .arg("--remote-debugging-port=0")
            .arg(format!("--user-data-dir={}", user_data_dir.display()))
            .arg("--no-first-run")
            .arg("--no-default-browser-check")
            .arg("--disable-background-networking")
            .arg("--disable-sync")
            .arg("--disable-extensions")
            .arg("--disable-gpu")
            .arg(format!(
                "--window-size={},{}",
                options.window_size.0, options.window_size.1
            ));

        for arg in &options.extra_args {
            cmd.arg(arg);
        }

        cmd.arg("about:blank")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true);

        let mut child = cmd.spawn().map_err(|err| {
            let _ = fs::remove_dir_all(&user_data_dir);
            anyhow::anyhow!(
                "failed to launch headless browser at {}: {err}",
                binary.display()
            )
        })?;

        let stderr = child
            .stderr
            .take()
            .ok_or_else(|| anyhow::anyhow!("headless browser stderr pipe was not captured"))?;

        let (ws_url, stderr_task) =
            match wait_for_devtools_ws_url(stderr, options.startup_timeout).await {
                Ok(pair) => pair,
                Err(err) => {
                    let _ = child.start_kill();
                    let _ = fs::remove_dir_all(&user_data_dir);
                    return Err(err);
                }
            };

        tracing::info!(
            binary = %binary.display(),
            ws_url = %ws_url,
            "connected to headless browser DevTools endpoint"
        );

        let (ws_stream, _) = connect_async(&ws_url).await.map_err(|err| {
            stderr_task.abort();
            let _ = child.start_kill();
            let _ = fs::remove_dir_all(&user_data_dir);
            anyhow::anyhow!("failed to connect to DevTools WebSocket {ws_url}: {err}")
        })?;

        let (mut ws_write, mut ws_read) = ws_stream.split();
        let (ws_tx, mut ws_rx) = mpsc::unbounded_channel::<String>();
        let (events_tx, _) = broadcast::channel(1024);
        let pending: Arc<Mutex<HashMap<u64, oneshot::Sender<Result<Value, CdpError>>>>> =
            Arc::new(Mutex::new(HashMap::new()));
        let registry = Arc::new(Mutex::new(TabRegistry::new()));

        let writer_task = tokio::spawn(async move {
            while let Some(payload) = ws_rx.recv().await {
                if ws_write.send(Message::Text(payload.into())).await.is_err() {
                    break;
                }
            }
        });

        let reader_pending = Arc::clone(&pending);
        let reader_registry = Arc::clone(&registry);
        let reader_events = events_tx.clone();
        let reader_task = tokio::spawn(async move {
            while let Some(frame) = ws_read.next().await {
                let text = match frame {
                    Ok(Message::Text(t)) => t.to_string(),
                    Ok(Message::Close(_)) => break,
                    Ok(_) => continue,
                    Err(err) => {
                        tracing::debug!(error = %err, "headless CDP WebSocket read ended");
                        break;
                    }
                };

                let Ok(msg) = serde_json::from_str::<Value>(&text) else {
                    continue;
                };

                if let Some(id) = msg.get("id").and_then(Value::as_u64) {
                    let tx = reader_pending
                        .lock()
                        .expect("pending mutex poisoned")
                        .remove(&id);
                    if let Some(tx) = tx {
                        let outcome = if let Some(err_obj) = msg.get("error") {
                            let code = err_obj.get("code").and_then(Value::as_i64);
                            let message = err_obj
                                .get("message")
                                .and_then(Value::as_str)
                                .unwrap_or("CDP error");
                            Err(classify_cdp_error(message, code))
                        } else {
                            Ok(msg.get("result").cloned().unwrap_or_else(|| json!({})))
                        };
                        let _ = tx.send(outcome);
                    }
                    continue;
                }

                let Some(method) = msg.get("method").and_then(Value::as_str) else {
                    continue;
                };
                let params = msg.get("params").cloned().unwrap_or_else(|| json!({}));
                let msg_session = msg.get("sessionId").and_then(Value::as_str);

                let routed = {
                    let mut reg = reader_registry.lock().expect("registry mutex poisoned");
                    let tab_id = msg_session.and_then(|sid| reg.session_to_tab.get(sid).copied());

                    if method == "Target.attachedToTarget"
                        && let Some(parent_tab_id) = tab_id
                        && let Some(child_sid) = params.get("sessionId").and_then(Value::as_str)
                    {
                        reg.register_child_session(child_sid.to_string(), parent_tab_id);
                    }

                    tab_id.map(|tid| {
                        let is_top_level = reg
                            .tabs
                            .get(&tid)
                            .and_then(|t| t.session_id.as_deref())
                            == msg_session;
                        let forwarded_session = if is_top_level {
                            None
                        } else {
                            msg_session.map(ToOwned::to_owned)
                        };
                        (tid, forwarded_session)
                    })
                };

                if let Some((tab_id, session_id)) = routed {
                    let _ = reader_events.send(CdpEvent {
                        tab_id,
                        method: method.to_string(),
                        params,
                        session_id,
                    });
                }
            }

            // Fail any remaining in-flight CDP requests when the socket closes.
            let mut map = reader_pending.lock().expect("pending mutex poisoned");
            for (_, tx) in map.drain() {
                let _ = tx.send(Err(CdpError::Disconnected));
            }
        });

        let transport = Arc::new(Self {
            ws_tx,
            pending,
            next_id: AtomicU64::new(1),
            registry,
            events_tx,
            child: Mutex::new(Some(child)),
            user_data_dir,
            tasks: Mutex::new(vec![stderr_task, writer_task, reader_task]),
        });

        // Discover or create the initial page target and attach to it as tab_id = 1.
        let tabs = transport.list_tabs().await?;
        let initial_tab_id = if let Some(first) = tabs.first() {
            first.tab_id
        } else {
            transport.create_tab("about:blank", true).await?.tab_id
        };
        transport.attach_tab(initial_tab_id).await?;

        Ok(transport)
    }

    async fn send_raw_cdp(
        &self,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = oneshot::channel();
        self.pending
            .lock()
            .expect("pending mutex poisoned")
            .insert(id, tx);

        let mut payload = json!({
            "id": id,
            "method": method,
            "params": params,
        });
        if let Some(sid) = session_id {
            payload["sessionId"] = Value::String(sid.to_string());
        }

        if self.ws_tx.send(payload.to_string()).is_err() {
            self.pending
                .lock()
                .expect("pending mutex poisoned")
                .remove(&id);
            return Err(CdpError::Disconnected);
        }

        match tokio::time::timeout(DEFAULT_CDP_TIMEOUT, rx).await {
            Ok(Ok(res)) => res,
            Ok(Err(_)) => Err(CdpError::Disconnected),
            Err(_) => {
                self.pending
                    .lock()
                    .expect("pending mutex poisoned")
                    .remove(&id);
                Err(CdpError::Timeout(DEFAULT_CDP_TIMEOUT.as_millis() as u64))
            }
        }
    }

    /// Gracefully terminates the managed headless browser process and removes its temp profile.
    pub async fn shutdown(&self) {
        {
            let mut tasks = self.tasks.lock().expect("tasks mutex poisoned");
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }

        let child = self.child.lock().expect("child mutex poisoned").take();
        if let Some(mut proc) = child {
            let _ = proc.kill().await;
            let _ = tokio::time::timeout(Duration::from_secs(2), proc.wait()).await;
        }

        let _ = fs::remove_dir_all(&self.user_data_dir);
    }
}

impl Drop for HeadlessCdpTransport {
    fn drop(&mut self) {
        if let Ok(mut tasks) = self.tasks.lock() {
            for handle in tasks.drain(..) {
                handle.abort();
            }
        }
        if let Ok(mut guard) = self.child.lock()
            && let Some(mut proc) = guard.take()
        {
            let _ = proc.start_kill();
        }
        let _ = fs::remove_dir_all(&self.user_data_dir);
    }
}

#[async_trait]
impl CdpTransport for HeadlessCdpTransport {
    async fn send_cdp(
        &self,
        tab_id: i64,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let effective_session = if let Some(child_sid) = session_id {
            child_sid.to_string()
        } else {
            let reg = self.registry.lock().expect("registry mutex poisoned");
            reg.tabs
                .get(&tab_id)
                .and_then(|t| t.session_id.clone())
                .ok_or_else(|| CdpError::Detached(format!("tab {tab_id}")))?
        };

        self.send_raw_cdp(method, params, Some(&effective_session))
            .await
    }

    async fn attach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let (target_id, already_attached) = {
            let reg = self.registry.lock().expect("registry mutex poisoned");
            let entry = reg
                .tabs
                .get(&tab_id)
                .ok_or_else(|| CdpError::Detached(format!("unknown tab {tab_id}")))?;
            (entry.target_id.clone(), entry.session_id.is_some())
        };

        if already_attached {
            return Ok(());
        }

        let res = self
            .send_raw_cdp(
                "Target.attachToTarget",
                json!({
                    "targetId": target_id,
                    "flatten": true,
                }),
                None,
            )
            .await?;

        let session_id = res
            .get("sessionId")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Target.attachToTarget did not return sessionId".to_string(),
            })?
            .to_string();

        self.registry
            .lock()
            .expect("registry mutex poisoned")
            .set_session(tab_id, session_id);
        Ok(())
    }

    async fn detach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let session_id = self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .clear_session(tab_id);
        if let Some(sid) = session_id {
            let _ = self
                .send_raw_cdp("Target.detachFromTarget", json!({ "sessionId": sid }), None)
                .await;
        }
        Ok(())
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>, CdpError> {
        let res = self.send_raw_cdp("Target.getTargets", json!({}), None).await?;
        let target_infos = res
            .get("targetInfos")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();

        let mut reg = self.registry.lock().expect("registry mutex poisoned");
        let mut seen_ids = Vec::new();
        for info in target_infos {
            if info.get("type").and_then(Value::as_str) != Some("page") {
                continue;
            }
            let Some(target_id) = info.get("targetId").and_then(Value::as_str) else {
                continue;
            };
            let url = info
                .get("url")
                .and_then(Value::as_str)
                .unwrap_or("about:blank")
                .to_string();
            let title = info
                .get("title")
                .and_then(Value::as_str)
                .unwrap_or("")
                .to_string();
            let make_active = reg.tabs.is_empty();
            let tab_id = reg.register_target(target_id.to_string(), url, title, make_active);
            seen_ids.push(tab_id);
        }

        let mut result: Vec<TabInfo> = seen_ids
            .into_iter()
            .filter_map(|tid| reg.tabs.get(&tid))
            .map(|entry| TabInfo {
                tab_id: entry.tab_id,
                window_id: 1,
                url: entry.url.clone(),
                title: entry.title.clone(),
                active: entry.active,
                attached: entry.session_id.is_some(),
            })
            .collect();
        result.sort_by_key(|t| t.tab_id);
        Ok(result)
    }

    async fn create_tab(&self, url: &str, active: bool) -> Result<TabInfo, CdpError> {
        let res = self
            .send_raw_cdp("Target.createTarget", json!({ "url": url }), None)
            .await?;
        let target_id = res
            .get("targetId")
            .and_then(Value::as_str)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Target.createTarget did not return targetId".to_string(),
            })?
            .to_string();

        let tab_id = self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .register_target(target_id, url.to_string(), String::new(), active);

        self.attach_tab(tab_id).await?;

        Ok(TabInfo {
            tab_id,
            window_id: 1,
            url: url.to_string(),
            title: String::new(),
            active,
            attached: true,
        })
    }

    async fn close_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let entry = self
            .registry
            .lock()
            .expect("registry mutex poisoned")
            .remove_tab(tab_id);
        if let Some(entry) = entry {
            let _ = self
                .send_raw_cdp(
                    "Target.closeTarget",
                    json!({ "targetId": entry.target_id }),
                    None,
                )
                .await;
        }
        Ok(())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
    }
}

fn create_temp_profile_dir() -> anyhow::Result<PathBuf> {
    let mut rand_bytes = [0u8; 8];
    getrandom::fill(&mut rand_bytes)
        .map_err(|err| anyhow::anyhow!("failed to generate temp profile suffix: {err}"))?;
    let hex: String = rand_bytes.iter().map(|b| format!("{b:02x}")).collect();
    let dir = std::env::temp_dir().join(format!("ferrite-headless-{}-{hex}", std::process::id()));
    fs::create_dir_all(&dir)?;
    Ok(dir)
}

pub(crate) fn parse_devtools_ws_line(line: &str) -> Option<String> {
    let marker = "DevTools listening on ";
    let idx = line.find(marker)?;
    let url = line[idx + marker.len()..].trim();
    if url.starts_with("ws://") || url.starts_with("wss://") {
        Some(url.to_string())
    } else {
        None
    }
}

async fn wait_for_devtools_ws_url(
    stderr: tokio::process::ChildStderr,
    timeout: Duration,
) -> anyhow::Result<(String, JoinHandle<()>)> {
    let mut reader = BufReader::new(stderr);
    let mut line = String::new();
    let mut recent_lines = Vec::new();

    let ws_url = tokio::time::timeout(timeout, async {
        loop {
            line.clear();
            let read = reader.read_line(&mut line).await?;
            if read == 0 {
                anyhow::bail!(
                    "headless browser exited before announcing DevTools WebSocket URL. Stderr:\n{}",
                    recent_lines.join("\n")
                );
            }
            let trimmed = line.trim();
            if let Some(url) = parse_devtools_ws_line(trimmed) {
                return Ok::<String, anyhow::Error>(url);
            }
            if !trimmed.is_empty() {
                if recent_lines.len() >= 12 {
                    recent_lines.remove(0);
                }
                recent_lines.push(trimmed.to_string());
            }
        }
    })
    .await
    .map_err(|_| {
        anyhow::anyhow!(
            "timed out after {}s waiting for headless browser DevTools endpoint",
            timeout.as_secs()
        )
    })??;

    // Keep draining stderr in the background so Chrome never blocks on a full pipe.
    let drain_task = tokio::spawn(async move {
        let mut sink = String::new();
        loop {
            sink.clear();
            match reader.read_line(&mut sink).await {
                Ok(0) | Err(_) => break,
                Ok(_) => {}
            }
        }
    });

    Ok((ws_url, drain_task))
}

/// Discovers a locally installed Chrome, Chromium, Edge, or Brave executable.
pub fn find_chrome_binary(explicit: Option<&Path>) -> anyhow::Result<PathBuf> {
    if let Some(path) = explicit {
        if path.is_file() {
            return Ok(path.to_path_buf());
        }
        anyhow::bail!(
            "configured --chrome-path does not exist or is not a file: {}",
            path.display()
        );
    }

    for env_key in ["FERRITE_CHROME_PATH", "CHROME_PATH"] {
        if let Some(val) = std::env::var_os(env_key) {
            let p = PathBuf::from(val);
            if p.is_file() {
                return Ok(p);
            }
        }
    }

    for candidate in platform_chrome_candidates() {
        if candidate.is_file() {
            return Ok(candidate);
        }
    }

    if let Some(found) = find_in_path(platform_chrome_names()) {
        return Ok(found);
    }

    anyhow::bail!(
        "could not find a Chrome/Chromium/Edge executable for --headless mode. \
         Install Google Chrome or pass `--chrome-path <PATH>` (or set `FERRITE_CHROME_PATH`)."
    )
}

fn platform_chrome_candidates() -> Vec<PathBuf> {
    #[cfg(windows)]
    {
        let mut list = vec![
            PathBuf::from(r"C:\Program Files\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Google\Chrome\Application\chrome.exe"),
            PathBuf::from(r"C:\Program Files (x86)\Microsoft\Edge\Application\msedge.exe"),
            PathBuf::from(r"C:\Program Files\Microsoft\Edge\Application\msedge.exe"),
            PathBuf::from(r"C:\Program Files\BraveSoftware\Brave-Browser\Application\brave.exe"),
        ];
        if let Some(local) = std::env::var_os("LOCALAPPDATA") {
            let base = PathBuf::from(local);
            list.push(base.join(r"Google\Chrome\Application\chrome.exe"));
            list.push(base.join(r"Chromium\Application\chrome.exe"));
            list.push(base.join(r"BraveSoftware\Brave-Browser\Application\brave.exe"));
        }
        list
    }

    #[cfg(target_os = "macos")]
    {
        vec![
            PathBuf::from("/Applications/Google Chrome.app/Contents/MacOS/Google Chrome"),
            PathBuf::from("/Applications/Chromium.app/Contents/MacOS/Chromium"),
            PathBuf::from("/Applications/Microsoft Edge.app/Contents/MacOS/Microsoft Edge"),
            PathBuf::from("/Applications/Brave Browser.app/Contents/MacOS/Brave Browser"),
        ]
    }

    #[cfg(all(not(windows), not(target_os = "macos")))]
    {
        vec![
            PathBuf::from("/usr/bin/google-chrome"),
            PathBuf::from("/usr/bin/google-chrome-stable"),
            PathBuf::from("/usr/bin/chromium"),
            PathBuf::from("/usr/bin/chromium-browser"),
            PathBuf::from("/usr/bin/microsoft-edge"),
            PathBuf::from("/snap/bin/chromium"),
        ]
    }
}

fn platform_chrome_names() -> &'static [&'static str] {
    #[cfg(windows)]
    {
        &["chrome.exe", "msedge.exe", "brave.exe", "chromium.exe"]
    }
    #[cfg(not(windows))]
    {
        &[
            "google-chrome",
            "google-chrome-stable",
            "chromium",
            "chromium-browser",
            "microsoft-edge",
            "brave-browser",
        ]
    }
}

fn find_in_path(names: &[&str]) -> Option<PathBuf> {
    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        for name in names {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Some(candidate);
            }
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_devtools_ws_line() {
        let line =
            "DevTools listening on ws://127.0.0.1:54321/devtools/browser/1234-abcd\r\n";
        assert_eq!(
            parse_devtools_ws_line(line),
            Some("ws://127.0.0.1:54321/devtools/browser/1234-abcd".to_string())
        );
        assert_eq!(parse_devtools_ws_line("some unrelated stderr log"), None);
    }

    #[test]
    fn tab_registry_maps_top_level_and_child_sessions() {
        let mut reg = TabRegistry::new();
        let t1 = reg.register_target(
            "target-1".into(),
            "about:blank".into(),
            "Blank".into(),
            true,
        );
        assert_eq!(t1, 1);
        reg.set_session(t1, "sess-main".into());
        reg.register_child_session("sess-iframe".into(), t1);
        assert_eq!(reg.session_to_tab.get("sess-main"), Some(&1));
        assert_eq!(reg.session_to_tab.get("sess-iframe"), Some(&1));

        reg.remove_tab(t1);
        assert!(reg.session_to_tab.is_empty());
        assert!(reg.tabs.is_empty());
    }
}
