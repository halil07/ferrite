#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use ferrite_core::{
    CdpError, CdpEvent, CdpEventPayload, CdpSendParams, CdpTransport, DebuggerDetachedPayload,
    HelloPayload, TabInfo, WireMessage, classify_cdp_error,
};
use serde_json::{Value, json};
use tokio::sync::{broadcast, mpsc, oneshot, watch};

use crate::chunker::{ChunkReassembler, encode_wire_message};

const DEFAULT_REQUEST_TIMEOUT: Duration = Duration::from_secs(30);
const EVENT_BROADCAST_CAPACITY: usize = 1024;

struct PendingCall {
    tab_id: Option<i64>,
    tx: oneshot::Sender<Result<Value, CdpError>>,
}

#[derive(Default)]
struct RouterState {
    pending: HashMap<u64, PendingCall>,
    pending_by_tab: HashMap<i64, HashSet<u64>>,
    hello: Option<HelloPayload>,
    disconnected: bool,
}

/// Multiplexed async CDP transport backed by the Chrome Native Messaging pipe.
///
/// Correlates concurrent JSON requests via monotonic `u64` IDs, splits >700 KiB messages into
/// Chrome-safe chunks, broadcasts incoming `cdp.event` notifications, and immediately fails
/// pending commands on a tab when `debugger.detached` arrives.
pub struct NativeBridgeClient {
    next_id: AtomicU64,
    outbound_tx: mpsc::Sender<Vec<u8>>,
    events_tx: broadcast::Sender<CdpEvent>,
    disconnected_rx: watch::Receiver<bool>,
    state: Arc<Mutex<RouterState>>,
    timeout: Duration,
}

impl NativeBridgeClient {
    pub fn connect(
        outbound_tx: mpsc::Sender<Vec<u8>>,
        inbound_rx: mpsc::Receiver<Vec<u8>>,
    ) -> Arc<Self> {
        Self::connect_with_timeout(outbound_tx, inbound_rx, DEFAULT_REQUEST_TIMEOUT)
    }

    pub fn connect_with_timeout(
        outbound_tx: mpsc::Sender<Vec<u8>>,
        mut inbound_rx: mpsc::Receiver<Vec<u8>>,
        timeout: Duration,
    ) -> Arc<Self> {
        let (events_tx, _) = broadcast::channel(EVENT_BROADCAST_CAPACITY);
        let (disconnected_tx, disconnected_rx) = watch::channel(false);
        let state = Arc::new(Mutex::new(RouterState::default()));

        let client = Arc::new(Self {
            next_id: AtomicU64::new(1),
            outbound_tx: outbound_tx.clone(),
            events_tx: events_tx.clone(),
            disconnected_rx,
            state: state.clone(),
            timeout,
        });

        tokio::spawn(async move {
            let mut reassembler = ChunkReassembler::default();
            while let Some(raw) = inbound_rx.recv().await {
                let parsed: WireMessage = match serde_json::from_slice(&raw) {
                    Ok(msg) => msg,
                    Err(err) => {
                        tracing::warn!("ignoring malformed JSON frame from extension: {err}");
                        continue;
                    }
                };

                let complete = match reassembler.feed(parsed) {
                    Ok(Some(msg)) => msg,
                    Ok(None) => continue,
                    Err(err) => {
                        tracing::warn!("chunk reassembly error: {err}");
                        continue;
                    }
                };

                handle_inbound_message(complete, &state, &events_tx, &outbound_tx).await;
            }

            // Fail all outstanding requests cleanly when the pipe closes.
            let drained: Vec<PendingCall> = {
                let mut guard = state.lock().expect("router mutex poisoned");
                guard.disconnected = true;
                guard.pending_by_tab.clear();
                guard.pending.drain().map(|(_, call)| call).collect()
            };
            for call in drained {
                let _ = call.tx.send(Err(CdpError::Disconnected));
            }
            let _ = disconnected_tx.send(true);
        });

        client
    }

    /// Resolves as soon as the underlying Native Messaging pipe reaches EOF and disconnects,
    /// even if callers still hold `Arc<NativeBridgeClient>` or `events` receivers.
    pub async fn wait_disconnected(&self) {
        let mut rx = self.disconnected_rx.clone();
        let _ = rx.wait_for(|&closed| closed).await;
    }

