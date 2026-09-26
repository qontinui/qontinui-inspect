//! Pure walks over a captured `UnifiedNode` tree.
//!
//! Everything here is synchronous and side-effect free so it can be unit
//! tested against hand-built trees — the Tauri commands in `lib.rs` and the
//! focus task in `focus.rs` only lock the manager, take a snapshot, and call in.

use std::collections::HashSet;

use qontinui_runner_lib::accessibility::model::{UnifiedBounds, UnifiedNode};

/// Title of the inspector's own in-target overlay window (see `overlay.rs`).
///
/// The overlay is a real top-level window, so a desktop capture taken while it
/// is visible contains it. It sits ON TOP of the element it outlines, which
/// means an unfiltered hit-test would resolve the overlay instead of the
/// target and a selector match count would include it. Every walk below skips
/// the subtree rooted at a node carrying this name.
pub const OVERLAY_WINDOW_TITLE: &str = "Qontinui Inspector Overlay";

/// Canonical ref form. The runner's ref manager assigns refs WITH the sigil
/// (`"@e3"`), but users type and paste them either way, so every lookup and
/// every displayed ref goes through here: trimmed, exactly one leading `@`.
pub fn normalize_ref(ref_id: &str) -> String {
    format!("@{}", ref_id.trim().trim_start_matches('@'))
}

/// Whether `node` is the inspector's overlay window.
pub fn is_overlay_window(node: &UnifiedNode) -> bool {
    node.name.as_deref() == Some(OVERLAY_WINDOW_TITLE)
}

/// Whether the screen point `(x, y)` falls inside `node`'s bounds.
pub fn node_contains(node: &UnifiedNode, x: i32, y: i32) -> bool {
    match &node.bounds {
        Some(b) => x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height,
        None => false,
    }
}

/// The deepest node whose bounds contain `(x, y)`, skipping the overlay.
pub fn find_deepest_at(node: &UnifiedNode, x: i32, y: i32) -> Option<&UnifiedNode> {
    if is_overlay_window(node) || !node_contains(node, x, y) {
        return None;
    }
    for child in &node.children {
        if let Some(hit) = find_deepest_at(child, x, y) {
            return Some(hit);
        }
    }
    Some(node)
}

/// The deepest node reporting `state.is_focused`, skipping the overlay.
///
/// Deepest rather than first because some backends also flag the focused
/// element's container (the window that holds keyboard focus); the element
/// the user is typing into is the leaf-most one.
pub fn find_focused(node: &UnifiedNode) -> Option<&UnifiedNode> {
    if is_overlay_window(node) {
        return None;
    }
    for child in &node.children {
        if let Some(hit) = find_focused(child) {
            return Some(hit);
        }
    }
    if node.state.is_focused {
        Some(node)
    } else {
        None
    }
}

/// The single node whose accessible name equals `name`, or `None` when zero
/// or several nodes carry it. Used only as a fallback for focus events that
/// carry a name but no resolvable ref, where guessing between duplicates would
/// select the wrong element.
pub fn find_unique_by_name<'a>(root: &'a UnifiedNode, name: &str) -> Option<&'a UnifiedNode> {
    fn walk<'a>(node: &'a UnifiedNode, name: &str, hits: &mut Vec<&'a UnifiedNode>) {
        if is_overlay_window(node) || hits.len() > 1 {
            return;
        }
        if node.name.as_deref() == Some(name) {
            hits.push(node);
        }
        for child in &node.children {
            walk(child, name, hits);
        }
    }
    let mut hits = Vec::new();
    walk(root, name, &mut hits);
    if hits.len() == 1 {
        hits.pop()
    } else {
        None
    }
}

/// Refs of every node inside the overlay window's subtree (the window itself
/// included). Empty when no overlay is in the tree.
pub fn overlay_subtree_refs(root: &UnifiedNode) -> HashSet<String> {
    fn collect(node: &UnifiedNode, out: &mut HashSet<String>) {
        out.insert(node.ref_id.clone());
        for child in &node.children {
            collect(child, out);
        }
    }
    fn find(node: &UnifiedNode, out: &mut HashSet<String>) {
        if is_overlay_window(node) {
            collect(node, out);
            return;
        }
        for child in &node.children {
            find(child, out);
        }
    }
    let mut out = HashSet::new();
    find(root, &mut out);
    out
}

