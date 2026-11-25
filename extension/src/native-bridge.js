import { ChunkReassembler, NATIVE_HOST_NAME, PROTOCOL_VERSION, splitOutgoingMessage } from "./protocol.js";

export class NativeBridge {
  constructor({ onRequest, getHelloPayload }) {
    this.onRequest = onRequest;
    this.getHelloPayload = getHelloPayload;
    this.port = null;
    this.reassembler = new ChunkReassembler();
    this.reconnectAttempts = 0;
    this.reconnectTimer = null;
    this.lastError = null;
  }

  isConnected() {
    return this.port !== null;
  }

  status() {
    return {
      connected: this.isConnected(),
      hostName: NATIVE_HOST_NAME,
      reconnectAttempts: this.reconnectAttempts,
      lastError: this.lastError,
    };
  }

  connect() {
    if (this.port) return;

    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }

    try {
      const port = chrome.runtime.connectNative(NATIVE_HOST_NAME);
      this.port = port;
      this.lastError = null;

      port.onMessage.addListener((raw) => {
        void this.#handleInbound(raw);
      });

      port.onDisconnect.addListener(() => {
        const err = chrome.runtime.lastError?.message || "Native host disconnected";
        this.lastError = err;
        this.port = null;
        this.reassembler = new ChunkReassembler();
        console.warn("[Ferrite NativeBridge] Disconnected:", err);
        this.#scheduleReconnect();
      });

      this.reconnectAttempts = 0;
      void this.#sendHello();
    } catch (error) {
      this.port = null;
      this.lastError = error instanceof Error ? error.message : String(error);
      this.#scheduleReconnect();
    }
  }

  disconnect() {
    if (this.reconnectTimer !== null) {
      clearTimeout(this.reconnectTimer);
      this.reconnectTimer = null;
    }
    if (this.port) {
      this.port.disconnect();
      this.port = null;
    }
  }

  sendEvent(event, payload) {
    this.sendRaw({
      v: PROTOCOL_VERSION,
      kind: "event",
      event,
      payload,
    });
  }

  sendRaw(msg) {
    if (!this.port) return false;
    try {
      const frames = splitOutgoingMessage(msg);
      for (const frame of frames) {
        this.port.postMessage(frame);
      }
      return true;
    } catch (error) {
      this.lastError = error instanceof Error ? error.message : String(error);
      this.port = null;
      this.#scheduleReconnect();
      return false;
    }
  }

  async #sendHello() {
    const payload = await this.getHelloPayload();
    this.sendEvent("hello", payload);
  }

  async #handleInbound(raw) {
    let msg;
    try {
      msg = this.reassembler.feed(raw);
    } catch (error) {
      console.error("[Ferrite NativeBridge] Chunk reassembly failed:", error);
      return;
    }
    if (!msg) return;

    if (msg.kind === "request") {
      const response = await this.onRequest(msg);
      this.sendRaw(response);
    }
  }

  #scheduleReconnect() {
    if (this.reconnectTimer !== null) return;
    const delayMs = Math.min(1_000 * Math.pow(2, this.reconnectAttempts), 30_000);
    this.reconnectAttempts += 1;
    this.reconnectTimer = setTimeout(() => {
      this.reconnectTimer = null;
      this.connect();
    }, delayMs);
  }
}
