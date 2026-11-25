#![forbid(unsafe_code)]

use std::collections::{HashMap, HashSet};

use ferrite_core::CdpError;
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawAxValue {
    #[serde(rename = "type")]
    pub value_type: Option<String>,
    pub value: Option<Value>,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawAxProperty {
    pub name: String,
    pub value: RawAxValue,
}

#[derive(Debug, Clone, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct RawAxNode {
    pub node_id: String,
    #[serde(default)]
    pub ignored: bool,
    pub role: Option<RawAxValue>,
    pub name: Option<RawAxValue>,
    pub value: Option<RawAxValue>,
    #[serde(default)]
    pub properties: Vec<RawAxProperty>,
    #[serde(default)]
    pub child_ids: Vec<String>,
    pub parent_id: Option<String>,
    #[serde(rename = "backendDOMNodeId")]
    pub backend_dom_node_id: Option<i64>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxStates {
    pub disabled: bool,
    pub checked: Option<bool>,
    pub expanded: Option<bool>,
    pub focused: bool,
    pub selected: bool,
    pub required: bool,
    pub level: Option<u32>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AxNode {
    pub node_id: String,
    pub backend_dom_node_id: Option<i64>,
    pub role: String,
    pub name: String,
    pub value: Option<String>,
    pub states: AxStates,
    pub children: Vec<AxNode>,
}

pub fn is_interactive_role(role: &str) -> bool {
    matches!(
        role,
        "button"
            | "link"
            | "textbox"
            | "searchbox"
            | "checkbox"
            | "radio"
            | "switch"
            | "combobox"
            | "listbox"
            | "option"
            | "menuitem"
            | "menuitemcheckbox"
            | "menuitemradio"
            | "tab"
            | "slider"
            | "spinbutton"
            | "treeitem"
    )
}

pub fn is_landmark_role(role: &str) -> bool {
    matches!(
        role,
        "RootWebArea"
            | "main"
            | "navigation"
            | "banner"
            | "contentinfo"
            | "complementary"
            | "region"
            | "form"
            | "search"
            | "dialog"
            | "alertdialog"
            | "table"
            | "list"
            | "menu"
            | "tablist"
            | "toolbar"
            | "article"
            | "section"
    )
}

pub fn is_semantic_role(role: &str) -> bool {
    is_interactive_role(role)
        || is_landmark_role(role)
        || matches!(
            role,
            "heading"
                | "img"
                | "alert"
                | "status"
                | "progressbar"
                | "StaticText"
                | "listitem"
                | "row"
                | "cell"
                | "columnheader"
                | "rowheader"
        )
}

fn ax_value_to_string(val: Option<&RawAxValue>) -> Option<String> {
    let raw = val?.value.as_ref()?;
    match raw {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn ax_value_to_bool(val: &RawAxValue) -> Option<bool> {
    match val.value.as_ref()? {
        Value::Bool(b) => Some(*b),
        Value::String(s) if s.eq_ignore_ascii_case("true") => Some(true),
        Value::String(s) if s.eq_ignore_ascii_case("false") => Some(false),
        _ => None,
    }
}

fn extract_states(properties: &[RawAxProperty]) -> AxStates {
    let mut states = AxStates::default();
    for prop in properties {
        match prop.name.as_str() {
            "disabled" => states.disabled = ax_value_to_bool(&prop.value).unwrap_or(false),
            "checked" => states.checked = ax_value_to_bool(&prop.value),
            "expanded" => states.expanded = ax_value_to_bool(&prop.value),
            "focused" => states.focused = ax_value_to_bool(&prop.value).unwrap_or(false),
            "selected" => states.selected = ax_value_to_bool(&prop.value).unwrap_or(false),
            "required" => states.required = ax_value_to_bool(&prop.value).unwrap_or(false),
            "level" => {
                states.level = prop
                    .value
                    .value
                    .as_ref()
                    .and_then(Value::as_u64)
                    .map(|v| v as u32);
            }
            _ => {}
        }
    }
    states
}

/// Parses a CDP `Accessibility.getFullAXTree` response (`{ "nodes": [...] }`), hoisting children
/// of `ignored: true` or wrapper `none`/`generic` nodes cleanly into a semantic AX hierarchy.
pub fn parse_full_ax_tree(response: &Value) -> Result<Vec<AxNode>, CdpError> {
    let nodes_val = response.get("nodes").unwrap_or(response);
    let raw_nodes: Vec<RawAxNode> =
        serde_json::from_value(nodes_val.clone()).map_err(|err| CdpError::Protocol {
            code: None,
            message: format!("invalid Accessibility.getFullAXTree payload: {err}"),
        })?;

    if raw_nodes.is_empty() {
        return Ok(Vec::new());
    }

    let by_id: HashMap<String, RawAxNode> = raw_nodes
        .iter()
        .cloned()
        .map(|n| (n.node_id.clone(), n))
        .collect();

    let mut has_parent = HashSet::new();
    for n in &raw_nodes {
        for cid in &n.child_ids {
            has_parent.insert(cid.as_str());
        }
    }

    let root_ids: Vec<String> = raw_nodes
        .iter()
        .filter(|n| n.parent_id.is_none() && !has_parent.contains(n.node_id.as_str()))
        .map(|n| n.node_id.clone())
        .collect();

    let starts = if root_ids.is_empty() {
        vec![raw_nodes[0].node_id.clone()]
    } else {
        root_ids
    };

    let mut visited = HashSet::new();
    let mut roots = Vec::new();
    for id in starts {
        roots.extend(build_clean_nodes(&id, &by_id, &mut visited, None));
    }
    Ok(roots)
}

fn build_clean_nodes(
    node_id: &str,
    by_id: &HashMap<String, RawAxNode>,
    visited: &mut HashSet<String>,
    inherited_backend_id: Option<i64>,
) -> Vec<AxNode> {
    if !visited.insert(node_id.to_string()) {
        return Vec::new();
    }
    let Some(raw) = by_id.get(node_id) else {
        return Vec::new();
    };

    let effective_backend_id = raw.backend_dom_node_id.or(inherited_backend_id);

    let mut children = Vec::new();
    for child_id in &raw.child_ids {
        children.extend(build_clean_nodes(
            child_id,
            by_id,
            visited,
            effective_backend_id,
        ));
    }

    let role = ax_value_to_string(raw.role.as_ref()).unwrap_or_else(|| "none".to_string());
    let name = ax_value_to_string(raw.name.as_ref()).unwrap_or_default();
    let value = ax_value_to_string(raw.value.as_ref());

    // Hoist children of ignored nodes or unlabelled generic/none structural wrappers.
    let is_structural_wrapper =
        raw.ignored || ((role == "none" || role == "generic" || role == "InlineTextBox") && name.is_empty());

    if is_structural_wrapper {
        return children;
    }

    vec![AxNode {
        node_id: raw.node_id.clone(),
        backend_dom_node_id: effective_backend_id,
        role,
        name,
        value,
        states: extract_states(&raw.properties),
        children,
    }]
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn hoists_children_through_ignored_and_generic_wrappers() {
        let payload = json!({
            "nodes": [
                {
                    "nodeId": "1",
                    "role": { "type": "role", "value": "RootWebArea" },
                    "name": { "type": "computedString", "value": "App" },
                    "childIds": ["2"]
                },
                {
                    "nodeId": "2",
                    "parentId": "1",
                    "ignored": true,
                    "role": { "type": "role", "value": "generic" },
                    "childIds": ["3"]
                },
                {
                    "nodeId": "3",
                    "parentId": "2",
                    "ignored": false,
                    "role": { "type": "role", "value": "generic" },
                    "childIds": ["4"]
                },
                {
                    "nodeId": "4",
                    "parentId": "3",
                    "role": { "type": "role", "value": "button" },
                    "name": { "type": "computedString", "value": "Submit" },
                    "backendDOMNodeId": 99,
                    "properties": [{ "name": "disabled", "value": { "type": "boolean", "value": false } }],
                    "childIds": []
                }
            ]
        });

        let roots = parse_full_ax_tree(&payload).unwrap();
        assert_eq!(roots.len(), 1);
        assert_eq!(roots[0].role, "RootWebArea");
        assert_eq!(roots[0].children.len(), 1);
        assert_eq!(roots[0].children[0].role, "button");
        assert_eq!(roots[0].children[0].name, "Submit");
        assert_eq!(roots[0].children[0].backend_dom_node_id, Some(99));
    }
}
