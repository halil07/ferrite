export const PROTOCOL_VERSION = 1;
export const NATIVE_HOST_NAME = "com.ferrite.e2e";

// Chrome Native Messaging caps host -> extension frames at 1 MiB.
// Keep chunks safely below 1 MiB after base64 + JSON envelope overhead.
export const CHUNK_RAW_BYTES = 512 * 1024;
export const CHUNK_THRESHOLD_BYTES = 700 * 1024;
export const MAX_REASSEMBLED_BYTES = 32 * 1024 * 1024;
export const MAX_INFLIGHT_TRANSFERS = 32;
export const TRANSFER_TTL_MS = 30_000;

const textEncoder = new TextEncoder();
const textDecoder = new TextDecoder();

let transferSeq = 1;

export function encodeBase64(bytes) {
  const CHUNK = 0x8000;
  let binary = "";
  for (let i = 0; i < bytes.length; i += CHUNK) {
    const slice = bytes.subarray(i, i + CHUNK);
    binary += String.fromCharCode.apply(null, slice);
  }
  return btoa(binary);
}

export function decodeBase64(b64) {
  const binary = atob(b64);
  const bytes = new Uint8Array(binary.length);
  for (let i = 0; i < binary.length; i++) {
    bytes[i] = binary.charCodeAt(i);
  }
  return bytes;
}

/**
 * Serializes a WireMessage object and splits it into Chunk envelopes if its UTF-8 JSON
 * representation exceeds CHUNK_THRESHOLD_BYTES (700 KiB).
 */
export function splitOutgoingMessage(msg) {
  const jsonText = JSON.stringify(msg);
  const utf8 = textEncoder.encode(jsonText);
  if (utf8.byteLength <= CHUNK_THRESHOLD_BYTES) {
    return [msg];
  }

  const transferId = `ext-tx-${transferSeq++}`;
  const total = Math.ceil(utf8.byteLength / CHUNK_RAW_BYTES);
  const frames = [];

  for (let index = 0; index < total; index++) {
    const start = index * CHUNK_RAW_BYTES;
    const end = Math.min(start + CHUNK_RAW_BYTES, utf8.byteLength);
    frames.push({
      v: PROTOCOL_VERSION,
      kind: "chunk",
      transfer_id: transferId,
      index,
      total,
      data: encodeBase64(utf8.subarray(start, end)),
    });
  }

  return frames;
}

export class ChunkReassembler {
  constructor() {
    this.transfers = new Map();
  }

  evictExpired(now = Date.now()) {
    for (const [id, entry] of this.transfers.entries()) {
      if (now - entry.createdAt >= TRANSFER_TTL_MS) {
        this.transfers.delete(id);
      }
    }
  }

  feed(msg) {
    if (!msg || msg.kind !== "chunk") {
      return msg;
    }

    const { v, transfer_id: transferId, index, total, data } = msg;
    if (v !== PROTOCOL_VERSION) {
      throw new Error(`Unsupported chunk protocol version: ${v}`);
    }
    if (!transferId || !Number.isInteger(total) || total <= 0 || !Number.isInteger(index) || index < 0 || index >= total) {
      throw new Error(`Invalid chunk header for ${transferId}`);
    }

    const now = Date.now();
    this.evictExpired(now);

    if (!this.transfers.has(transferId) && this.transfers.size >= MAX_INFLIGHT_TRANSFERS) {
      throw new Error("Too many concurrent chunked transfers");
    }

    const bytes = decodeBase64(data);
    let entry = this.transfers.get(transferId);
    if (!entry) {
      entry = {
        total,
        received: 0,
        totalBytes: 0,
        parts: new Array(total).fill(null),
        createdAt: now,
      };
      this.transfers.set(transferId, entry);
    }

    if (entry.total !== total || entry.parts[index] !== null) {
      this.transfers.delete(transferId);
      throw new Error(`Corrupt or duplicate chunk ${index} for ${transferId}`);
    }

    const nextBytes = entry.totalBytes + bytes.byteLength;
    if (nextBytes > MAX_REASSEMBLED_BYTES) {
      this.transfers.delete(transferId);
      throw new Error(`Chunked transfer ${transferId} exceeded ${MAX_REASSEMBLED_BYTES} bytes`);
    }

    entry.parts[index] = bytes;
    entry.totalBytes = nextBytes;
    entry.received += 1;

    if (entry.received < entry.total) {
      return null;
    }

    this.transfers.delete(transferId);
    const combined = new Uint8Array(entry.totalBytes);
    let offset = 0;
    for (const part of entry.parts) {
      combined.set(part, offset);
      offset += part.byteLength;
    }

    const inner = JSON.parse(textDecoder.decode(combined));
    if (inner?.kind === "chunk") {
      throw new Error("Nested chunk frames are disallowed");
    }
    return inner;
  }
}
