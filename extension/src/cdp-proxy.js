import * as DebuggerHub from "./debugger-hub.js";
import { PROTOCOL_VERSION } from "./protocol.js";

function ok(id, result) {
  return {
    v: PROTOCOL_VERSION,
    kind: "response",
    id,
    ok: true,
    result: result ?? {},
  };
}

function fail(id, code, message) {
  return {
    v: PROTOCOL_VERSION,
    kind: "response",
    id,
    ok: false,
    error: { code, message },
  };
}

function toTabInfo(tab) {
  return {
    tabId: tab.id,
    windowId: tab.windowId,
    url: tab.pendingUrl || tab.url || "",
    title: tab.title || "",
    active: Boolean(tab.active),
    attached: typeof tab.id === "number" ? DebuggerHub.isAttached(tab.id) : false,
  };
}

/**
 * Routes JSON RPC requests from the Rust Native Messaging host to chrome.tabs and chrome.debugger.
 */
export async function handleHostRequest(msg) {
  const { id, method, params = {} } = msg;

  try {
    switch (method) {
      case "ping":
        return ok(id, { pong: true, attachedTabs: DebuggerHub.attachedTabs() });

      case "tab.list": {
        const tabs = await chrome.tabs.query({});
        return ok(id, {
          tabs: tabs.filter((t) => typeof t.id === "number").map(toTabInfo),
        });
      }

      case "tab.create": {
        const url = typeof params.url === "string" ? params.url : "about:blank";
        const active = params.active !== false;
        const tab = await chrome.tabs.create({ url, active });
        if (typeof tab.id === "number" && DebuggerHub.isAttachableUrl(url)) {
          await DebuggerHub.attach(tab.id);
        }
        return ok(id, toTabInfo(tab));
      }

      case "tab.close": {
        if (typeof params.tabId !== "number") {
          return fail(id, "INVALID_PARAMS", "tab.close requires numeric tabId");
        }
        await DebuggerHub.detach(params.tabId);
        await chrome.tabs.remove(params.tabId);
        return ok(id, { closed: true });
      }

      case "debugger.attach": {
        if (typeof params.tabId !== "number") {
          return fail(id, "INVALID_PARAMS", "debugger.attach requires numeric tabId");
        }
        await DebuggerHub.attach(params.tabId);
        return ok(id, { tabId: params.tabId, attached: true });
      }

      case "debugger.detach": {
        if (typeof params.tabId !== "number") {
          return fail(id, "INVALID_PARAMS", "debugger.detach requires numeric tabId");
        }
        await DebuggerHub.detach(params.tabId);
        return ok(id, { tabId: params.tabId, detached: true });
      }

      case "cdp.send": {
        if (typeof params.tabId !== "number" || typeof params.method !== "string") {
          return fail(id, "INVALID_PARAMS", "cdp.send requires tabId and method");
        }
        const result = await DebuggerHub.sendCommand({
          tabId: params.tabId,
          method: params.method,
          params: params.params ?? {},
          sessionId: typeof params.sessionId === "string" ? params.sessionId : undefined,
        });
        return ok(id, result);
      }

      default:
        return fail(id, "METHOD_NOT_FOUND", `Unknown host method: ${method}`);
    }
  } catch (error) {
    const message = error instanceof Error ? error.message : String(error);
    return fail(id, "CDP_ERROR", message);
  }
}
