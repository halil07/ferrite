#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::sync::{Arc, Mutex};

use ferrite_core::{CdpError, CdpTransport};
use serde_json::{Value, json};

use crate::ax_tree::parse_full_ax_tree;
use crate::registry::{RefEntry, RefRegistry, Snapshot};

/// Coordinates Accessibility Tree snapshotting, deterministic `eN` tracking, self-healing
/// geometry lookup, and CDP input actions across tabs.
pub struct LocatorEngine {
    transport: Arc<dyn CdpTransport>,
    registries: Mutex<HashMap<i64, RefRegistry>>,
}

impl LocatorEngine {
    pub fn new(transport: Arc<dyn CdpTransport>) -> Self {
        Self {
            transport,
            registries: Mutex::new(HashMap::new()),
        }
    }

    /// Clears the cached Accessibility Tree ref registry for `tab_id` (e.g. after a cross-document
    /// navigation, reload, or tab close) so `@e1` numbering resets deterministically and memory stays bounded.
    pub fn reset_tab(&self, tab_id: i64) {
        self.registries
            .lock()
            .expect("locator mutex poisoned")
            .remove(&tab_id);
    }

    pub async fn snapshot(&self, tab_id: i64, interactive_only: bool) -> Result<Snapshot, CdpError> {
        self.transport
            .send_cdp(tab_id, "Accessibility.enable", json!({}), None)
            .await?;

        let raw = self
            .transport
            .send_cdp(tab_id, "Accessibility.getFullAXTree", json!({}), None)
            .await?;

        let tree = parse_full_ax_tree(&raw)?;
        let mut guard = self.registries.lock().expect("locator mutex poisoned");
        let registry = guard.entry(tab_id).or_default();
        Ok(registry.update_from_ax_tree("main", &tree, interactive_only))
    }

    pub fn get_by_role(
        &self,
        tab_id: i64,
        role: &str,
        name: Option<&str>,
        exact: bool,
    ) -> Vec<RefEntry> {
        let guard = self.registries.lock().expect("locator mutex poisoned");
        guard
            .get(&tab_id)
            .map(|reg| {
                reg.get_by_role(role, name, exact)
                    .into_iter()
                    .cloned()
                    .collect()
            })
            .unwrap_or_default()
    }

