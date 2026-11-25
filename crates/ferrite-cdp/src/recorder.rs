#![forbid(unsafe_code)]

use std::sync::{Arc, Mutex};

use ferrite_core::{CdpError, CdpEvent, CdpTransport};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use tokio::task::JoinHandle;

pub const RECORDER_BINDING_NAME: &str = "__ferriteRecordEvent";

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum RecordedAction {
    Navigate {
        url: String,
    },
    ClickRole {
        role: String,
        #[serde(default)]
        name: Option<String>,
    },
    FillRole {
        role: String,
        #[serde(default)]
        name: Option<String>,
        value: String,
    },
    DblclickText {
        text: String,
    },
    PressKey {
        key: String,
    },
}

/// Builds the browser-side DOM listener script that streams user clicks, debounced inputs
/// (masking password fields), double-clicks, and special key presses over `Runtime.addBinding`.
pub fn build_capture_script(mask_passwords: bool) -> String {
    let mask_flag = if mask_passwords { "true" } else { "false" };
    format!(
        r#"(() => {{
  if (globalThis.__ferriteRecorderInstalled) {{
    globalThis.__ferriteRecorderEnabled = true;
    return;
  }}
  globalThis.__ferriteRecorderInstalled = true;
  globalThis.__ferriteRecorderEnabled = true;

  function emit(payload) {{
    if (!globalThis.__ferriteRecorderEnabled) return;
    if (typeof globalThis.{RECORDER_BINDING_NAME} === 'function') {{
      try {{ globalThis.{RECORDER_BINDING_NAME}(JSON.stringify(payload)); }} catch {{}}
    }}
  }}

  function inferRole(el) {{
    const explicit = el.getAttribute('role');
    if (explicit) return explicit;
    const tag = el.tagName.toLowerCase();
    if (tag === 'button') return 'button';
    if (tag === 'a' && el.hasAttribute('href')) return 'link';
    if (tag === 'textarea') return 'textbox';
    if (tag === 'select') return 'combobox';
    if (tag === 'input') {{
      const t = (el.getAttribute('type') || 'text').toLowerCase();
      if (t === 'checkbox') return 'checkbox';
      if (t === 'radio') return 'radio';
      if (t === 'submit' || t === 'button') return 'button';
      return 'textbox';
    }}
    return 'generic';
  }}

  function accessibleName(el) {{
    const aria = el.getAttribute('aria-label');
    if (aria && aria.trim()) return aria.trim();
    if (el.id) {{
      const lbl = document.querySelector('label[for="' + CSS.escape(el.id) + '"]');
      if (lbl && lbl.textContent && lbl.textContent.trim()) return lbl.textContent.trim();
    }}
    const ph = el.getAttribute('placeholder');
    if (ph && ph.trim()) return ph.trim();
    const txt = (el.innerText || el.textContent || '').replace(/\s+/g, ' ').trim();
    return txt ? txt.slice(0, 80) : null;
  }}

  function interactiveTarget(raw) {{
    if (!(raw instanceof Element)) return null;
    return raw.closest('button, a, input, textarea, select, [role="button"], [role="link"], [role="checkbox"], [role="tab"]') || raw;
  }}

  document.addEventListener('click', (e) => {{
    const el = interactiveTarget(e.target);
    if (!el) return;
    const role = inferRole(el);
    if (role === 'generic') return;
    emit({{ action: 'click_role', role, name: accessibleName(el) }});
  }}, true);

  document.addEventListener('dblclick', (e) => {{
    if (!(e.target instanceof Element)) return;
    const text = (e.target.innerText || e.target.textContent || '').replace(/\s+/g, ' ').trim();
    if (text && text.length <= 120) {{
      emit({{ action: 'dblclick_text', text }});
    }}
  }}, true);

  const timers = new WeakMap();
  function recordInput(el) {{
    const role = inferRole(el);
    if (role === 'checkbox' || role === 'radio') return;
    const isPassword = {mask_flag} && el.tagName.toLowerCase() === 'input' && el.type === 'password';
    const value = isPassword ? '<REDACTED_PASSWORD>' : (el.value || '');
    emit({{ action: 'fill_role', role, name: accessibleName(el), value }});
  }}

  document.addEventListener('input', (e) => {{
    const el = e.target;
    if (!(el instanceof Element)) return;
    const prev = timers.get(el);
    if (prev) clearTimeout(prev);
    timers.set(el, setTimeout(() => recordInput(el), 300));
  }}, true);

  document.addEventListener('change', (e) => {{
    const el = e.target;
    if (!(el instanceof Element)) return;
    const prev = timers.get(el);
    if (prev) clearTimeout(prev);
    recordInput(el);
  }}, true);

  document.addEventListener('keydown', (e) => {{
    if (['Enter', 'Escape', 'Tab'].includes(e.key)) {{
      emit({{ action: 'press_key', key: e.key }});
    }}
  }}, true);
}})()"#
    )
}

