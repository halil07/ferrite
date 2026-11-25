#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferrite_core::{CdpError, CdpEvent, CdpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TargetInfo {
    pub target_id: String,
    #[serde(rename = "type")]
    pub target_type: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub attached: bool,
    #[serde(default)]
    pub opener_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChildSession {
    pub session_id: String,
    pub target_info: TargetInfo,
    pub waiting_for_debugger: bool,
}

/// Maintains an up-to-date map of child target sessions (OOPIFs, workers) on a tab
/// from `Target.attachedToTarget` and `Target.detachedFromTarget` events.
#[derive(Default)]
pub struct TargetSessionRegistry {
    sessions: Mutex<HashMap<String, ChildSession>>,
}

impl TargetSessionRegistry {
    pub fn new() -> Arc<Self> {
        Arc::new(Self::default())
    }

    pub fn get(&self, session_id: &str) -> Option<ChildSession> {
        self.sessions
            .lock()
            .expect("target registry mutex poisoned")
            .get(session_id)
            .cloned()
    }

    pub fn list(&self) -> Vec<ChildSession> {
        self.sessions
            .lock()
            .expect("target registry mutex poisoned")
            .values()
            .cloned()
            .collect()
    }

    /// Processes a single CDP event and optionally returns the newly attached child session
    /// when `waitingForDebugger` requires resuming via `Runtime.runIfWaitingForDebugger`.
    pub fn ingest_event(&self, event: &CdpEvent) -> Option<ChildSession> {
        match event.method.as_str() {
            "Target.attachedToTarget" => {
                let session_id = event
                    .params
                    .get("sessionId")
                    .and_then(Value::as_str)?
                    .to_string();
                let target_info: TargetInfo =
                    serde_json::from_value(event.params.get("targetInfo")?.clone()).ok()?;
                let waiting_for_debugger = event
                    .params
                    .get("waitingForDebugger")
                    .and_then(Value::as_bool)
                    .unwrap_or(false);

                let child = ChildSession {
                    session_id: session_id.clone(),
                    target_info,
                    waiting_for_debugger,
                };
                self.sessions
                    .lock()
                    .expect("target registry mutex poisoned")
                    .insert(session_id, child.clone());
                Some(child)
            }
            "Target.detachedFromTarget" => {
                if let Some(session_id) = event.params.get("sessionId").and_then(Value::as_str) {
                    self.sessions
                        .lock()
                        .expect("target registry mutex poisoned")
                        .remove(session_id);
                }
                None
            }
            _ => None,
        }
    }
}

/// Typed manager for the CDP `Target` domain (auto-attach for OOPIFs/workers and target lifecycle).
pub struct TargetDomain {
    transport: Arc<dyn CdpTransport>,
    tab_id: i64,
    registry: Arc<TargetSessionRegistry>,
}

impl TargetDomain {
    pub fn new(transport: Arc<dyn CdpTransport>, tab_id: i64) -> Self {
        Self {
            transport,
            tab_id,
            registry: TargetSessionRegistry::new(),
        }
    }

    pub fn registry(&self) -> Arc<TargetSessionRegistry> {
        self.registry.clone()
    }

    pub async fn set_auto_attach(
        &self,
        auto_attach: bool,
        wait_for_debugger_on_start: bool,
        flatten: bool,
    ) -> Result<(), CdpError> {
        self.transport
            .send_cdp(
                self.tab_id,
                "Target.setAutoAttach",
                json!({
                    "autoAttach": auto_attach,
                    "waitForDebuggerOnStart": wait_for_debugger_on_start,
                    "flatten": flatten,
                }),
                None,
            )
            .await?;
        Ok(())
    }

    pub async fn get_targets(&self) -> Result<Vec<TargetInfo>, CdpError> {
        let res = self
            .transport
            .send_cdp(self.tab_id, "Target.getTargets", json!({}), None)
            .await?;
        let arr = res.get("targetInfos").cloned().unwrap_or_else(|| json!([]));
        serde_json::from_value(arr).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("invalid Target.getTargets payload: {err}"),
        })
    }

    pub async fn create_target(&self, url: &str) -> Result<String, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Target.createTarget",
                json!({ "url": url }),
                None,
            )
            .await?;
        res.get("targetId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Target.createTarget omitted targetId".to_string(),
            })
    }

    pub async fn close_target(&self, target_id: &str) -> Result<bool, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Target.closeTarget",
                json!({ "targetId": target_id }),
                None,
            )
            .await?;
        Ok(res.get("success").and_then(Value::as_bool).unwrap_or(true))
    }

    pub async fn attach_to_target(&self, target_id: &str, flatten: bool) -> Result<String, CdpError> {
        let res = self
            .transport
            .send_cdp(
                self.tab_id,
                "Target.attachToTarget",
                json!({ "targetId": target_id, "flatten": flatten }),
                None,
            )
            .await?;
        res.get("sessionId")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "Target.attachToTarget omitted sessionId".to_string(),
            })
    }

    /// Spawns a background watcher that updates `TargetSessionRegistry` and resumes paused
    /// child targets after enabling `Page` on their flattened child session.
    pub fn spawn_child_watcher(&self) -> JoinHandle<()> {
        let mut rx = self.transport.subscribe_events();
        let transport = self.transport.clone();
        let tab_id = self.tab_id;
        let registry = self.registry.clone();

        tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.tab_id == tab_id => {
                        if let Some(child) = registry.ingest_event(&ev)
                            && child.waiting_for_debugger
                        {
                            let _ = transport
                                .send_cdp(tab_id, "Page.enable", json!({}), Some(&child.session_id))
                                .await;
                            let _ = transport
                                .send_cdp(
                                    tab_id,
                                    "Runtime.runIfWaitingForDebugger",
                                    json!({}),
                                    Some(&child.session_id),
                                )
                                .await;
                        }
                    }
                    Ok(_) => {}
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
    async fn tracks_and_resumes_auto_attached_child_sessions() {
        let mock = MockCdpTransport::new();
        let target = TargetDomain::new(mock.clone(), 1);
        let handle = target.spawn_child_watcher();

        // Allow watcher task to subscribe before emitting events.
        tokio::task::yield_now().await;

        mock.emit_event(CdpEvent {
            tab_id: 1,
            method: "Target.attachedToTarget".to_string(),
            params: json!({
                "sessionId": "child-sess-1",
                "waitingForDebugger": true,
                "targetInfo": {
                    "targetId": "iframe-1",
                    "type": "iframe",
                    "title": "Payment Frame",
                    "url": "https://pay.example/form",
                    "attached": true
                }
            }),
            session_id: None,
        });

        tokio::time::sleep(std::time::Duration::from_millis(30)).await;
        let sessions = target.registry().list();
        assert_eq!(sessions.len(), 1);
        assert_eq!(sessions[0].session_id, "child-sess-1");

        let calls = mock.recorded_calls();
        assert!(calls.iter().any(|c| {
            c.method == "Runtime.runIfWaitingForDebugger"
                && c.session_id.as_deref() == Some("child-sess-1")
        }));

        mock.emit_event(CdpEvent {
            tab_id: 1,
            method: "Target.detachedFromTarget".to_string(),
            params: json!({ "sessionId": "child-sess-1" }),
            session_id: None,
        });
        tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        assert!(target.registry().list().is_empty());

        handle.abort();
    }
}
