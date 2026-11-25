#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD;
use ferrite_core::{
    CHUNK_RAW_BYTES, CHUNK_THRESHOLD_BYTES, MAX_HOST_TO_EXT_BYTES, MAX_REASSEMBLED_BYTES,
    PROTOCOL_VERSION, WireMessage,
};

use crate::codec::IpcError;

pub const MAX_INFLIGHT_TRANSFERS: usize = 32;
pub const TRANSFER_TTL: Duration = Duration::from_secs(30);

fn next_transfer_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("tx-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

/// Serializes a [`WireMessage`] into one or more Native Messaging JSON payloads, automatically
/// splitting payloads larger than [`CHUNK_THRESHOLD_BYTES`] into [`WireMessage::Chunk`] frames
/// so no single host-to-extension frame ever exceeds Chrome's 1 MiB hard cap.
pub fn encode_wire_message(msg: &WireMessage) -> Result<Vec<Vec<u8>>, IpcError> {
    encode_wire_message_with_limits(msg, CHUNK_THRESHOLD_BYTES, CHUNK_RAW_BYTES)
}

pub fn encode_wire_message_with_limits(
    msg: &WireMessage,
    threshold_bytes: usize,
    raw_chunk_bytes: usize,
) -> Result<Vec<Vec<u8>>, IpcError> {
    let encoded = serde_json::to_vec(msg)?;
    if encoded.len() <= threshold_bytes {
        return Ok(vec![encoded]);
    }

    let transfer_id = next_transfer_id();
    let chunks: Vec<&[u8]> = encoded.chunks(raw_chunk_bytes.max(1)).collect();
    let total = u32::try_from(chunks.len())
        .map_err(|_| IpcError::InvalidChunk("too many chunks".to_string()))?;

    let mut frames = Vec::with_capacity(chunks.len());
    for (idx, slice) in chunks.into_iter().enumerate() {
        let chunk_msg = WireMessage::Chunk {
            v: PROTOCOL_VERSION,
            transfer_id: transfer_id.clone(),
            index: idx as u32,
            total,
            data: STANDARD.encode(slice),
        };
        let frame = serde_json::to_vec(&chunk_msg)?;
        if frame.len() > MAX_HOST_TO_EXT_BYTES {
            return Err(IpcError::FrameTooLarge {
                len: frame.len(),
                max_bytes: MAX_HOST_TO_EXT_BYTES,
            });
        }
        frames.push(frame);
    }

    Ok(frames)
}

struct PartialTransfer {
    total: u32,
    received_count: u32,
    total_bytes: usize,
    parts: Vec<Option<Vec<u8>>>,
    created_at: Instant,
}

/// Reassembles incoming [`WireMessage::Chunk`] frames (including out-of-order delivery)
/// with bounded concurrency, per-transfer TTL eviction, and total decoded size limits.
pub struct ChunkReassembler {
    transfers: HashMap<String, PartialTransfer>,
    max_inflight: usize,
    max_reassembled_bytes: usize,
    ttl: Duration,
}

impl Default for ChunkReassembler {
    fn default() -> Self {
        Self::new(MAX_INFLIGHT_TRANSFERS, MAX_REASSEMBLED_BYTES, TRANSFER_TTL)
    }
}

impl ChunkReassembler {
    pub fn new(max_inflight: usize, max_reassembled_bytes: usize, ttl: Duration) -> Self {
        Self {
            transfers: HashMap::new(),
            max_inflight,
            max_reassembled_bytes,
            ttl,
        }
    }

    pub fn inflight_count(&self) -> usize {
        self.transfers.len()
    }

    pub fn evict_expired(&mut self, now: Instant) {
        self.transfers
            .retain(|_, entry| now.saturating_duration_since(entry.created_at) < self.ttl);
    }

    pub fn feed(&mut self, msg: WireMessage) -> Result<Option<WireMessage>, IpcError> {
        self.feed_at(msg, Instant::now())
    }

    pub fn feed_at(
        &mut self,
        msg: WireMessage,
        now: Instant,
    ) -> Result<Option<WireMessage>, IpcError> {
        let WireMessage::Chunk {
            v,
            transfer_id,
            index,
            total,
            data,
        } = msg
        else {
            return Ok(Some(msg));
        };

        if v != PROTOCOL_VERSION {
            return Err(IpcError::InvalidChunk(format!(
                "unsupported chunk protocol version {v}"
            )));
        }
        if transfer_id.is_empty() || total == 0 || index >= total {
            return Err(IpcError::InvalidChunk(format!(
                "invalid metadata transfer_id={transfer_id:?} index={index} total={total}"
            )));
        }

        self.evict_expired(now);

        if !self.transfers.contains_key(&transfer_id) && self.transfers.len() >= self.max_inflight {
            return Err(IpcError::TooManyTransfers {
                limit: self.max_inflight,
            });
        }

        let decoded = STANDARD.decode(data.as_bytes())?;
        let total_usize = total as usize;

        let entry = self
            .transfers
            .entry(transfer_id.clone())
            .or_insert_with(|| PartialTransfer {
                total,
                received_count: 0,
                total_bytes: 0,
                parts: vec![None; total_usize],
                created_at: now,
            });

        if entry.total != total {
            self.transfers.remove(&transfer_id);
            return Err(IpcError::InvalidChunk(
                "mismatched total chunk count for transfer".to_string(),
            ));
        }

        let idx = index as usize;
        if entry.parts[idx].is_some() {
            self.transfers.remove(&transfer_id);
            return Err(IpcError::InvalidChunk(format!(
                "duplicate chunk index {index} for transfer {transfer_id}"
            )));
        }

        let new_total_bytes = entry.total_bytes.saturating_add(decoded.len());
        if new_total_bytes > self.max_reassembled_bytes {
            self.transfers.remove(&transfer_id);
            return Err(IpcError::ReassemblyTooLarge {
                max_bytes: self.max_reassembled_bytes,
            });
        }

        entry.total_bytes = new_total_bytes;
        entry.parts[idx] = Some(decoded);
        entry.received_count += 1;

        if entry.received_count < entry.total {
            return Ok(None);
        }

        let completed = self.transfers.remove(&transfer_id).expect("transfer exists");
        let mut combined = Vec::with_capacity(completed.total_bytes);
        for part in completed.parts {
            let slice = part.ok_or_else(|| {
                IpcError::InvalidChunk("missing chunk part during finalization".to_string())
            })?;
            combined.extend_from_slice(&slice);
        }

        let inner: WireMessage = serde_json::from_slice(&combined)?;
        if matches!(inner, WireMessage::Chunk { .. }) {
            return Err(IpcError::InvalidChunk(
                "nested chunk frames are disallowed".to_string(),
            ));
        }
        Ok(Some(inner))
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn splits_large_payload_below_1mib_and_reassembles_out_of_order() {
        let large_b64 = "A".repeat(1_500_000);
        let original = WireMessage::response_ok(42, json!({ "data": large_b64 }));

        let frames = encode_wire_message(&original).unwrap();
        assert!(frames.len() >= 3);
        for frame in &frames {
            assert!(
                frame.len() <= MAX_HOST_TO_EXT_BYTES,
                "frame size {} exceeded 1 MiB Chrome cap",
                frame.len()
            );
        }

        let mut parsed: Vec<WireMessage> = frames
            .iter()
            .map(|bytes| serde_json::from_slice(bytes).unwrap())
            .collect();
        // Deliver out of order: last, first, middle.
        let last = parsed.len() - 1;
        parsed.swap(0, last);

        let mut reassembler = ChunkReassembler::default();
        let mut output = None;
        for msg in parsed {
            if let Some(done) = reassembler.feed(msg).unwrap() {
                output = Some(done);
            }
        }

        assert_eq!(output.unwrap(), original);
        assert_eq!(reassembler.inflight_count(), 0);
    }

    #[test]
    fn rejects_reassembly_exceeding_max_bytes_and_evicts_expired() {
        let big = WireMessage::event("test", json!({ "blob": "x".repeat(4096) }));
        let frames = encode_wire_message_with_limits(&big, 512, 1024).unwrap();
        assert!(frames.len() >= 3);

        let mut reassembler = ChunkReassembler::new(4, 1500, Duration::from_secs(5));
        let first: WireMessage = serde_json::from_slice(&frames[0]).unwrap();
        let second: WireMessage = serde_json::from_slice(&frames[1]).unwrap();
        assert!(reassembler.feed(first).unwrap().is_none());
        assert!(matches!(
            reassembler.feed(second),
            Err(IpcError::ReassemblyTooLarge { max_bytes: 1500 })
        ));
        assert_eq!(reassembler.inflight_count(), 0);

        // Verify TTL eviction clears stale transfers.
        let mut ttl_reassembler = ChunkReassembler::new(4, 65_536, Duration::from_secs(5));
        let start = Instant::now();
        let first_again: WireMessage = serde_json::from_slice(&frames[0]).unwrap();
        assert!(ttl_reassembler.feed_at(first_again, start).unwrap().is_none());
        assert_eq!(ttl_reassembler.inflight_count(), 1);
        ttl_reassembler.evict_expired(start + Duration::from_secs(6));
        assert_eq!(ttl_reassembler.inflight_count(), 0);
    }
}
