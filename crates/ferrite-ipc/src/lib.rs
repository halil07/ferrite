#![forbid(unsafe_code)]

//! Chrome Native Messaging 4-byte little-endian framing, chunking (>1 MiB workaround),
//! Tokio stdio bridge, and multiplexed CDP transport client.

pub mod chunker;
pub mod codec;
pub mod router;
pub mod stdio_bridge;

pub use chunker::{ChunkReassembler, encode_wire_message};
pub use codec::{
    IpcError, read_frame_async, read_frame_sync, write_frame_async, write_frame_sync,
};
pub use router::NativeBridgeClient;
pub use stdio_bridge::{AsyncFrameBridge, StdioBridge};