/// Compresses noisy raw browser actions: merges consecutive fills on the same field, removes
/// redundant click-before-fill on the exact same textbox, and deduplicates consecutive navigations.
pub fn compress_recorded_actions(raw: &[RecordedAction]) -> Vec<RecordedAction> {
    let mut out: Vec<RecordedAction> = Vec::with_capacity(raw.len());
    for action in raw {
        if let Some(prev) = out.last_mut() {
            match (prev, action) {
                (
                    RecordedAction::FillRole {
                        role: r1,
                        name: n1,
                        value: v1,
                    },
                    RecordedAction::FillRole {
                        role: r2,
                        name: n2,
                        value: v2,
                    },
                ) if r1 == r2 && n1 == n2 => {
                    *v1 = v2.clone();
                    continue;
                }
                (RecordedAction::Navigate { url: u1 }, RecordedAction::Navigate { url: u2 })
                    if u1 == u2 =>
                {
                    continue;
                }
                _ => {}
            }
        }

        if let RecordedAction::FillRole { role, name, .. } = action
            && matches!(
                out.last(),
                Some(RecordedAction::ClickRole { role: r, name: n }) if r == role && n == name
            )
        {
            out.pop();
        }

        out.push(action.clone());
    }
    out
}

/// Renders recorded actions as clean, idiomatic `.ferrite` DSL source code.
pub fn render_ferrite_script(
    suite_name: &str,
    case_name: &str,
    actions: &[RecordedAction],
) -> String {
    let mut lines = vec![
        "// Recorded by Ferrite E2E Action Recorder".to_string(),
        format!("suite {}", quote(suite_name)),
        String::new(),
        format!("test {} {{", quote(case_name)),
    ];

    for action in actions {
        match action {
            RecordedAction::Navigate { url } => {
                lines.push(format!("  goto {} wait load timeout 15s", quote(url)));
            }
            RecordedAction::ClickRole { role, name } => match name.as_deref().filter(|s| !s.is_empty()) {
                Some(n) => lines.push(format!(
                    "  click role {} {} exact timeout 5s",
                    quote(role),
                    quote(n)
                )),
                None => lines.push(format!("  click role {} timeout 5s", quote(role))),
            },
            RecordedAction::FillRole { role, name, value } => {
                match name.as_deref().filter(|s| !s.is_empty()) {
                    Some(n) => lines.push(format!(
                        "  fill role {} {} {} timeout 5s",
                        quote(role),
                        quote(n),
                        quote(value)
                    )),
                    None => lines.push(format!(
                        "  fill role {} {} timeout 5s",
                        quote(role),
                        quote(value)
                    )),
                }
            }
            RecordedAction::DblclickText { text } => {
                lines.push(format!("  dblclick text {} timeout 5s", quote(text)));
            }
            RecordedAction::PressKey { key } => {
                lines.push(format!("  press {}", quote(key)));
            }
        }
    }

    lines.push("}".to_string());
    lines.push(String::new());
    lines.join("\n")
}

fn quote(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '$' => out.push_str("\\$"),
            other => out.push(other),
        }
    }
    out.push('"');
    out
}

/// Live CDP Action Recorder bound to a Chrome tab.
pub struct ActionRecorder {
    transport: Arc<dyn CdpTransport>,
    tab_id: i64,
    actions: Arc<Mutex<Vec<RecordedAction>>>,
    listener: Mutex<Option<JoinHandle<()>>>,
    script_identifier: Mutex<Option<String>>,
}

impl Drop for ActionRecorder {
    fn drop(&mut self) {
        if let Ok(mut slot) = self.listener.lock()
            && let Some(handle) = slot.take()
        {
            handle.abort();
        }
    }
}

impl ActionRecorder {
    pub fn new(transport: Arc<dyn CdpTransport>, tab_id: i64) -> Arc<Self> {
        Arc::new(Self {
            transport,
            tab_id,
            actions: Arc::new(Mutex::new(Vec::new())),
            listener: Mutex::new(None),
            script_identifier: Mutex::new(None),
        })
    }

