//! Show Selector (plan Phase 4b.2): turn an inspected node into the
//! `native_accessibility` workflow step fragment that re-finds it.
//!
//! The output is not a new selector grammar — it is literally the `query`
//! step the runner already executes
//! (`qontinui-runner/src-tauri/src/step_executor/handlers/native_accessibility.rs`,
//! `build_query` / `action_query`), so a user can paste it into a workflow.
//! [`query_builder_for`] mirrors that handler's `build_query` filter mapping
//! exactly (serde role parse; blank — empty or whitespace-only — label,
//! automation id and class name skipped; every non-blank value matched exactly
//! as given, never trimmed), which is what makes [`SelectorInfo::match_count`]
//! a prediction of what the step will see on the captured desktop.
//!
//! # Narrowing
//!
//! The automation id alone is preferred. When it is not unique, the selector is
//! narrowed by adding class name and role, then label, re-counting each time;
//! the first candidate that uniquely matches the inspected node wins, else the
//! one with the fewest matches.
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
    /// Automation id narrowed with class name + role (and label, if still
    /// ambiguous), because the id alone matched several nodes.
    AutomationIdNarrowed,
    /// Class name + role + label, used when there is no automation id.
    Attributes,
}

/// Show Selector result: the step fragment plus how well it discriminates.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SelectorInfo {
    pub step: QueryStep,
    pub strategy: SelectorStrategy,
    /// Matches on the captured desktop: nodes in the cached whole-desktop
    /// snapshot the step's query matches (the inspector's own overlay window
    /// excluded). An upper bound for a step whose target is narrower than the
    /// desktop (a window or process), which searches a subset of this tree.
    pub match_count: usize,
    /// True only when exactly one node matches AND it is the inspected node.
    /// A single match that is some OTHER node is ambiguous, not unique.
    pub unique: bool,
    /// `@<ref_id>` — see [`SESSION_REF_NOTE`].
    pub session_ref: String,
    pub session_ref_note: String,
}

