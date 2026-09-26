//! Show Selector (plan Phase 4b.2): turn an inspected node into the
//! `native_accessibility` workflow step fragment that re-finds it.
//!
//! The output is not a new selector grammar — it is literally the `query`
//! step the runner already executes
//! (`qontinui-runner/src-tauri/src/step_executor/handlers/native_accessibility.rs`,
//! `build_query` / `action_query`), so a user can paste it into a workflow.
//! [`query_builder_for`] mirrors that handler's `build_query` filter mapping
//! exactly (serde role parse, blank automation id / class name skipped, label
//! applied verbatim), which is what makes [`SelectorInfo::match_count`] a
//! prediction of what the step will see rather than an approximation.
//!
//! Session refs (`@e3`) are reported too, but only as a convenience: the ref
//! manager assigns them per capture session and they are NOT stable across
//! sessions, so they are never the recommended selector.

use serde::{Deserialize, Serialize};

use qontinui_runner_lib::accessibility::{
    model::{UnifiedNode, UnifiedRole},
    query::QueryBuilder,
};

use crate::tree::{normalize_ref, overlay_subtree_refs};

/// The `a11y_action` value of the step fragment.
pub const QUERY_ACTION: &str = "query";

/// Caveat attached to every `session_ref`.
pub const SESSION_REF_NOTE: &str =
    "session ref: valid only for the current capture session, not stable across sessions";

/// A `native_accessibility` step fragment using the `query` action.
///
/// Field names are the step's own snake_case field names
/// (`ExecutionStepConfig` in the runner's `executor_types.rs`); absent
/// filters are omitted rather than serialized as `null`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct QueryStep {
    pub a11y_action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_query_automation_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_query_class_name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_query_role: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a11y_query_label: Option<String>,
}

/// Which identifying property the step relies on.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SelectorStrategy {
    /// UIA AutomationId / test id — the most stable native identifier.
    AutomationId,
    /// Class name + role + label, used when there is no automation id.
    Attributes,
}

/// Show Selector result: the step fragment plus how well it discriminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorInfo {
    pub step: QueryStep,
    pub strategy: SelectorStrategy,
    /// Nodes in the cached snapshot the step's query matches (the
    /// inspector's own overlay window excluded).
    pub match_count: usize,
    /// True only when exactly one node matches AND it is the inspected node.
    /// A single match that is some OTHER node is ambiguous, not unique.
    pub unique: bool,
    /// `@<ref_id>` — see [`SESSION_REF_NOTE`].
    pub session_ref: String,
    pub session_ref_note: String,
}

fn non_blank(value: Option<&str>) -> Option<&str> {
    value.map(str::trim).filter(|v| !v.is_empty())
}

/// The role's wire name, via serde — the same form the step handler parses
/// with `serde_json::from_value::<UnifiedRole>`. `UnifiedRole::as_str` is the
/// Python-compat name and is not guaranteed to round-trip through serde.
pub fn role_wire_name(role: UnifiedRole) -> String {
    match serde_json::to_value(role) {
        Ok(serde_json::Value::String(s)) => s,
        _ => role.as_str().to_string(),
    }
}

/// The step fragment that re-finds `node`.
///
/// Prefers the automation id alone when it is non-blank; otherwise combines
/// class name (when non-blank), role, and label (when non-empty).
pub fn query_step_for(node: &UnifiedNode) -> (QueryStep, SelectorStrategy) {
    if let Some(id) = non_blank(node.automation_id.as_deref()) {
        return (
            QueryStep {
                a11y_action: QUERY_ACTION.to_string(),
                a11y_query_automation_id: Some(id.to_string()),
                a11y_query_class_name: None,
                a11y_query_role: None,
                a11y_query_label: None,
            },
            SelectorStrategy::AutomationId,
        );
    }
    (
        QueryStep {
            a11y_action: QUERY_ACTION.to_string(),
            a11y_query_automation_id: None,
            a11y_query_class_name: non_blank(node.class_name.as_deref()).map(str::to_string),
            a11y_query_role: Some(role_wire_name(node.role)),
            // The handler matches the label verbatim (exact, case-sensitive,
            // untrimmed), so it is emitted verbatim; only an empty name is
            // dropped, since it would match every unnamed node.
            a11y_query_label: node
                .name
                .as_deref()
                .filter(|n| !n.is_empty())
                .map(str::to_string),
        },
        SelectorStrategy::Attributes,
    )
}

/// Mirror of the runner step handler's `build_query` for the fields
/// [`QueryStep`] carries. Errors exactly where the handler errors: on a role
/// string serde cannot parse.
pub fn query_builder_for(step: &QueryStep) -> Result<QueryBuilder, String> {
    let mut builder = QueryBuilder::new();
    if let Some(ref role_str) = step.a11y_query_role {
        let role =
            serde_json::from_value::<UnifiedRole>(serde_json::Value::String(role_str.clone()))
                .map_err(|_| format!("Unknown accessibility role: '{}'", role_str))?;
        builder = builder.by_role(role);
    }
    if let Some(ref label) = step.a11y_query_label {
        builder = builder.by_label(label.as_str());
    }
    if let Some(id) = non_blank(step.a11y_query_automation_id.as_deref()) {
        builder = builder.by_automation_id(id);
    }
    if let Some(class_name) = non_blank(step.a11y_query_class_name.as_deref()) {
        builder = builder.by_class_name(class_name);
    }
    Ok(builder)
}

/// Every node under `root` the step matches, excluding the inspector's own
/// overlay window (which only exists because the inspector is drawing it).
pub fn find_matches<'a>(
    step: &QueryStep,
    root: &'a UnifiedNode,
) -> Result<Vec<&'a UnifiedNode>, String> {
    let excluded = overlay_subtree_refs(root);
    let mut hits = query_builder_for(step)?.find_all(root);
    if !excluded.is_empty() {
        hits.retain(|n| !excluded.contains(&n.ref_id));
    }
    Ok(hits)
}

