import { handleHostRequest } from "./cdp-proxy.js";
import * as DebuggerHub from "./debugger-hub.js";
import { NativeBridge } from "./native-bridge.js";

const KEEPALIVE_ALARM = "ferrite-keepalive";

const bridge = new NativeBridge({
  onRequest: handleHostRequest,
  getHelloPayload: async () => ({
    extensionId: chrome.runtime.id,
    extensionVersion: chrome.runtime.getManifest().version,
    attachedTabs: DebuggerHub.attachedTabs(),
  }),
});

DebuggerHub.registerListeners({
  onCdpEvent: (payload) => {
    bridge.sendEvent("cdp.event", payload);
  },
  onDetached: (payload) => {
    bridge.sendEvent("debugger.detached", payload);
  },
});

let bootstrapPromise = null;

async function bootstrap() {
  if (bootstrapPromise) {
    return bootstrapPromise;
  }
  bootstrapPromise = (async () => {
    await DebuggerHub.restore();
    bridge.connect();
  })().finally(() => {
    bootstrapPromise = null;
  });
  return bootstrapPromise;
}

chrome.runtime.onInstalled.addListener(() => {
  void chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
  void bootstrap();
});

chrome.runtime.onStartup.addListener(() => {
  void chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
  void bootstrap();
});

chrome.alarms.onAlarm.addListener((alarm) => {
  if (alarm.name !== KEEPALIVE_ALARM) return;
  if (!bridge.isConnected()) {
    bridge.connect();
  } else {
    // Proactive heartbeat detects half-open native messaging pipes and keeps the MV3 worker healthy.
    bridge.sendEvent("heartbeat", { ts: Date.now() });
  }
});

chrome.runtime.onMessage.addListener((message, _sender, sendResponse) => {
  void (async () => {
    switch (message?.action) {
      case "get-status": {
        const [activeTab] = await chrome.tabs.query({ active: true, currentWindow: true });
        sendResponse({
          bridge: bridge.status(),
          attachedTabs: DebuggerHub.attachedTabs(),
          activeTab: activeTab
            ? {
                id: activeTab.id,
                url: activeTab.url || "",
                attached: typeof activeTab.id === "number" && DebuggerHub.isAttached(activeTab.id),
              }
            : null,
        });
        break;
      }
      case "attach-active-tab": {
        const [activeTab] = await chrome.tabs.query({ active: true, currentWindow: true });
        if (!activeTab || typeof activeTab.id !== "number") {
          sendResponse({ ok: false, error: "No active tab found" });
          return;
        }
        try {
          await DebuggerHub.attach(activeTab.id);
          sendResponse({ ok: true, tabId: activeTab.id });
        } catch (err) {
          sendResponse({ ok: false, error: err instanceof Error ? err.message : String(err) });
        }
        break;
      }
      case "detach-active-tab": {
        const [activeTab] = await chrome.tabs.query({ active: true, currentWindow: true });
        if (activeTab && typeof activeTab.id === "number") {
          await DebuggerHub.detach(activeTab.id);
        }
        sendResponse({ ok: true });
        break;
      }
      case "reconnect-native": {
        bridge.disconnect();
        bridge.connect();
        sendResponse({ ok: true });
        break;
      }
      default:
        sendResponse({ ok: false, error: "Unknown popup action" });
    }
  })();
  return true;
});

// Immediately restore and connect whenever Chrome wakes the Service Worker.
void chrome.alarms.create(KEEPALIVE_ALARM, { periodInMinutes: 0.5 });
void bootstrap();