    pub fn get_by_text(&self, tab_id: i64, text: &str, exact: bool) -> Vec<RefEntry> {
        let guard = self.registries.lock().expect("locator mutex poisoned");
        guard
            .get(&tab_id)
            .map(|reg| reg.get_by_text(text, exact).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    pub fn get_by_label(&self, tab_id: i64, label: &str, exact: bool) -> Vec<RefEntry> {
        let guard = self.registries.lock().expect("locator mutex poisoned");
        guard
            .get(&tab_id)
            .map(|reg| reg.get_by_label(label, exact).into_iter().cloned().collect())
            .unwrap_or_default()
    }

    fn lookup_active(&self, tab_id: i64, ref_id: &str) -> Result<RefEntry, CdpError> {
        let guard = self.registries.lock().expect("locator mutex poisoned");
        let reg = guard
            .get(&tab_id)
            .ok_or_else(|| CdpError::InvalidArgument(format!("no snapshot taken yet for tab {tab_id}")))?;
        reg.resolve_active_ref(ref_id).cloned()
    }

    fn lookup_after_heal(
        &self,
        tab_id: i64,
        fingerprint: &crate::fingerprint::ElementFingerprint,
    ) -> Result<RefEntry, CdpError> {
        let guard = self.registries.lock().expect("locator mutex poisoned");
        let reg = guard.get(&tab_id).ok_or(CdpError::StaleNode)?;
        reg.find_by_fingerprint(fingerprint)
            .cloned()
            .ok_or(CdpError::StaleNode)
    }

    /// Resolves `(x, y)` viewport center of `ref_id`, transparently re-snapshotting and matching
    /// by fingerprint if Chrome reports the underlying DOM node became stale.
    pub async fn resolve_box_center(&self, tab_id: i64, ref_id: &str) -> Result<(f64, f64), CdpError> {
        let entry = self.lookup_active(tab_id, ref_id)?;
        if entry.states.disabled {
            return Err(CdpError::InvalidArgument(format!(
                "element {ref_id} ({} \"{}\") is disabled",
                entry.role, entry.name
            )));
        }

        let backend_id = entry
            .backend_dom_node_id
            .ok_or_else(|| CdpError::InvalidArgument(format!("element {ref_id} has no DOM backend node")))?;

        match self.fetch_box_center(tab_id, backend_id).await {
            Ok(coords) => Ok(coords),
            Err(CdpError::StaleNode) => {
                // Self-healing retry: capture fresh AX tree and locate by CSS-independent fingerprint.
                let _ = self.snapshot(tab_id, false).await?;
                let healed = self.lookup_after_heal(tab_id, &entry.fingerprint)?;
                let new_backend_id = healed.backend_dom_node_id.ok_or(CdpError::StaleNode)?;
                self.fetch_box_center(tab_id, new_backend_id).await
            }
            Err(other) => Err(other),
        }
    }

    async fn fetch_box_center(&self, tab_id: i64, backend_node_id: i64) -> Result<(f64, f64), CdpError> {
        let _ = self
            .transport
            .send_cdp(tab_id, "DOM.enable", json!({}), None)
            .await;

        self.transport
            .send_cdp(
                tab_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({ "backendNodeId": backend_node_id }),
                None,
            )
            .await?;

        match self
            .transport
            .send_cdp(
                tab_id,
                "DOM.getBoxModel",
                json!({ "backendNodeId": backend_node_id }),
                None,
            )
            .await
        {
            Ok(box_res) => parse_box_model_center(&box_res),
            Err(CdpError::Protocol { .. }) => {
                // Fallback for text/inline nodes where Chrome reports "Could not compute box model".
                let quads_res = self
                    .transport
                    .send_cdp(
                        tab_id,
                        "DOM.getContentQuads",
                        json!({ "backendNodeId": backend_node_id }),
                        None,
                    )
                    .await?;
                parse_content_quads_center(&quads_res)
            }
            Err(err) => Err(err),
        }
    }

    pub async fn click_ref(&self, tab_id: i64, ref_id: &str) -> Result<(f64, f64), CdpError> {
        let (x, y) = self.resolve_box_center(tab_id, ref_id).await?;
        self.dispatch_mouse_sequence(tab_id, x, y, &[("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)])
            .await?;
        Ok((x, y))
    }

    pub async fn dblclick_ref(&self, tab_id: i64, ref_id: &str) -> Result<(f64, f64), CdpError> {
        let (x, y) = self.resolve_box_center(tab_id, ref_id).await?;
        self.dispatch_mouse_sequence(
            tab_id,
            x,
            y,
            &[
                ("mouseMoved", 0),
                ("mousePressed", 1),
                ("mouseReleased", 1),
                ("mousePressed", 2),
                ("mouseReleased", 2),
            ],
        )
        .await?;
        Ok((x, y))
    }

    pub async fn hover_ref(&self, tab_id: i64, ref_id: &str) -> Result<(f64, f64), CdpError> {
        let (x, y) = self.resolve_box_center(tab_id, ref_id).await?;
        self.dispatch_mouse_sequence(tab_id, x, y, &[("mouseMoved", 0)])
            .await?;
        Ok((x, y))
    }

    async fn dispatch_mouse_sequence(
        &self,
        tab_id: i64,
        x: f64,
        y: f64,
        steps: &[(&str, i32)],
    ) -> Result<(), CdpError> {
        for &(event_type, click_count) in steps {
            let mut params = json!({
                "type": event_type,
                "x": x,
                "y": y,
            });
            if click_count > 0 {
                params["button"] = json!("left");
                params["clickCount"] = json!(click_count);
            }
            self.transport
                .send_cdp(tab_id, "Input.dispatchMouseEvent", params, None)
                .await?;
        }
        Ok(())
    }

    pub async fn press_key(&self, tab_id: i64, key: &str) -> Result<(), CdpError> {
        let (key_val, code_val, vk, text_val, modifiers, command) = resolve_key_spec(key)?;
        for event_type in ["keyDown", "keyUp"] {
            let mut params = json!({
                "type": event_type,
                "key": key_val,
                "code": code_val,
                "windowsVirtualKeyCode": vk,
            });
            if modifiers != 0 {
                params["modifiers"] = json!(modifiers);
            }
            if event_type == "keyDown" {
                if let Some(ref t) = text_val {
                    params["text"] = json!(t);
                    params["unmodifiedText"] = json!(t);
                }
                if let Some(cmd) = command {
                    params["commands"] = json!([cmd]);
                }
            }
            self.transport
                .send_cdp(tab_id, "Input.dispatchKeyEvent", params, None)
                .await?;
        }
        Ok(())
    }

    pub async fn type_text(&self, tab_id: i64, text: &str) -> Result<(), CdpError> {
        self.transport
            .send_cdp(tab_id, "Input.insertText", json!({ "text": text }), None)
            .await?;
        Ok(())
    }

    pub async fn fill_ref(&self, tab_id: i64, ref_id: &str, text: &str) -> Result<(), CdpError> {
        let entry = self.lookup_active(tab_id, ref_id)?;
        if entry.states.disabled {
            return Err(CdpError::InvalidArgument(format!(
                "element {ref_id} ({} \"{}\") is disabled",
                entry.role, entry.name
            )));
        }

        let backend_id = entry
            .backend_dom_node_id
            .ok_or_else(|| CdpError::InvalidArgument(format!("element {ref_id} has no DOM backend node")))?;

        let _ = self
            .transport
            .send_cdp(tab_id, "DOM.enable", json!({}), None)
            .await;

        match self
            .transport
            .send_cdp(tab_id, "DOM.focus", json!({ "backendNodeId": backend_id }), None)
            .await
        {
            Ok(_) => {}
            Err(CdpError::StaleNode) => {
                let _ = self.snapshot(tab_id, false).await?;
                let healed = self.lookup_after_heal(tab_id, &entry.fingerprint)?;
                let new_backend_id = healed.backend_dom_node_id.ok_or(CdpError::StaleNode)?;
                self.transport
                    .send_cdp(tab_id, "DOM.focus", json!({ "backendNodeId": new_backend_id }), None)
                    .await?;
            }
            Err(err) => return Err(err),
        }

        // Select existing text (Ctrl+A / modifier=2 + selectAll edit command) and clear before inserting text.
        for key_type in ["keyDown", "keyUp"] {
            let mut key_params = json!({
                "type": key_type,
                "key": "a",
                "code": "KeyA",
                "modifiers": 2,
                "windowsVirtualKeyCode": 65
            });
            if key_type == "keyDown" {
                key_params["commands"] = json!(["selectAll"]);
            }
            self.transport
                .send_cdp(tab_id, "Input.dispatchKeyEvent", key_params, None)
                .await?;
        }
        for key_type in ["keyDown", "keyUp"] {
            self.transport
                .send_cdp(
                    tab_id,
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": key_type,
                        "key": "Backspace",
                        "code": "Backspace",
                        "windowsVirtualKeyCode": 8
                    }),
                    None,
                )
                .await?;
        }

        self.transport
            .send_cdp(tab_id, "Input.insertText", json!({ "text": text }), None)
            .await?;

        Ok(())
    }

    /// Queries all DOM nodes matching a CSS selector (`DOM.querySelectorAll`, equivalent to `document.querySelectorAll`).
    pub async fn query_selector_all(
        &self,
        tab_id: i64,
        selector: &str,
    ) -> Result<Vec<i64>, CdpError> {
        let _ = self
            .transport
            .send_cdp(tab_id, "DOM.enable", json!({}), None)
            .await;

        let doc = self
            .transport
            .send_cdp(tab_id, "DOM.getDocument", json!({ "depth": 0 }), None)
            .await?;
        let root_node_id = doc
            .get("root")
            .and_then(|r| r.get("nodeId"))
            .and_then(Value::as_i64)
            .ok_or_else(|| CdpError::Protocol {
                code: None,
                message: "DOM.getDocument omitted root.nodeId".to_string(),
            })?;

        let res = self
            .transport
            .send_cdp(
                tab_id,
                "DOM.querySelectorAll",
                json!({ "nodeId": root_node_id, "selector": selector }),
                None,
            )
            .await?;

        let ids = res
            .get("nodeIds")
            .and_then(Value::as_array)
            .map(|arr| arr.iter().filter_map(Value::as_i64).collect())
            .unwrap_or_default();
        Ok(ids)
    }

    async fn fetch_node_box_center(&self, tab_id: i64, node_id: i64) -> Result<(f64, f64), CdpError> {
        self.transport
            .send_cdp(
                tab_id,
                "DOM.scrollIntoViewIfNeeded",
                json!({ "nodeId": node_id }),
                None,
            )
            .await?;

        match self
            .transport
            .send_cdp(tab_id, "DOM.getBoxModel", json!({ "nodeId": node_id }), None)
            .await
        {
            Ok(box_res) => parse_box_model_center(&box_res),
            Err(CdpError::Protocol { .. }) => {
                let quads_res = self
                    .transport
                    .send_cdp(
                        tab_id,
                        "DOM.getContentQuads",
                        json!({ "nodeId": node_id }),
                        None,
                    )
                    .await?;
                parse_content_quads_center(&quads_res)
            }
            Err(err) => Err(err),
        }
    }

    async fn resolve_css_node(&self, tab_id: i64, selector: &str, nth: usize) -> Result<i64, CdpError> {
        let ids = self.query_selector_all(tab_id, selector).await?;
        ids.into_iter().nth(nth).ok_or_else(|| {
            CdpError::InvalidArgument(format!(
                "no DOM element matched CSS selector {selector:?} at index {nth}"
            ))
        })
    }

    pub async fn is_css_visible(
        &self,
        tab_id: i64,
        selector: &str,
        nth: usize,
    ) -> Result<bool, CdpError> {
        let ids = self.query_selector_all(tab_id, selector).await?;
        let Some(node_id) = ids.into_iter().nth(nth) else {
            return Ok(false);
        };
        match self.fetch_node_box_center(tab_id, node_id).await {
            Ok(_) => Ok(true),
            Err(CdpError::InvalidArgument(_) | CdpError::Protocol { .. } | CdpError::StaleNode) => {
                Ok(false)
            }
            Err(err) => Err(err),
        }
    }

    pub async fn click_css(
        &self,
        tab_id: i64,
        selector: &str,
        nth: usize,
    ) -> Result<(f64, f64), CdpError> {
        let node_id = self.resolve_css_node(tab_id, selector, nth).await?;
        let (x, y) = self.fetch_node_box_center(tab_id, node_id).await?;
        self.dispatch_mouse_sequence(
            tab_id,
            x,
            y,
            &[("mouseMoved", 0), ("mousePressed", 1), ("mouseReleased", 1)],
        )
        .await?;
        Ok((x, y))
    }

    pub async fn dblclick_css(
        &self,
        tab_id: i64,
        selector: &str,
        nth: usize,
    ) -> Result<(f64, f64), CdpError> {
        let node_id = self.resolve_css_node(tab_id, selector, nth).await?;
        let (x, y) = self.fetch_node_box_center(tab_id, node_id).await?;
        self.dispatch_mouse_sequence(
            tab_id,
            x,
            y,
            &[
                ("mouseMoved", 0),
                ("mousePressed", 1),
                ("mouseReleased", 1),
                ("mousePressed", 2),
                ("mouseReleased", 2),
            ],
        )
        .await?;
        Ok((x, y))
    }

    pub async fn hover_css(
        &self,
        tab_id: i64,
        selector: &str,
        nth: usize,
    ) -> Result<(f64, f64), CdpError> {
        let node_id = self.resolve_css_node(tab_id, selector, nth).await?;
        let (x, y) = self.fetch_node_box_center(tab_id, node_id).await?;
        self.dispatch_mouse_sequence(tab_id, x, y, &[("mouseMoved", 0)])
            .await?;
        Ok((x, y))
    }

    pub async fn fill_css(
        &self,
        tab_id: i64,
        selector: &str,
        text: &str,
        nth: usize,
    ) -> Result<(), CdpError> {
        let node_id = self.resolve_css_node(tab_id, selector, nth).await?;
        self.transport
            .send_cdp(tab_id, "DOM.focus", json!({ "nodeId": node_id }), None)
            .await?;

        for key_type in ["keyDown", "keyUp"] {
            let mut key_params = json!({
                "type": key_type,
                "key": "a",
                "code": "KeyA",
                "modifiers": 2,
                "windowsVirtualKeyCode": 65
            });
            if key_type == "keyDown" {
                key_params["commands"] = json!(["selectAll"]);
            }
            self.transport
                .send_cdp(tab_id, "Input.dispatchKeyEvent", key_params, None)
                .await?;
        }
        for key_type in ["keyDown", "keyUp"] {
            self.transport
                .send_cdp(
                    tab_id,
                    "Input.dispatchKeyEvent",
                    json!({
                        "type": key_type,
                        "key": "Backspace",
                        "code": "Backspace",
                        "windowsVirtualKeyCode": 8
                    }),
                    None,
                )
                .await?;
        }

        self.transport
            .send_cdp(tab_id, "Input.insertText", json!({ "text": text }), None)
            .await?;
        Ok(())
    }
}

fn resolve_key_spec(
    key: &str,
) -> Result<(String, String, u32, Option<String>, u32, Option<&'static str>), CdpError> {
    match key {
        "Enter" => Ok(("Enter".into(), "Enter".into(), 13, Some("\r".into()), 0, None)),
        "Escape" => Ok(("Escape".into(), "Escape".into(), 27, None, 0, None)),
        "Tab" => Ok(("Tab".into(), "Tab".into(), 9, Some("\t".into()), 0, None)),
        "Backspace" => Ok(("Backspace".into(), "Backspace".into(), 8, None, 0, None)),
        "Delete" => Ok(("Delete".into(), "Delete".into(), 46, None, 0, None)),
        "Space" | " " => Ok((" ".into(), "Space".into(), 32, Some(" ".into()), 0, None)),
        "ArrowUp" => Ok(("ArrowUp".into(), "ArrowUp".into(), 38, None, 0, None)),
        "ArrowDown" => Ok(("ArrowDown".into(), "ArrowDown".into(), 40, None, 0, None)),
        "ArrowLeft" => Ok(("ArrowLeft".into(), "ArrowLeft".into(), 37, None, 0, None)),
        "ArrowRight" => Ok(("ArrowRight".into(), "ArrowRight".into(), 39, None, 0, None)),
        "Control+a" | "Ctrl+a" | "Control+A" | "Ctrl+A" => Ok((
            "a".into(),
            "KeyA".into(),
            65,
            None,
            2,
            Some("selectAll"),
        )),
        single if single.chars().count() == 1 => {
            let ch = single.chars().next().expect("single char");
            let code = if ch.is_ascii_alphabetic() {
                format!("Key{}", ch.to_ascii_uppercase())
            } else if ch.is_ascii_digit() {
                format!("Digit{ch}")
            } else {
                single.to_string()
            };
            Ok((
                single.to_string(),
                code,
                ch.to_ascii_uppercase() as u32,
                Some(single.to_string()),
                0,
                None,
            ))
        }
        other => Err(CdpError::InvalidArgument(format!("unsupported key name {other:?}"))),
    }
}

fn parse_box_model_center(box_res: &Value) -> Result<(f64, f64), CdpError> {
    let quad = box_res
        .get("model")
        .and_then(|m| m.get("content"))
        .and_then(Value::as_array)
        .ok_or_else(|| CdpError::Protocol {
            code: None,
            message: "DOM.getBoxModel omitted model.content quad".to_string(),
        })?;
    compute_quad_center(quad)
}

fn parse_content_quads_center(quads_res: &Value) -> Result<(f64, f64), CdpError> {
    let quad = quads_res
        .get("quads")
        .and_then(Value::as_array)
        .and_then(|arr| arr.first())
        .and_then(Value::as_array)
        .ok_or_else(|| CdpError::Protocol {
            code: None,
            message: "DOM.getContentQuads omitted non-empty quads[0]".to_string(),
        })?;
    compute_quad_center(quad)
}

fn compute_quad_center(quad: &[Value]) -> Result<(f64, f64), CdpError> {
    if quad.len() < 8 {
        return Err(CdpError::Protocol {
            code: None,
            message: "content quad has fewer than 8 coordinates".to_string(),
        });
    }

    let mut xs = [0.0f64; 4];
    let mut ys = [0.0f64; 4];
    for i in 0..4 {
        xs[i] = quad[i * 2].as_f64().unwrap_or(0.0);
        ys[i] = quad[i * 2 + 1].as_f64().unwrap_or(0.0);
    }

    let min_x = xs.into_iter().fold(f64::INFINITY, f64::min);
    let max_x = xs.into_iter().fold(f64::NEG_INFINITY, f64::max);
    let min_y = ys.into_iter().fold(f64::INFINITY, f64::min);
    let max_y = ys.into_iter().fold(f64::NEG_INFINITY, f64::max);

    if (max_x - min_x) <= 0.0 || (max_y - min_y) <= 0.0 {
        return Err(CdpError::InvalidArgument(
            "target element has zero layout area and is not clickable".to_string(),
        ));
    }

    Ok(((min_x + max_x) / 2.0, (min_y + max_y) / 2.0))
}

#[cfg(test)]
mod tests {
    use ferrite_cdp::MockCdpTransport;

    use super::*;

    #[tokio::test]
    async fn click_ref_self_heals_after_stale_dom_node() {
        let mock = MockCdpTransport::new();
        let engine = LocatorEngine::new(mock.clone());

        // First snapshot returns backendDOMNodeId 55.
        mock.push_ok(
            "Accessibility.getFullAXTree",
            json!({
                "nodes": [
                    {
                        "nodeId": "1",
                        "role": { "type": "role", "value": "RootWebArea" },
                        "childIds": ["2"]
                    },
                    {
                        "nodeId": "2",
                        "parentId": "1",
                        "role": { "type": "role", "value": "button" },
                        "name": { "type": "computedString", "value": "Pay Now" },
                        "backendDOMNodeId": 55,
                        "childIds": []
                    }
                ]
            }),
        );

        let snap = engine.snapshot(1, true).await.unwrap();
        assert_eq!(snap.items[0].entry.ref_id, "e1");

        // Next DOM.scrollIntoViewIfNeeded fails as StaleNode on 55, triggering fresh snapshot -> 77!
        mock.push_err("DOM.scrollIntoViewIfNeeded", CdpError::StaleNode);
        mock.push_ok(
            "Accessibility.getFullAXTree",
            json!({
                "nodes": [
                    {
                        "nodeId": "10",
                        "role": { "type": "role", "value": "RootWebArea" },
                        "childIds": ["20"]
                    },
                    {
                        "nodeId": "20",
                        "parentId": "10",
                        "role": { "type": "role", "value": "button" },
                        "name": { "type": "computedString", "value": "Pay Now" },
                        "backendDOMNodeId": 77,
                        "childIds": []
                    }
                ]
            }),
        );
        mock.push_ok("DOM.scrollIntoViewIfNeeded", json!({}));
        mock.push_ok(
            "DOM.getBoxModel",
            json!({ "model": { "content": [100.0, 200.0, 220.0, 200.0, 220.0, 240.0, 100.0, 240.0] } }),
        );

        let (cx, cy) = engine.click_ref(1, "e1").await.unwrap();
        assert_eq!((cx, cy), (160.0, 220.0));

        let calls = mock.recorded_calls();
        assert!(calls.iter().any(|c| {
            c.method == "DOM.getBoxModel" && c.params["backendNodeId"] == 77
        }));
        assert!(calls.iter().any(|c| {
            c.method == "Input.dispatchMouseEvent"
                && c.params["type"] == "mousePressed"
                && c.params["x"] == 160.0
                && c.params["y"] == 220.0
        }));
    }

    #[tokio::test]
    async fn supports_dblclick_hover_press_key_and_content_quads_fallback() {
        let mock = MockCdpTransport::new();
        let engine = LocatorEngine::new(mock.clone());

        mock.push_ok(
            "Accessibility.getFullAXTree",
            json!({
                "nodes": [
                    {
                        "nodeId": "1",
                        "role": { "type": "role", "value": "RootWebArea" },
                        "childIds": ["2"]
                    },
                    {
                        "nodeId": "2",
                        "parentId": "1",
                        "role": { "type": "role", "value": "StaticText" },
                        "name": { "type": "computedString", "value": "Buy milk" },
                        "backendDOMNodeId": 303,
                        "childIds": []
                    }
                ]
            }),
        );
        let _ = engine.snapshot(1, false).await.unwrap();

        mock.push_err(
            "DOM.getBoxModel",
            CdpError::Protocol {
                code: Some(-32000),
                message: "Could not compute box model.".to_string(),
            },
        );
        mock.push_ok(
            "DOM.getContentQuads",
            json!({ "quads": [[10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0]] }),
        );
        let (hx, hy) = engine.hover_ref(1, "e1").await.unwrap();
        assert_eq!((hx, hy), (60.0, 40.0));

        mock.push_ok(
            "DOM.getBoxModel",
            json!({ "model": { "content": [10.0, 20.0, 110.0, 20.0, 110.0, 60.0, 10.0, 60.0] } }),
        );
        let (dx, dy) = engine.dblclick_ref(1, "e1").await.unwrap();
        assert_eq!((dx, dy), (60.0, 40.0));

        engine.press_key(1, "Enter").await.unwrap();
        engine.type_text(1, "Updated").await.unwrap();

        let calls = mock.recorded_calls();
        assert!(calls.iter().any(|c| {
            c.method == "Input.dispatchMouseEvent"
                && c.params["type"] == "mousePressed"
                && c.params["clickCount"] == 2
        }));
        assert!(calls.iter().any(|c| {
            c.method == "Input.dispatchKeyEvent"
                && c.params["type"] == "keyDown"
                && c.params["key"] == "Enter"
                && c.params["text"] == "\r"
        }));
        assert!(calls.iter().any(|c| {
            c.method == "Input.insertText" && c.params["text"] == "Updated"
        }));
    }

    #[tokio::test]
    async fn supports_css_query_selector_all_click_and_fill() {
        let mock = MockCdpTransport::new();
        let engine = LocatorEngine::new(mock.clone());

        mock.add_handler(|_tab_id, method, _params, _session| match method {
            "DOM.getDocument" => Some(Ok(json!({ "root": { "nodeId": 1 } }))),
            "DOM.querySelectorAll" => Some(Ok(json!({ "nodeIds": [41, 42] }))),
            "DOM.getBoxModel" => Some(Ok(json!({
                "model": { "content": [0.0, 0.0, 80.0, 0.0, 80.0, 30.0, 0.0, 30.0] }
            }))),
            _ => None,
        });

        let (x, y) = engine.click_css(1, ".todo-list li .toggle", 1).await.unwrap();
        assert_eq!((x, y), (40.0, 15.0));

        engine.fill_css(1, "#new-todo", "Buy bread", 0).await.unwrap();

        let calls = mock.recorded_calls();
        assert!(calls.iter().any(|c| {
            c.method == "DOM.querySelectorAll" && c.params["selector"] == ".todo-list li .toggle"
        }));
        assert!(calls.iter().any(|c| {
            c.method == "DOM.getBoxModel" && c.params["nodeId"] == 42
        }));
        assert!(calls.iter().any(|c| {
            c.method == "DOM.focus" && c.params["nodeId"] == 41
        }));
        assert!(calls.iter().any(|c| {
            c.method == "Input.insertText" && c.params["text"] == "Buy bread"
        }));
    }
}
