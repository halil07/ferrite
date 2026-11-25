const SESSION_KEY = "ferriteAttachedTabs";
const RESTRICTED_PREFIXES = [
  "chrome://",
  "chrome-extension://",
  "chrome-search://",
  "edge://",
  "devtools://",
  "view-source:",
  "https://chrome.google.com/webstore",
  "https://chromewebstore.google.com",
];

const attached = new Set();
const pending = new Map();

let eventListenersBound = false;

export function isAttachableUrl(url = "") {
  const lower = url.toLowerCase();
  return !RESTRICTED_PREFIXES.some((prefix) => lower.startsWith(prefix));
}

function normalizeNavigateUrl(url) {
  try {
    const parsed = new URL(url);
    if (
      parsed.protocol === "about:" &&
      parsed.pathname.toLowerCase() === "blank" &&
      parsed.search === "" &&
      url.includes("#")
    ) {
      return "about:blank";
    }
  } catch {
    // Leave invalid URLs for Chromium CDP validation.
  }
  return url;
}

async function persistAttachedTabs() {
  const entries = [];
  for (const tabId of attached) {
    const tab = await chrome.tabs.get(tabId).catch(() => null);
    if (tab) {
      entries.push({ tabId, url: tab.url || tab.pendingUrl || "" });
    }
  }
  await chrome.storage.session.set({ [SESSION_KEY]: entries }).catch(() => {});
}

async function ownedByUs(tabId) {
  try {
    // Use Page.getFrameTree instead of Runtime.evaluate to avoid activating Runtime.enable.
    await chrome.debugger.sendCommand({ tabId }, "Page.getFrameTree");
    return true;
  } catch {
    return false;
  }
}

export function attachedTabs() {
  return Array.from(attached);
}

export function isAttached(tabId) {
  return attached.has(tabId);
}

export async function attach(tabId) {
  if (attached.has(tabId)) {
    return;
  }
  if (pending.has(tabId)) {
    return pending.get(tabId);
  }

  const task = (async () => {
    const tab = await chrome.tabs.get(tabId);
    const url = tab.pendingUrl || tab.url || "";
    if (!isAttachableUrl(url)) {
      throw new Error(`Cannot attach chrome.debugger to restricted URL: ${url}`);
    }

    try {
      await chrome.debugger.attach({ tabId }, "1.3");
    } catch (error) {
      const msg = error instanceof Error ? error.message : String(error);
      if (/already attached/i.test(msg) && (await ownedByUs(tabId))) {
        // Attachment survived an MV3 Service Worker sleep/restart cycle.
      } else if (/already attached/i.test(msg)) {
        throw new Error(
          "Another debugger (DevTools or another extension) is attached to this tab. Close DevTools and retry."
        );
      } else {
        throw new Error(`chrome.debugger.attach failed for tab ${tabId}: ${msg}`);
      }
    }

    attached.add(tabId);
    // Keep background tabs from throttling timers/rAF during E2E execution.
    await chrome.debugger
      .sendCommand({ tabId }, "Emulation.setFocusEmulationEnabled", { enabled: true })
      .catch(() => {});
    await persistAttachedTabs();
  })().finally(() => {
    pending.delete(tabId);
  });

  pending.set(tabId, task);
  return task;
}

export async function detach(tabId) {
  attached.delete(tabId);
  await chrome.debugger.detach({ tabId }).catch(() => {});
  await persistAttachedTabs();
}

export async function restore() {
  const stored = await chrome.storage.session.get(SESSION_KEY).catch(() => ({}));
  const items = Array.isArray(stored[SESSION_KEY]) ? stored[SESSION_KEY] : [];

  await Promise.all(
    items.map(async (item) => {
      if (!item || typeof item.tabId !== "number") return;
      const tab = await chrome.tabs.get(item.tabId).catch(() => null);
      if (!tab) return;

      if (await ownedByUs(item.tabId)) {
        attached.add(item.tabId);
        return;
      }

      const currentUrl = tab.url || tab.pendingUrl || "";
      if (item.url && currentUrl === item.url && isAttachableUrl(currentUrl)) {
        await attach(item.tabId).catch(() => {});
      }
    })
  );

  await persistAttachedTabs();
}

export async function sendCommand({ tabId, method, params = {}, sessionId = undefined }) {
  if (!attached.has(tabId)) {
    await attach(tabId);
  }

  let sendParams = params;
  if (method === "Page.navigate" && typeof params?.url === "string") {
    const normalized = normalizeNavigateUrl(params.url);
    if (normalized !== params.url) {
      sendParams = { ...params, url: normalized };
    }
  }

  const debuggee = sessionId ? { tabId, sessionId } : { tabId };
  const result = await chrome.debugger.sendCommand(debuggee, method, sendParams);
  return result ?? {};
}

export function registerListeners({ onCdpEvent, onDetached }) {
  if (eventListenersBound) return;
  eventListenersBound = true;

  chrome.debugger.onEvent.addListener((source, method, params) => {
    if (typeof source.tabId !== "number") return;
    onCdpEvent({
      tabId: source.tabId,
      method,
      params: params ?? {},
      sessionId: source.sessionId ?? undefined,
    });
  });

  chrome.debugger.onDetach.addListener((source, reason) => {
    if (typeof source.tabId !== "number") return;
    attached.delete(source.tabId);
    void persistAttachedTabs();
    onDetached({
      tabId: source.tabId,
      reason: reason || "detached",
    });
  });

  chrome.tabs.onRemoved.addListener((tabId) => {
    if (attached.delete(tabId)) {
      void persistAttachedTabs();
      onDetached({ tabId, reason: "tab_closed" });
    }
  });
}