    pub async fn start(self: &Arc<Self>, mask_passwords: bool) -> Result<(), CdpError> {
        self.actions
            .lock()
            .expect("recorder mutex poisoned")
            .clear();

        self.transport
            .send_cdp(self.tab_id, "Runtime.enable", json!({}), None)
            .await?;
        self.transport
            .send_cdp(self.tab_id, "Page.enable", json!({}), None)
            .await?;
        self.transport
            .send_cdp(
                self.tab_id,
                "Runtime.addBinding",
                json!({ "name": RECORDER_BINDING_NAME }),
                None,
            )
            .await?;

        let script = build_capture_script(mask_passwords);
        if let Ok(res) = self
            .transport
            .send_cdp(
                self.tab_id,
                "Page.addScriptToEvaluateOnNewDocument",
                json!({ "source": script }),
                None,
            )
            .await
            && let Some(id) = res.get("identifier").and_then(Value::as_str)
        {
            *self
                .script_identifier
                .lock()
                .expect("recorder script_identifier mutex poisoned") = Some(id.to_string());
        }
        let _ = self
            .transport
            .send_cdp(
                self.tab_id,
                "Runtime.evaluate",
                json!({ "expression": script }),
                None,
            )
            .await;

        let mut rx = self.transport.subscribe_events();
        let tab_id = self.tab_id;
        let actions = self.actions.clone();
        let handle = tokio::spawn(async move {
            loop {
                match rx.recv().await {
                    Ok(ev) if ev.tab_id == tab_id => ingest_recorder_event(&ev, &actions),
                    Ok(_) => {}
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
                }
            }
        });

        let mut slot = self.listener.lock().expect("recorder listener mutex poisoned");
        if let Some(old) = slot.replace(handle) {
            old.abort();
        }
        Ok(())
    }

    pub async fn stop(
        &self,
        suite_name: &str,
        case_name: &str,
    ) -> Result<(Vec<RecordedAction>, String), CdpError> {
        if let Some(handle) = self
            .listener
            .lock()
            .expect("recorder listener mutex poisoned")
            .take()
        {
            handle.abort();
        }
        let script_id = self
            .script_identifier
            .lock()
            .expect("recorder script_identifier mutex poisoned")
            .take();
        if let Some(script_id) = script_id {
            let _ = self
                .transport
                .send_cdp(
                    self.tab_id,
                    "Page.removeScriptToEvaluateOnNewDocument",
                    json!({ "identifier": script_id }),
                    None,
                )
                .await;
        }
        let _ = self
            .transport
            .send_cdp(
                self.tab_id,
                "Runtime.evaluate",
                json!({ "expression": "globalThis.__ferriteRecorderEnabled = false;" }),
                None,
            )
            .await;

        let raw = self
            .actions
            .lock()
            .expect("recorder mutex poisoned")
            .clone();
        let compressed = compress_recorded_actions(&raw);
        let script = render_ferrite_script(suite_name, case_name, &compressed);
        Ok((compressed, script))
    }
}

fn ingest_recorder_event(ev: &CdpEvent, actions: &Arc<Mutex<Vec<RecordedAction>>>) {
    match ev.method.as_str() {
        "Runtime.bindingCalled" => {
            if ev.params.get("name").and_then(Value::as_str) != Some(RECORDER_BINDING_NAME) {
                return;
            }
            let Some(payload) = ev.params.get("payload").and_then(Value::as_str) else {
                return;
            };
            if let Ok(action) = serde_json::from_str::<RecordedAction>(payload) {
                actions
                    .lock()
                    .expect("recorder mutex poisoned")
                    .push(action);
            }
        }
        "Page.frameNavigated" => {
            let frame = &ev.params["frame"];
            if frame.get("parentId").is_none()
                && let Some(url) = frame.get("url").and_then(Value::as_str)
                && !url.is_empty()
                && url != "about:blank"
            {
                actions
                    .lock()
                    .expect("recorder mutex poisoned")
                    .push(RecordedAction::Navigate {
                        url: url.to_string(),
                    });
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn compresses_click_before_fill_and_renders_ferrite_source() {
        let raw = vec![
            RecordedAction::Navigate {
                url: "https://todomvc.com/examples/react/dist/".into(),
            },
            RecordedAction::Navigate {
                url: "https://todomvc.com/examples/react/dist/".into(),
            },
            RecordedAction::ClickRole {
                role: "textbox".into(),
                name: Some("New Todo Input".into()),
            },
            RecordedAction::FillRole {
                role: "textbox".into(),
                name: Some("New Todo Input".into()),
                value: "Buy".into(),
            },
            RecordedAction::FillRole {
                role: "textbox".into(),
                name: Some("New Todo Input".into()),
                value: "Buy $5 milk".into(),
            },
            RecordedAction::PressKey {
                key: "Enter".into(),
            },
        ];

        let compressed = compress_recorded_actions(&raw);
        assert_eq!(compressed.len(), 3);
        let dsl = render_ferrite_script("recorded-suite", "recorded-flow", &compressed);
        assert!(dsl.contains("suite \"recorded-suite\""));
        assert!(dsl.contains("fill role \"textbox\" \"New Todo Input\" \"Buy \\$5 milk\""));
        assert!(dsl.contains("press \"Enter\""));
    }
}