/// The value as given, or `None` when absent or blank (empty or
/// whitespace-only). Mirrors the handler's `non_blank`: never trimmed, because
/// the handler compares non-blank values exactly.
fn non_blank(value: Option<&str>) -> Option<&str> {
    value.filter(|v| !v.trim().is_empty())
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

fn owned(value: Option<&str>) -> Option<String> {
    non_blank(value).map(str::to_string)
}

/// Candidate step fragments for `node`, broadest first.
///
/// With a non-blank automation id: the id alone, then id + class name + role,
/// then id + class name + role + label. Without one: class name + role +
/// label. Values are emitted exactly as the node carries them (the handler
/// compares exactly); blank ones are omitted, as the handler would skip them.
pub fn candidate_steps(node: &UnifiedNode) -> Vec<(QueryStep, SelectorStrategy)> {
    let class_name = owned(node.class_name.as_deref());
    let role = Some(role_wire_name(node.role));
    let label = owned(node.name.as_deref());
    let step = |automation_id: Option<String>,
                class_name: Option<String>,
                role: Option<String>,
                label: Option<String>| QueryStep {
        a11y_action: QUERY_ACTION.to_string(),
        a11y_query_automation_id: automation_id,
        a11y_query_class_name: class_name,
        a11y_query_role: role,
        a11y_query_label: label,
    };
    match owned(node.automation_id.as_deref()) {
        Some(id) => vec![
            (
                step(Some(id.clone()), None, None, None),
                SelectorStrategy::AutomationId,
            ),
            (
                step(Some(id.clone()), class_name.clone(), role.clone(), None),
                SelectorStrategy::AutomationIdNarrowed,
            ),
            (
                step(Some(id), class_name, role, label),
                SelectorStrategy::AutomationIdNarrowed,
            ),
        ],
        None => vec![(
            step(None, class_name, role, label),
            SelectorStrategy::Attributes,
        )],
    }
}

/// The broadest step fragment that re-finds `node`, without counting matches
/// (the first of [`candidate_steps`]). [`selector_for`] narrows it.
pub fn query_step_for(node: &UnifiedNode) -> (QueryStep, SelectorStrategy) {
    candidate_steps(node)
        .into_iter()
        .next()
        .expect("candidate_steps always yields at least one step")
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
    if let Some(label) = non_blank(step.a11y_query_label.as_deref()) {
        builder = builder.by_label(label);
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

/// Build the Show Selector result for `node` against the snapshot `root`,
/// narrowing the automation-id selector when it is ambiguous (module docs).
pub fn selector_for(node: &UnifiedNode, root: &UnifiedNode) -> SelectorInfo {
    let mut best: Option<(QueryStep, SelectorStrategy, usize, bool)> = None;
    for (step, strategy) in candidate_steps(node) {
        // candidate_steps only emits serde-produced role names, so this cannot
        // fail in practice; a failure is reported as zero matches, not a panic.
        let hits = find_matches(&step, root).unwrap_or_default();
        let match_count = hits.len();
        let unique = match_count == 1 && hits[0].ref_id == node.ref_id;
        let narrower = best.as_ref().is_none_or(|b| match_count < b.2);
        if narrower {
            best = Some((step, strategy, match_count, unique));
        }
        if unique {
            break;
        }
    }
    let (step, strategy, match_count, unique) =
        best.expect("candidate_steps always yields at least one step");
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

    /// Two windows each holding an "ok" button (same automation id), one a
    /// PushButton and one a SplitButton, plus a checkbox also carrying "ok".
    fn duplicated_id_tree() -> UnifiedNode {
        let mut root = node("e1", UnifiedRole::Window);
        let mut ok_push = node("e2", UnifiedRole::Button);
        ok_push.automation_id = Some("ok".into());
        ok_push.class_name = Some("PushButton".into());
        ok_push.name = Some("OK".into());
        let mut ok_split = node("e3", UnifiedRole::Button);
        ok_split.automation_id = Some("ok".into());
        ok_split.class_name = Some("SplitButton".into());
        ok_split.name = Some("OK".into());
        let mut ok_check = node("e4", UnifiedRole::Checkbox);
        ok_check.automation_id = Some("ok".into());
        ok_check.class_name = Some("PushButton".into());
        ok_check.name = Some("OK".into());
        let mut ok_push_twin = node("e5", UnifiedRole::Button);
        ok_push_twin.automation_id = Some("ok".into());
        ok_push_twin.class_name = Some("PushButton".into());
        ok_push_twin.name = Some("Apply".into());
        root.children = vec![ok_push, ok_split, ok_check, ok_push_twin];
        root
    }

    #[test]
    fn duplicated_automation_id_is_narrowed_by_class_and_role() {
        let t = duplicated_id_tree();
        // "ok" alone matches 4; + SplitButton + button matches only e3.
        let info = selector_for(&t.children[1], &t);
        assert_eq!(info.strategy, SelectorStrategy::AutomationIdNarrowed);
        assert_eq!(
            serde_json::to_value(&info.step).unwrap(),
            serde_json::json!({
                "a11y_action": "query",
                "a11y_query_automation_id": "ok",
                "a11y_query_class_name": "SplitButton",
                "a11y_query_role": "button",
            })
        );
        assert_eq!(info.match_count, 1);
        assert!(info.unique);
    }

    #[test]
    fn duplicated_automation_id_adds_label_when_still_ambiguous() {
        let t = duplicated_id_tree();
        // + PushButton + button still matches e2 and e5; the label splits them.
        let info = selector_for(&t.children[0], &t);
        assert_eq!(info.strategy, SelectorStrategy::AutomationIdNarrowed);
        assert_eq!(info.step.a11y_query_label.as_deref(), Some("OK"));
        assert_eq!(info.match_count, 1);
        assert!(info.unique);
    }

    #[test]
    fn narrowing_keeps_the_narrowest_when_nothing_is_unique() {
        let mut t = duplicated_id_tree();
        // An exact twin of e2: no candidate can tell them apart.
        let mut twin = t.children[0].clone();
        twin.ref_id = "e9".into();
        t.children.push(twin);
        let info = selector_for(&t.children[0], &t);
        assert!(!info.unique);
        // id alone matches 5; + class + role matches 3; + label matches 2.
        assert_eq!(info.match_count, 2);
        assert_eq!(info.step.a11y_query_label.as_deref(), Some("OK"));
    }

    #[test]
    fn padded_values_are_emitted_verbatim_and_match_exactly() {
        let mut t = tree();
        t.children[0].automation_id = Some(" btn_save ".into());
        let info = selector_for(&t.children[0], &t);
        assert_eq!(info.strategy, SelectorStrategy::AutomationId);
        assert_eq!(
            info.step.a11y_query_automation_id.as_deref(),
            Some(" btn_save ")
        );
        assert_eq!(info.match_count, 1);
        assert!(info.unique);
    }

    #[test]
    fn whitespace_only_label_is_omitted_and_skipped() {
        let mut t = tree();
        t.children[3].name = Some("   ".into());
        let info = selector_for(&t.children[3], &t);
        assert_eq!(info.step.a11y_query_label, None);
        // A hand-written step with a blank label is skipped like the handler does.
        let step = QueryStep {
            a11y_action: QUERY_ACTION.into(),
            a11y_query_automation_id: None,
            a11y_query_class_name: None,
            a11y_query_role: Some("textbox".into()),
            a11y_query_label: Some("  ".into()),
        };
        assert_eq!(find_matches(&step, &t).unwrap().len(), 2);
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
