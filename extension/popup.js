const hostStatusEl = document.getElementById("host-status");
const attachedCountEl = document.getElementById("attached-count");
const activeTabEl = document.getElementById("active-tab-status");
const errorBoxEl = document.getElementById("error-box");
const toggleBtn = document.getElementById("toggle-attach");
const reconnectBtn = document.getElementById("reconnect");

let activeTabAttached = false;

async function refresh() {
  const res = await chrome.runtime.sendMessage({ action: "get-status" });
  const connected = Boolean(res?.bridge?.connected);
  hostStatusEl.textContent = connected ? "Connected" : "Disconnected";
  hostStatusEl.className = connected ? "badge-ok" : "badge-err";

  attachedCountEl.textContent = String(res?.attachedTabs?.length ?? 0);
  activeTabAttached = Boolean(res?.activeTab?.attached);
  activeTabEl.textContent = res?.activeTab
    ? `#${res.activeTab.id} (${activeTabAttached ? "Attached" : "Detached"})`
    : "None";

  toggleBtn.textContent = activeTabAttached ? "Detach Tab" : "Attach Tab";
  errorBoxEl.textContent = res?.bridge?.lastError || "";
}

toggleBtn.addEventListener("click", async () => {
  const action = activeTabAttached ? "detach-active-tab" : "attach-active-tab";
  const res = await chrome.runtime.sendMessage({ action });
  if (!res?.ok && res?.error) {
    errorBoxEl.textContent = res.error;
  }
  await refresh();
});

reconnectBtn.addEventListener("click", async () => {
  await chrome.runtime.sendMessage({ action: "reconnect-native" });
  await refresh();
});

void refresh();
