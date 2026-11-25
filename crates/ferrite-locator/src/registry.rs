#![forbid(unsafe_code)]

use std::collections::HashMap;
use std::fmt::Write as _;

use ferrite_core::CdpError;
use serde::{Deserialize, Serialize};

use crate::ax_tree::{AxNode, AxStates, is_interactive_role, is_landmark_role, is_semantic_role};
use crate::fingerprint::{ElementFingerprint, compute_fingerprint, normalize_name};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct RefEntry {
    pub ref_id: String,
    pub fingerprint: ElementFingerprint,
    pub frame_id: String,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub states: AxStates,
    pub backend_dom_node_id: Option<i64>,
    pub nth: usize,
    pub semantic_path: String,
    pub stale: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SnapshotItem {
    pub depth: usize,
    pub entry: RefEntry,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct Snapshot {
    pub frame_id: String,
    pub items: Vec<SnapshotItem>,
}

impl Snapshot {
    pub fn render_compact(&self) -> String {
        let mut out = String::new();
        for item in &self.items {
            let indent = "  ".repeat(item.depth.min(8));
            let e = &item.entry;
            let _ = write!(out, "{indent}- [{}] {}", e.ref_id, e.role);
            if !e.name.is_empty() {
                let _ = write!(out, " \"{}\"", e.name);
            }
            if let Some(ref val) = e.value
                && !val.is_empty()
            {
                let _ = write!(out, " value=\"{val}\"");
            }
            if e.states.disabled {
                out.push_str(" [disabled]");
            }
            if let Some(checked) = e.states.checked {
                let _ = write!(out, " [checked={checked}]");
            }
            if let Some(expanded) = e.states.expanded {
                let _ = write!(out, " [expanded={expanded}]");
            }
            if e.states.focused {
                out.push_str(" [focused]");
            }
            out.push('\n');
        }
        out
    }
}

/// Maintains persistent deterministic element references (`e1`, `e2`, ...) for a single tab
/// across DOM updates and CSS mutations.
#[derive(Debug)]
pub struct RefRegistry {
    next_id: u64,
    entries: HashMap<String, RefEntry>,
    fingerprint_to_ref: HashMap<ElementFingerprint, String>,
    insertion_order: Vec<String>,
}

impl Default for RefRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl RefRegistry {
    pub fn new() -> Self {
        Self {
            next_id: 1,
            entries: HashMap::new(),
            fingerprint_to_ref: HashMap::new(),
            insertion_order: Vec::new(),
        }
    }

    pub fn update_from_ax_tree(
        &mut self,
        frame_id: &str,
        roots: &[AxNode],
        interactive_only: bool,
    ) -> Snapshot {
        // Mark all existing entries for this frame as stale until observed in the new tree.
        for entry in self.entries.values_mut() {
            if entry.frame_id == frame_id {
                entry.stale = true;
            }
        }

        let mut occurrence_counts: HashMap<(String, String, String), usize> = HashMap::new();
        let mut items = Vec::new();
        let mut path = Vec::new();

        for root in roots {
            self.walk_node(
                frame_id,
                root,
                0,
                interactive_only,
                &mut path,
                &mut occurrence_counts,
                &mut items,
            );
        }

        Snapshot {
            frame_id: frame_id.to_string(),
            items,
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn walk_node(
        &mut self,
        frame_id: &str,
        node: &AxNode,
        depth: usize,
        interactive_only: bool,
        path: &mut Vec<String>,
        occurrence_counts: &mut HashMap<(String, String, String), usize>,
        items: &mut Vec<SnapshotItem>,
    ) {
        let norm_name = normalize_name(&node.name);
        let include = if interactive_only {
            is_interactive_role(&node.role)
        } else {
            is_semantic_role(&node.role)
                && !(node.role == "StaticText" && norm_name.is_empty())
                && node.role != "RootWebArea"
        };

        let mut next_depth = depth;
        if include {
            let semantic_path_str = path.join("/");
            let key = (
                semantic_path_str.clone(),
                node.role.to_ascii_lowercase(),
                norm_name.to_ascii_lowercase(),
            );
            let nth = {
                let count = occurrence_counts.entry(key).or_insert(0);
                let current = *count;
                *count += 1;
                current
            };

            let fingerprint =
                compute_fingerprint(frame_id, path, &node.role, &norm_name, nth);

            let ref_id = if let Some(existing) = self.fingerprint_to_ref.get(&fingerprint) {
                existing.clone()
            } else {
                let new_ref = format!("e{}", self.next_id);
                self.next_id += 1;
                self.fingerprint_to_ref
                    .insert(fingerprint.clone(), new_ref.clone());
                self.insertion_order.push(new_ref.clone());
                new_ref
            };

            let entry = RefEntry {
                ref_id: ref_id.clone(),
                fingerprint,
                frame_id: frame_id.to_string(),
                role: node.role.clone(),
                name: norm_name.clone(),
                value: node.value.clone(),
                states: node.states.clone(),
                backend_dom_node_id: node.backend_dom_node_id,
                nth,
                semantic_path: semantic_path_str,
                stale: false,
            };

            self.entries.insert(ref_id, entry.clone());
            items.push(SnapshotItem { depth, entry });
            next_depth = depth + 1;
        }

        let pushed_landmark = is_landmark_role(&node.role) && node.role != "RootWebArea";
        if pushed_landmark {
            let segment = if norm_name.is_empty() {
                node.role.to_ascii_lowercase()
            } else {
                format!(
                    "{}:{}",
                    node.role.to_ascii_lowercase(),
                    norm_name.to_ascii_lowercase()
                )
            };
            path.push(segment);
        }

        for child in &node.children {
            self.walk_node(
                frame_id,
                child,
                next_depth,
                interactive_only,
                path,
                occurrence_counts,
                items,
            );
        }

        if pushed_landmark {
            path.pop();
        }
    }

    pub fn resolve_ref(&self, ref_id: &str) -> Option<&RefEntry> {
        self.entries.get(ref_id)
    }

    pub fn resolve_active_ref(&self, ref_id: &str) -> Result<&RefEntry, CdpError> {
        let entry = self
            .entries
            .get(ref_id)
            .ok_or_else(|| CdpError::InvalidArgument(format!("unknown element ref {ref_id}")))?;
        if entry.stale {
            return Err(CdpError::StaleNode);
        }
        Ok(entry)
    }

    pub fn find_by_fingerprint(&self, fingerprint: &ElementFingerprint) -> Option<&RefEntry> {
        let ref_id = self.fingerprint_to_ref.get(fingerprint)?;
        self.entries.get(ref_id).filter(|e| !e.stale)
    }

    pub fn get_by_role(&self, role: &str, name: Option<&str>, exact: bool) -> Vec<&RefEntry> {
        self.active_entries()
            .filter(|e| e.role.eq_ignore_ascii_case(role))
            .filter(|e| match name {
                Some(target) => matches_text(&e.name, target, exact),
                None => true,
            })
            .collect()
    }

    pub fn get_by_label(&self, label: &str, exact: bool) -> Vec<&RefEntry> {
        self.active_entries()
            .filter(|e| is_interactive_role(&e.role) && matches_text(&e.name, label, exact))
            .collect()
    }

    pub fn get_by_text(&self, text: &str, exact: bool) -> Vec<&RefEntry> {
        self.active_entries()
            .filter(|e| {
                matches_text(&e.name, text, exact)
                    || e.value
                        .as_deref()
                        .is_some_and(|v| matches_text(v, text, exact))
            })
            .collect()
    }

    fn active_entries(&self) -> impl Iterator<Item = &RefEntry> {
        self.insertion_order
            .iter()
            .filter_map(|id| self.entries.get(id))
            .filter(|e| !e.stale)
    }
}

fn matches_text(actual: &str, query: &str, exact: bool) -> bool {
    if exact {
        actual == query
    } else {
        actual
            .to_ascii_lowercase()
            .contains(&query.to_ascii_lowercase())
    }
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use crate::ax_tree::parse_full_ax_tree;

    use super::*;

    fn sample_tree(include_promo_banner: bool, button_dom_id: i64) -> Vec<AxNode> {
        let mut main_children = Vec::new();
        if include_promo_banner {
            main_children.push("promo-banner");
        }
        main_children.push("login-form");

        let mut nodes = vec![
            json!({
                "nodeId": "root",
                "role": { "type": "role", "value": "RootWebArea" },
                "name": { "type": "computedString", "value": "Portal" },
                "childIds": ["main"]
            }),
            json!({
                "nodeId": "main",
                "parentId": "root",
                "role": { "type": "role", "value": "main" },
                "childIds": main_children
            }),
            json!({
                "nodeId": "login-form",
                "parentId": "main",
                "role": { "type": "role", "value": "form" },
                "name": { "type": "computedString", "value": "Login" },
                "childIds": ["email-input", "submit-btn"]
            }),
            json!({
                "nodeId": "email-input",
                "parentId": "login-form",
                "role": { "type": "role", "value": "textbox" },
                "name": { "type": "computedString", "value": "Email" },
                "backendDOMNodeId": 201,
                "childIds": []
            }),
            json!({
                "nodeId": "submit-btn",
                "parentId": "login-form",
                "role": { "type": "role", "value": "button" },
                "name": { "type": "computedString", "value": "Sign In" },
                "backendDOMNodeId": button_dom_id,
                "childIds": []
            }),
        ];

        if include_promo_banner {
            nodes.push(json!({
                "nodeId": "promo-banner",
                "parentId": "main",
                "role": { "type": "role", "value": "region" },
                "name": { "type": "computedString", "value": "Promo" },
                "childIds": ["dismiss-btn"]
            }));
            nodes.push(json!({
                "nodeId": "dismiss-btn",
                "parentId": "promo-banner",
                "role": { "type": "role", "value": "button" },
                "name": { "type": "computedString", "value": "Dismiss" },
                "backendDOMNodeId": 901,
                "childIds": []
            }));
        }

        parse_full_ax_tree(&json!({ "nodes": nodes })).unwrap()
    }

    #[test]
    fn preserves_ref_ids_across_dom_prepend_and_backend_node_recreation() {
        let mut registry = RefRegistry::new();

        let snap1 = registry.update_from_ax_tree("main", &sample_tree(false, 202), true);
        let initial_btn = registry.get_by_role("button", Some("Sign In"), true)[0].clone();
        assert_eq!(initial_btn.ref_id, "e2");
        assert!(snap1.render_compact().contains("[e2] button \"Sign In\""));

        // Prepend a promo banner with another button AND recreate the Sign In button DOM node (202 -> 888).
        let _snap2 = registry.update_from_ax_tree("main", &sample_tree(true, 888), true);

        let after_btn = registry.get_by_role("button", Some("Sign In"), true)[0];
        assert_eq!(
            after_btn.ref_id, initial_btn.ref_id,
            "Sign In button must keep deterministic ref e2"
        );
        assert_eq!(after_btn.backend_dom_node_id, Some(888));

        let dismiss_btn = registry.get_by_role("button", Some("Dismiss"), true)[0];
        assert_eq!(dismiss_btn.ref_id, "e3");
        assert_eq!(registry.get_by_label("Email", true)[0].ref_id, "e1");
        assert_eq!(registry.get_by_text("Sign", false)[0].ref_id, "e2");
    }

    #[test]
    fn marks_removed_nodes_as_stale() {
        let mut registry = RefRegistry::new();
        registry.update_from_ax_tree("main", &sample_tree(true, 202), true);
        let dismiss_ref = registry.get_by_role("button", Some("Dismiss"), true)[0]
            .ref_id
            .clone();

        // Remove promo banner.
        registry.update_from_ax_tree("main", &sample_tree(false, 202), true);
        assert!(registry.resolve_ref(&dismiss_ref).unwrap().stale);
        assert!(matches!(
            registry.resolve_active_ref(&dismiss_ref),
            Err(CdpError::StaleNode)
        ));
    }
}