    pub fn extension_hello(&self) -> Option<HelloPayload> {
        self.state
            .lock()
            .expect("router mutex poisoned")
            .hello
            .clone()
    }

    pub async fn send_control(
        &self,
        method: &str,
        params: Value,
        tab_id: Option<i64>,
    ) -> Result<Value, CdpError> {
        {
            let guard = self.state.lock().expect("router mutex poisoned");
            if guard.disconnected {
                return Err(CdpError::Disconnected);
            }
        }

        let id = self.next_id.fetch_add(1, Ordering::Relaxed);
        let msg = WireMessage::request(id, method, params);
        let frames = encode_wire_message(&msg).map_err(|err| CdpError::Protocol {
            code: None,
            message: err.to_string(),
        })?;

        let (tx, rx) = oneshot::channel();
        {
            let mut guard = self.state.lock().expect("router mutex poisoned");
            if guard.disconnected {
                return Err(CdpError::Disconnected);
            }
            guard.pending.insert(id, PendingCall { tab_id, tx });
            if let Some(tid) = tab_id {
                guard.pending_by_tab.entry(tid).or_default().insert(id);
            }
        }

        for frame in frames {
            if self.outbound_tx.send(frame).await.is_err() {
                self.remove_pending(id);
                return Err(CdpError::Disconnected);
            }
        }

        match tokio::time::timeout(self.timeout, rx).await {
            Ok(Ok(result)) => result,
            Ok(Err(_)) => Err(CdpError::Disconnected),
            Err(_) => {
                self.remove_pending(id);
                Err(CdpError::Timeout(self.timeout.as_millis() as u64))
            }
        }
    }

    fn remove_pending(&self, id: u64) {
        let mut guard = self.state.lock().expect("router mutex poisoned");
        if let Some(call) = guard.pending.remove(&id)
            && let Some(tid) = call.tab_id
            && let Some(set) = guard.pending_by_tab.get_mut(&tid)
        {
            set.remove(&id);
            if set.is_empty() {
                guard.pending_by_tab.remove(&tid);
            }
        }
    }
}

async fn handle_inbound_message(
    msg: WireMessage,
    state: &Arc<Mutex<RouterState>>,
    events_tx: &broadcast::Sender<CdpEvent>,
    outbound_tx: &mpsc::Sender<Vec<u8>>,
) {
    match msg {
        WireMessage::Response {
            id,
            ok,
            result,
            error,
            ..
        } => {
            let pending = {
                let mut guard = state.lock().expect("router mutex poisoned");
                let call = guard.pending.remove(&id);
                if let Some(ref entry) = call
                    && let Some(tid) = entry.tab_id
                    && let Some(set) = guard.pending_by_tab.get_mut(&tid)
                {
                    set.remove(&id);
                    if set.is_empty() {
                        guard.pending_by_tab.remove(&tid);
                    }
                }
                call
            };

            if let Some(call) = pending {
                let outcome = if ok {
                    Ok(result.unwrap_or(Value::Null))
                } else {
                    let wire_err = error.unwrap_or(ferrite_core::WireError {
                        code: "CDP_ERROR".to_string(),
                        message: "unknown extension error".to_string(),
                    });
                    Err(classify_cdp_error(wire_err.message, None))
                };
                let _ = call.tx.send(outcome);
            }
        }
        WireMessage::Event { event, payload, .. } => match event.as_str() {
            "hello" => {
                if let Ok(hello) = serde_json::from_value::<HelloPayload>(payload) {
                    let mut guard = state.lock().expect("router mutex poisoned");
                    guard.hello = Some(hello);
                }
            }
            "cdp.event" => {
                if let Ok(cdp) = serde_json::from_value::<CdpEventPayload>(payload) {
                    let _ = events_tx.send(CdpEvent {
                        tab_id: cdp.tab_id,
                        method: cdp.method,
                        params: cdp.params,
                        session_id: cdp.session_id,
                    });
                }
            }
            "debugger.detached" => {
                if let Ok(detached) = serde_json::from_value::<DebuggerDetachedPayload>(payload) {
                    let evicted: Vec<PendingCall> = {
                        let mut guard = state.lock().expect("router mutex poisoned");
                        let ids = guard
                            .pending_by_tab
                            .remove(&detached.tab_id)
                            .unwrap_or_default();
                        ids.into_iter()
                            .filter_map(|id| guard.pending.remove(&id))
                            .collect()
                    };
                    for call in evicted {
                        let _ = call
                            .tx
                            .send(Err(CdpError::Detached(detached.reason.clone())));
                    }
                    let _ = events_tx.send(CdpEvent {
                        tab_id: detached.tab_id,
                        method: "Inspector.detached".to_string(),
                        params: json!({ "reason": detached.reason }),
                        session_id: None,
                    });
                }
            }
            other => {
                tracing::debug!(event = other, "received extension lifecycle event");
            }
        },
        WireMessage::Request { id, method, .. } => {
            if method == "ping" {
                let pong = WireMessage::response_ok(id, json!({ "pong": true }));
                if let Ok(frames) = encode_wire_message(&pong) {
                    for frame in frames {
                        let _ = outbound_tx.send(frame).await;
                    }
                }
            }
        }
        WireMessage::Chunk { .. } => {}
    }
}

