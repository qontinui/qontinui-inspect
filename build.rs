/// Every `#[tauri::command]` registered in `lib.rs`'s `generate_handler!`.
///
/// Declaring an app manifest puts ALL app commands under Tauri's ACL (without
/// one, any window may invoke any app command). Each listed command gets a
/// generated `allow-<command>` permission that a capability must grant, which
/// is how `capabilities/overlay.json` limits the overlay window to
/// `get_overlay_state`. A command registered in `lib.rs` but missing here is
/// refused at runtime for every window — keep the two lists in step.
const APP_COMMANDS: &[&str] = &[
    "get_backend_name",
    "capture_desktop",
    "start_hover_mode",
    "stop_hover_mode",
    "start_focus_tracking",
    "stop_focus_tracking",
    "get_selector_for_ref",
    "get_property_grid",
    "save_collapse_state",
    "load_collapse_state",
    "show_overlay",
    "hide_overlay",
    "get_overlay_state",
    "show_selector_matches",
];

fn main() {
    tauri_build::try_build(
        tauri_build::Attributes::new()
            .app_manifest(tauri_build::AppManifest::new().commands(APP_COMMANDS)),
    )
    .expect("failed to run tauri-build");
}
