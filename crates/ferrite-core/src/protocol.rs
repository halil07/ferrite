#![forbid(unsafe_code)]

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Wire protocol version spoken between the MV3 extension and the Rust Native Messaging host.
pub const PROTOCOL_VERSION: u32 = 1;

/// Registered Native Messaging host identifier.
pub const NATIVE_HOST_NAME: &str = "com.ferrite.e2e";

/// Chrome strictly limits a single Native Messaging frame sent from the native host to the extension to 1 MiB.
pub const MAX_HOST_TO_EXT_BYTES: usize = 1024 * 1024;

/// Chrome allows up to 64 MiB from the extension to the host, but we enforce a defensive 16 MiB cap before allocating.
pub const MAX_EXT_TO_HOST_BYTES: usize = 16 * 1024 * 1024;

/// Raw UTF-8 payload slice size before base64 encoding (~683 KiB encoded + JSON header, safely below 1 MiB).
pub const CHUNK_RAW_BYTES: usize = 512 * 1024;

/// Messages exceeding this serialized byte length are split into [`WireMessage::Chunk`] frames.
pub const CHUNK_THRESHOLD_BYTES: usize = 700 * 1024;

/// Upper bound on reassembled multi-chunk messages to prevent memory exhaustion.
pub const MAX_REASSEMBLED_BYTES: usize = 32 * 1024 * 1024;

/// Primary bidirectional envelope exchanged over Native Messaging stdio framing.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WireMessage {
    Request {
        v: u32,
        id: u64,
        method: String,
        #[serde(default)]
        params: Value,
    },
    Response {
        v: u32,
        id: u64,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<Value>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<WireError>,
    },
    Event {
        v: u32,
        event: String,
        #[serde(default)]
        payload: Value,
    },
    Chunk {
        v: u32,
        transfer_id: String,
        index: u32,
        total: u32,
        data: String,
    },
}

impl WireMessage {
    pub fn request(id: u64, method: impl Into<String>, params: Value) -> Self {
        Self::Request {
            v: PROTOCOL_VERSION,
            id,
            method: method.into(),
            params,
        }
    }

    pub fn response_ok(id: u64, result: Value) -> Self {
        Self::Response {
            v: PROTOCOL_VERSION,
            id,
            ok: true,
            result: Some(result),
            error: None,
        }
    }

    pub fn response_err(id: u64, code: impl Into<String>, message: impl Into<String>) -> Self {
        Self::Response {
            v: PROTOCOL_VERSION,
            id,
            ok: false,
            result: None,
            error: Some(WireError {
                code: code.into(),
                message: message.into(),
            }),
        }
    }

    pub fn event(event: impl Into<String>, payload: Value) -> Self {
        Self::Event {
            v: PROTOCOL_VERSION,
            event: event.into(),
            payload,
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct WireError {
    pub code: String,
    pub message: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct TabInfo {
    pub tab_id: i64,
    #[serde(default)]
    pub window_id: i64,
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub title: String,
    #[serde(default)]
    pub active: bool,
    #[serde(default)]
    pub attached: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct HelloPayload {
    pub extension_id: String,
    pub extension_version: String,
    #[serde(default)]
    pub attached_tabs: Vec<i64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CdpSendParams {
    pub tab_id: i64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct CdpEventPayload {
    pub tab_id: i64,
    pub method: String,
    #[serde(default)]
    pub params: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DebuggerDetachedPayload {
    pub tab_id: i64,
    pub reason: String,
}

/// WebSocket RPC request from an E2E test client or CLI runner to `ferrite-daemon`.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WsRequest {
    pub id: u64,
    pub action: String,
    #[serde(default)]
    pub params: Value,
}

/// WebSocket RPC response from `ferrite-daemon` to a connected test client.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WsResponse {
    pub id: u64,
    pub ok: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// WebSocket async notification pushed from `ferrite-daemon` to subscribed test clients.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct WsEvent {
    pub event: String,
    pub payload: Value,
}