#[async_trait]
impl CdpTransport for NativeBridgeClient {
    async fn send_cdp(
        &self,
        tab_id: i64,
        method: &str,
        params: Value,
        session_id: Option<&str>,
    ) -> Result<Value, CdpError> {
        let payload = serde_json::to_value(CdpSendParams {
            tab_id,
            method: method.to_string(),
            params,
            session_id: session_id.map(str::to_string),
        })
        .map_err(|err| CdpError::InvalidArgument(err.to_string()))?;

        self.send_control("cdp.send", payload, Some(tab_id)).await
    }

    async fn attach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        self.send_control("debugger.attach", json!({ "tabId": tab_id }), Some(tab_id))
            .await?;
        Ok(())
    }

    async fn detach_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        self.send_control("debugger.detach", json!({ "tabId": tab_id }), Some(tab_id))
            .await?;
        Ok(())
    }

    async fn list_tabs(&self) -> Result<Vec<TabInfo>, CdpError> {
        let result = self.send_control("tab.list", json!({}), None).await?;
        let tabs_val = result.get("tabs").cloned().unwrap_or(result);
        serde_json::from_value(tabs_val).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("invalid tab.list response: {err}"),
        })
    }

    async fn create_tab(&self, url: &str, active: bool) -> Result<TabInfo, CdpError> {
        let result = self
            .send_control("tab.create", json!({ "url": url, "active": active }), None)
            .await?;
        serde_json::from_value(result).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("invalid tab.create response: {err}"),
        })
    }

    async fn close_tab(&self, tab_id: i64) -> Result<(), CdpError> {
        self.send_control("tab.close", json!({ "tabId": tab_id }), Some(tab_id))
            .await?;
        Ok(())
    }

    fn subscribe_events(&self) -> broadcast::Receiver<CdpEvent> {
        self.events_tx.subscribe()
    }
}

#[cfg(test)]
mod tests {
    use ferrite_core::{MAX_EXT_TO_HOST_BYTES, MAX_HOST_TO_EXT_BYTES};

    use super::*;
    use crate::codec::{read_frame_async, write_frame_async};
    use crate::stdio_bridge::AsyncFrameBridge;

