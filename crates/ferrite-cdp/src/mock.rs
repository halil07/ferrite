#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use ferrite_core::{CdpError, CdpEvent, CdpTransport, TabInfo};
use serde_json::{Value, json};
use tokio::sync::broadcast;

type DynamicHandler =
    Arc<dyn Fn(i64, &str, &Value, Option<&str>) -> Option<Result<Value, CdpError>> + Send + Sync>;

#[derive(Debug, Clone, PartialEq)]
pub struct RecordedCdpCall {
    pub tab_id: i64,
    pub method: String,
    pub params: Value,
    pub session_id: Option<String>,
}

#[derive(Default)]
struct MockState {
    tabs: Vec<TabInfo>,
    attached: HashSet<i64>,
    next_tab_id: i64,
    recorded: Vec<RecordedCdpCall>,
    scripted: HashMap<String, VecDeque<Result<Value, CdpError>>>,
    handlers: Vec<DynamicHandler>,
}

/// Deterministic in-memory [`CdpTransport`] for unit and integration tests.
pub struct MockCdpTransport {
    state: Mutex<MockState>,
    events_tx: broadcast::Sender<CdpEvent>,
}

impl Default for MockCdpTransport {
    fn default() -> Self {
        let (events_tx, _) = broadcast::channel(512);
        Self {
            state: Mutex::new(MockState {
                tabs: vec![TabInfo {
                    tab_id: 1,
                    window_id: 1,
                    url: "about:blank".to_string(),
                    title: "New Tab".to_string(),
                    active: true,
                    attached: true,
                }],
                attached: HashSet::from([1]),
                next_tab_id: 2,
                recorded: Vec::new(),
                scripted: HashMap::new(),
                handlers: Vec::new(),
            }),
            events_tx,
        }
    }
}

impl MockCdpTransport {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn push_response(&self, method: &str, response: Result<Value, CdpError>) {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard
            .scripted
            .entry(method.to_string())
            .or_default()
            .push_back(response);
    }

    pub fn push_ok(&self, method: &str, value: Value) {
        self.push_response(method, Ok(value));
    }

    pub fn push_err(&self, method: &str, err: CdpError) {
        self.push_response(method, Err(err));
    }

    pub fn add_handler<F>(&self, handler: F)
    where
        F: Fn(i64, &str, &Value, Option<&str>) -> Option<Result<Value, CdpError>>
            + Send
            + Sync
            + 'static,
    {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard.handlers.push(Arc::new(handler));
    }

    pub fn emit_event(&self, event: CdpEvent) {
        let _ = self.events_tx.send(event);
    }

    pub fn recorded_calls(&self) -> Vec<RecordedCdpCall> {
        self.state
            .lock()
            .expect("mock mutex poisoned")
            .recorded
            .clone()
    }

    pub fn clear_recorded(&self) {
        self.state
            .lock()
            .expect("mock mutex poisoned")
            .recorded
            .clear();
    }

    pub fn set_tabs(&self, tabs: Vec<TabInfo>) {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard.attached = tabs
            .iter()
            .filter(|t| t.attached)
            .map(|t| t.tab_id)
            .collect();
        guard.next_tab_id = tabs.iter().map(|t| t.tab_id).max().unwrap_or(0) + 1;
        guard.tabs = tabs;
    }

    pub fn is_attached(&self, tab_id: i64) -> bool {
        self.state
            .lock()
            .expect("mock mutex poisoned")
            .attached
            .contains(&tab_id)
    }
}

#[async_trait]
impl CdpTransport for MockCdpTransport {
    async fn send_cdp(
        &self,
        tab_id: i64,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let (queued, handlers) = {
            let mut guard = self.state.lock().expect("mock mutex poisoned");
            guard.recorded.push(RecordedCdpCall {
                tab_id,
                method: method.to_string(),
                params: params.clone(),
                session_id: session_id.map(str::to_string),
            });
            let queued = guard
                .scripted
                .get_mut(method)
                .and_then(|queue| queue.pop_front());
            let handlers = guard.handlers.clone();
            (queued, handlers)
        };

        if let Some(res) = queued {
            return res;
        }

        for handler in handlers {
            if let Some(res) = handler(tab_id, method, &params, session_id) {
                return res;
            }
        }

        Ok(json!({}))
    }

    async fn attach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard.attached.insert(tab_id);
        if let Some(tab) = guard.tabs.iter_mut().find(|t| t.tab_id == tab_id) {
            tab.attached = true;
        }
        Ok(())
    }

    async fn detach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard.attached.remove(&tab_id);
        if let Some(tab) = guard.tabs.iter_mut().find(|t| t.tab_id == tab_id) {
            tab.attached = false;
        }
        Ok(())
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>, CdpError> {
        Ok(self
            .state
            .lock()
            .expect("mock mutex poisoned")
            .tabs
            .clone())
    }

    async fn create_tab(&self, url: &str, active: bool) -> Result<TabInfo, CdpError> {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        let id = guard.next_tab_id;
        guard.next_tab_id += 1;
        let tab = TabInfo {
            tab_id: id,
            window_id: 1,
            url: url.to_string(),
            title: String::new(),
            active,
            attached: true,
        };
        guard.attached.insert(id);
        guard.tabs.push(tab.clone());
        Ok(tab)
    }

    async fn close_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        let mut guard = self.state.lock().expect("mock mutex poisoned");
        guard.tabs.retain(|t| t.tab_id != tab_id);
        guard.attached.remove(&tab_id);
        Ok(())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
    }
}