/// Bounds of `nodes`, dropping any without bounds.
pub fn bounds_of(nodes: &[&UnifiedNode]) -> Vec<UnifiedBounds> {
    nodes.iter().filter_map(|n| n.bounds.clone()).collect()
}

#[cfg(test)]
pub(crate) mod test_support {
    //! Hand-built trees. The runner's `UnifiedNode: Default` impl is
    //! `#[cfg(test)]` inside the runner crate, so it is not visible here and
    //! every field is spelled out once in `node`.
    use qontinui_runner_lib::accessibility::model::{
        NodeSource, UnifiedBounds, UnifiedNode, UnifiedRole, UnifiedState,
    };

    pub fn node(ref_id: &str, role: UnifiedRole) -> UnifiedNode {
        UnifiedNode {
            ref_id: ref_id.to_string(),
            role,
            name: None,
            value: None,
            description: None,
            bounds: None,
            state: UnifiedState::default(),
            is_interactive: false,
            level: None,
            automation_id: None,
            class_name: None,
            html_tag: None,
            url: None,
            children: vec![],
            source: NodeSource::Uia,
            platform_handle: None,
            supported_patterns: vec![],
            generation: 0,
        }
    }

    pub fn bounds(x: i32, y: i32, width: i32, height: i32) -> UnifiedBounds {
        UnifiedBounds {
            x,
            y,
            width,
            height,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::test_support::{bounds, node};
    use super::*;
    use qontinui_runner_lib::accessibility::model::UnifiedRole;

    fn tree() -> UnifiedNode {
        let mut root = node("e1", UnifiedRole::Window);
        root.bounds = Some(bounds(0, 0, 1000, 1000));

        let mut button = node("e2", UnifiedRole::Button);
        button.name = Some("Save".into());
        button.bounds = Some(bounds(10, 10, 100, 30));

        let mut edit = node("e3", UnifiedRole::Textbox);
        edit.name = Some("Name".into());
        edit.bounds = Some(bounds(10, 50, 200, 30));
        edit.state.is_focused = true;

        let mut overlay = node("e4", UnifiedRole::Window);
        overlay.name = Some(OVERLAY_WINDOW_TITLE.into());
        overlay.bounds = Some(bounds(7, 7, 106, 36));
        let mut overlay_doc = node("e5", UnifiedRole::Document);
        overlay_doc.bounds = Some(bounds(7, 7, 106, 36));
        overlay.children.push(overlay_doc);

        // Overlay first: UIA lists top-level windows topmost-first.
        root.children = vec![overlay, button, edit];
        root
    }

    #[test]
    fn normalize_ref_adds_exactly_one_sigil() {
        assert_eq!(normalize_ref("e3"), "@e3");
        assert_eq!(normalize_ref("@e3"), "@e3");
        assert_eq!(normalize_ref(" @@e3 "), "@e3");
    }

    #[test]
    fn hit_test_skips_the_overlay_window() {
        let t = tree();
        let hit = find_deepest_at(&t, 20, 20).expect("hit");
        assert_eq!(hit.ref_id, "e2");
    }

    #[test]
    fn hit_test_outside_everything_is_none() {
        let t = tree();
        assert!(find_deepest_at(&t, 5000, 5000).is_none());
    }

    #[test]
    fn find_focused_returns_deepest_focused() {
        let mut t = tree();
        t.state.is_focused = true; // container also flagged
        assert_eq!(find_focused(&t).unwrap().ref_id, "e3");
    }

    #[test]
    fn find_focused_none_when_nothing_focused() {
        let mut t = tree();
        t.children[2].state.is_focused = false;
        assert!(find_focused(&t).is_none());
    }

    #[test]
    fn unique_by_name_refuses_duplicates() {
        let mut t = tree();
        assert_eq!(find_unique_by_name(&t, "Save").unwrap().ref_id, "e2");
        let mut dup = node("e9", UnifiedRole::Button);
        dup.name = Some("Save".into());
        t.children.push(dup);
        assert!(find_unique_by_name(&t, "Save").is_none());
        assert!(find_unique_by_name(&t, "Missing").is_none());
    }

    #[test]
    fn overlay_subtree_refs_collects_window_and_descendants() {
        let t = tree();
        let refs = overlay_subtree_refs(&t);
        assert_eq!(refs.len(), 2);
        assert!(refs.contains("e4") && refs.contains("e5"));
    }
}