    #[tokio::test]
    async fn correlates_cdp_calls_and_cancels_on_debugger_detach() {
        let (host_stream, mut ext_stream) = tokio::io::duplex(256 * 1024);
        let (host_reader, host_writer) = tokio::io::split(host_stream);
        let bridge = AsyncFrameBridge::spawn(
            host_reader,
            host_writer,
            MAX_EXT_TO_HOST_BYTES,
            MAX_HOST_TO_EXT_BYTES,
        );
        let client = NativeBridgeClient::connect_with_timeout(
            bridge.outbound_tx,
            bridge.inbound_rx,
            Duration::from_secs(5),
        );

        let mut events = client.subscribe_events();

        // Spawn simulated Chrome extension side.
        let ext_task = tokio::spawn(async move {
            // Send hello event first.
            let hello = WireMessage::event(
                "hello",
                json!({
                    "extensionId": "test-ext-id",
                    "extensionVersion": "0.1.0",
                    "attachedTabs": [101]
                }),
            );
            let hello_bytes = serde_json::to_vec(&hello).unwrap();
            write_frame_async(&mut ext_stream, &hello_bytes, MAX_EXT_TO_HOST_BYTES)
                .await
                .unwrap();

            // First request: Page.navigate -> answer OK + emit CDP event.
            let req1_bytes = read_frame_async(&mut ext_stream, MAX_HOST_TO_EXT_BYTES)
                .await
                .unwrap()
                .unwrap();
            let req1: WireMessage = serde_json::from_slice(&req1_bytes).unwrap();
            let WireMessage::Request { id: id1, .. } = req1 else {
                panic!("expected request");
            };
            let resp1 = WireMessage::response_ok(id1, json!({ "frameId": "F1" }));
            write_frame_async(
                &mut ext_stream,
                &serde_json::to_vec(&resp1).unwrap(),
                MAX_EXT_TO_HOST_BYTES,
            )
            .await
            .unwrap();

            let cdp_ev = WireMessage::event(
                "cdp.event",
                json!({
                    "tabId": 101,
                    "method": "Page.loadEventFired",
                    "params": { "timestamp": 123.45 }
                }),
            );
            write_frame_async(
                &mut ext_stream,
                &serde_json::to_vec(&cdp_ev).unwrap(),
                MAX_EXT_TO_HOST_BYTES,
            )
            .await
            .unwrap();

            // Second request on tab 101: simulate user closing DevTools / debugger detach before response.
            let _req2_bytes = read_frame_async(&mut ext_stream, MAX_HOST_TO_EXT_BYTES)
                .await
                .unwrap()
                .unwrap();
            let detached = WireMessage::event(
                "debugger.detached",
                json!({ "tabId": 101, "reason": "canceled_by_user" }),
            );
            write_frame_async(
                &mut ext_stream,
                &serde_json::to_vec(&detached).unwrap(),
                MAX_EXT_TO_HOST_BYTES,
            )
            .await
            .unwrap();
        });

        let nav_res = client
            .send_cdp(101, "Page.navigate", json!({ "url": "https://example.com" }), None)
            .await
            .unwrap();
        assert_eq!(nav_res["frameId"], "F1");
        assert_eq!(client.extension_hello().unwrap().extension_id, "test-ext-id");

        let ev = events.recv().await.unwrap();
        assert_eq!(ev.method, "Page.loadEventFired");

        let detached_err = client
            .send_cdp(101, "Page.captureScreenshot", json!({}), None)
            .await
            .unwrap_err();
        assert!(
            matches!(detached_err, CdpError::Detached(ref r) if r == "canceled_by_user"),
            "unexpected error: {detached_err:?}"
        );

        ext_task.await.unwrap();
    }

    #[tokio::test]
    async fn signals_wait_disconnected_even_while_client_arc_is_held() {
        let (host_stream, ext_stream) = tokio::io::duplex(64 * 1024);
        let (host_reader, host_writer) = tokio::io::split(host_stream);
        let bridge = AsyncFrameBridge::spawn(
            host_reader,
            host_writer,
            MAX_EXT_TO_HOST_BYTES,
            MAX_HOST_TO_EXT_BYTES,
        );
        let client = NativeBridgeClient::connect_with_timeout(
            bridge.outbound_tx,
            bridge.inbound_rx,
            Duration::from_secs(5),
        );
        // Keep an extra Arc and broadcast receiver alive (mirroring E2eSession + WsServer).
        let held_arc = Arc::clone(&client);
        let _held_rx = held_arc.subscribe_events();

        drop(ext_stream);

        tokio::time::timeout(Duration::from_secs(1), client.wait_disconnected())
            .await
            .expect("wait_disconnected must resolve promptly when pipe reaches EOF");
    }
}
