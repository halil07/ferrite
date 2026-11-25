#![forbid(unsafe_code)]

//! Shared contracts, protocol envelopes, error taxonomy, and CDP transport abstraction for Ferrite E2E.

pub mod error;
pub mod protocol;
pub mod transport;

pub use error::{CdpError, classify_cdp_error};
pub use protocol::{
    CHUNK_RAW_BYTES, CHUNK_THRESHOLD_BYTES, CdpEventPayload, CdpSendParams, DebuggerDetachedPayload,
    HelloPayload, MAX_EXT_TO_HOST_BYTES, MAX_HOST_TO_EXT_BYTES, MAX_REASSEMBLED_BYTES,
    NATIVE_HOST_NAME, PROTOCOL_VERSION, TabInfo, WireError, WireMessage, WsEvent, WsRequest,
    WsResponse,
};
pub use transport::{CdpEvent, CdpTransport};