/// Build the Show Selector result for `node` against the snapshot `root`.
pub fn selector_for(node: &UnifiedNode, root: &UnifiedNode) -> SelectorInfo {
    let (step, strategy) = query_step_for(node);
    // query_step_for only emits serde-produced role names, so this cannot
    // fail in practice; a failure is reported as zero matches, not a panic.
    let hits = find_matches(&step, root).unwrap_or_default();
    let match_count = hits.len();
    let unique = match_count == 1 && hits[0].ref_id == node.ref_id;
    SelectorInfo {
        step,
        strategy,
        match_count,
        unique,
        session_ref: normalize_ref(&node.ref_id),
        session_ref_note: SESSION_REF_NOTE.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::test_support::node;
    use crate::tree::OVERLAY_WINDOW_TITLE;

    /// window e1
    ///   button e2  automation_id=btn_save  class=PushButton  name=Save
    ///   button e3  automation_id="  "      class=PushButton  name=Cancel
    ///   button e4  (no id)                 class=PushButton  name=Cancel
    ///   textbox e5 (no id, no class)       name=""
    ///   textbox e6 (no id, no class)       name=None
    fn tree() -> UnifiedNode {
        let mut root = node("e1", UnifiedRole::Window);
        let mut save = node("e2", UnifiedRole::Button);
        save.automation_id = Some("btn_save".into());
        save.class_name = Some("PushButton".into());
        save.name = Some("Save".into());
        let mut cancel_a = node("e3", UnifiedRole::Button);
        cancel_a.automation_id = Some("  ".into());
        cancel_a.class_name = Some("PushButton".into());
        cancel_a.name = Some("Cancel".into());
        let mut cancel_b = node("e4", UnifiedRole::Button);
        cancel_b.class_name = Some("PushButton".into());
        cancel_b.name = Some("Cancel".into());
        let mut edit_a = node("e5", UnifiedRole::Textbox);
        edit_a.name = Some(String::new());
        let edit_b = node("e6", UnifiedRole::Textbox);
        root.children = vec![save, cancel_a, cancel_b, edit_a, edit_b];
        root
    }

    #[test]
    fn automation_id_is_preferred_and_alone() {
        let t = tree();
        let info = selector_for(&t.children[0], &t);
        assert_eq!(info.strategy, SelectorStrategy::AutomationId);
        assert_eq!(
            serde_json::to_value(&info.step).unwrap(),
            serde_json::json!({"a11y_action": "query", "a11y_query_automation_id": "btn_save"})
        );
        assert_eq!(info.match_count, 1);
        assert!(info.unique);
        assert_eq!(info.session_ref, "@e2");
    }

    #[test]
    fn blank_automation_id_falls_back_to_attributes() {
        let t = tree();
        let info = selector_for(&t.children[1], &t);
        assert_eq!(info.strategy, SelectorStrategy::Attributes);
        assert_eq!(
            serde_json::to_value(&info.step).unwrap(),
            serde_json::json!({
                "a11y_action": "query",
                "a11y_query_class_name": "PushButton",
                "a11y_query_role": "button",
                "a11y_query_label": "Cancel",
            })
        );
        // e3 and e4 are indistinguishable by class+role+label.
        assert_eq!(info.match_count, 2);
        assert!(!info.unique);
    }

    #[test]
    fn empty_and_missing_fields_are_omitted() {
        let t = tree();
        let info = selector_for(&t.children[3], &t);
        assert_eq!(
            serde_json::to_value(&info.step).unwrap(),
            serde_json::json!({"a11y_action": "query", "a11y_query_role": "textbox"})
        );
        // Role alone matches both textboxes.
        assert_eq!(info.match_count, 2);
        assert!(!info.unique);
    }

    #[test]
    fn single_match_that_is_another_node_is_not_unique() {
        let t = tree();
        // A node not in the tree whose selector matches e2.
        let mut stranger = node("e99", UnifiedRole::Button);
        stranger.automation_id = Some("btn_save".into());
        let info = selector_for(&stranger, &t);
        assert_eq!(info.match_count, 1);
        assert!(!info.unique);
    }

    #[test]
    fn role_wire_name_round_trips_through_the_handler_parse() {
        for role in [
            UnifiedRole::Button,
            UnifiedRole::Textbox,
            UnifiedRole::Window,
            UnifiedRole::Unknown,
        ] {
            let name = role_wire_name(role);
            let parsed: UnifiedRole =
                serde_json::from_value(serde_json::Value::String(name)).unwrap();
            assert_eq!(parsed, role);
        }
    }

    #[test]
    fn unknown_role_is_an_error_like_the_handler() {
        let step = QueryStep {
            a11y_action: QUERY_ACTION.into(),
            a11y_query_automation_id: None,
            a11y_query_class_name: None,
            a11y_query_role: Some("not_a_role".into()),
            a11y_query_label: None,
        };
        assert!(query_builder_for(&step).is_err());
    }

    #[test]
    fn overlay_window_is_excluded_from_matches() {
        let mut t = tree();
        let mut overlay = node("e50", UnifiedRole::Window);
        overlay.name = Some(OVERLAY_WINDOW_TITLE.into());
        let mut inner = node("e51", UnifiedRole::Button);
        inner.automation_id = Some("btn_save".into());
        overlay.children.push(inner);
        t.children.insert(0, overlay);
        let info = selector_for(&t.children[1], &t);
        assert_eq!(info.match_count, 1);
        assert!(info.unique);
    }
}
