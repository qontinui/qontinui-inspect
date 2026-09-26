//! Qontinui Inspector — native accessibility inspector.
//!
//! A Tauri application providing FlaUInspect-style inspection of native apps
//! via the existing `qontinui_runner_lib::accessibility` API. The spec is plan
//! `scout-2026-04-16-native-accessibility-expansion` (qontinui-dev-notes/plans),
//! section "Phase 4 — Native app inspector UI".
//!
//! # Phase 4 status
//!
//! - [x] Crate compiles (`cargo check -p qontinui-inspect` passes).
//! - [x] Tauri window launches (800x600, title "Qontinui Inspector").
//! - [x] Three-mode selector UI (Hover / Focus / Selector).
//! - [x] Hover Mode — implemented end-to-end on Windows using cursor-position
//!   polling (not a global mouse hook). Polls `GetAsyncKeyState(VK_CONTROL)`
//!   every ~100ms; when Ctrl is held, emits `element-hovered` events.
//!   Linux (AT-SPI) and macOS (AX) hit-testing is not wired yet.
//! - [x] Property grid — reads `role`, `automation_id`, `class_name`, `state`,
//!   `bounds` from the cached `UnifiedNode`, plus the Show Selector result.
//! - [x] `tauri-plugin-store` persists `collapsed_sections`.
//! - [x] 4a Focus Tracking — `start_focus_tracking` / `stop_focus_tracking`
//!   emit `element-focused` (see `focus.rs` for where the events come from and
//!   how the focused node is resolved).
//! - [x] 4b.2 Show Selector — `get_selector_for_ref` returns the
//!   `native_accessibility` `query` step fragment that re-finds the element,
//!   with its match count against the cached snapshot (see `selector.rs`).
//! - [x] 4c In-target-app overlay — a transparent, click-through, always-on-top
//!   `overlay` window outlines elements on screen (see `overlay.rs`), in the
//!   same palette as the in-UI highlights: hover yellow (#eab308), selected
//!   blue (#3b82f6), focused / selector match green (#10b981). Unsupported
//!   on macOS (no `macos-private-api`) and on Linux Wayland sessions; those
//!   return `overlay unsupported on <os>: <reason>` and the in-UI
//!   highlighting remains.
//!
//! # Architecture note
//!
//! Consumes the runner's `AccessibilityManager` public API only
//! (`qontinui_runner_lib::accessibility`). Does NOT touch any adapter files,
//! matching the parallel-Phase-2 refactor constraint in the plan. Focus
//! tracking opens its own platform adapter through the public
//! `adapters::create_platform_adapter` factory, as an event source only.

pub mod focus;
pub mod overlay;
pub mod selector;
pub mod tree;

use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use tracing::{info, warn};

use qontinui_runner_lib::accessibility::{
    model::{UnifiedBounds, UnifiedNode, UnifiedRole, UnifiedState},
    traits::ConnectionTarget,
    AccessibilityManager,
};

use crate::overlay::{OverlayDraw, OverlayKind, OverlayState};
use crate::selector::SelectorInfo;
use crate::tree::find_deepest_at;

// -----------------------------------------------------------------------------
// Shared state
// -----------------------------------------------------------------------------

/// Shared accessibility manager + mode control.
///
/// Tauri stores this via `app.manage(InspectorState::new())`.
pub struct InspectorState {
    /// The accessibility manager used for all tree captures and lookups.
    manager: Mutex<AccessibilityManager>,

    /// When `true`, the hover loop task polls cursor position + Ctrl key.
    /// Setting to `false` lets the spawned task exit on its next tick.
    hover_active: Arc<std::sync::atomic::AtomicBool>,

    /// The running focus-tracking task, if any. Stopping signals it and waits
    /// for it to release its event adapter.
    focus_task: Mutex<Option<focus::FocusTask>>,

    /// Last overlay draw, pulled by the overlay page when it loads.
    overlay: OverlayState,
}

impl InspectorState {
    pub fn new() -> Self {
        Self {
            manager: Mutex::new(AccessibilityManager::new()),
            hover_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            focus_task: Mutex::new(None),
            overlay: OverlayState::default(),
        }
    }
}

impl Default for InspectorState {
    fn default() -> Self {
        Self::new()
    }
}

// -----------------------------------------------------------------------------
// Property grid
// -----------------------------------------------------------------------------

/// Serializable snapshot of a `UnifiedNode`'s inspect-relevant fields.
///
/// Mirrors the property-grid sections shown by `ui-bridge/src/debug/inspector.tsx`
/// (identifier / state / bounds) for UX parity with the web inspector. It is
/// the payload of both `element-hovered` and `element-focused`.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct PropertyGrid {
    pub ref_id: String,
    pub role: String,
    pub name: Option<String>,
    pub value: Option<String>,
    pub automation_id: Option<String>,
    pub class_name: Option<String>,
    pub html_tag: Option<String>,
    pub bounds: Option<UnifiedBounds>,
    pub state: UnifiedState,
    pub is_interactive: bool,
    /// Show Selector result — the same value `get_selector_for_ref` returns.
    /// `None` only for a focus event that could not be resolved to a node.
    pub selector: Option<SelectorInfo>,
    /// Set when the grid was not read from a captured node (an unresolved
    /// focus event); says why.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

impl PropertyGrid {
    /// Build the grid for `node`, computing its selector against `root` (the
    /// snapshot root the node was found in).
    pub fn from_node(node: &UnifiedNode, root: &UnifiedNode) -> Self {
        Self {
            ref_id: node.ref_id.clone(),
            role: node.role.as_str().to_string(),
            name: node.name.clone(),
            value: node.value.clone(),
            automation_id: node.automation_id.clone(),
            class_name: node.class_name.clone(),
            html_tag: node.html_tag.clone(),
            bounds: node.bounds.clone(),
            state: node.state.clone(),
            is_interactive: node.is_interactive,
            selector: Some(selector::selector_for(node, root)),
            note: None,
        }
    }
}

// -----------------------------------------------------------------------------
// Tauri commands
// -----------------------------------------------------------------------------

#[tauri::command]
async fn get_backend_name(state: tauri::State<'_, InspectorState>) -> Result<String, String> {
    let mgr = state.manager.lock().await;
    Ok(mgr.backend_name().to_string())
}

#[tauri::command]
async fn capture_desktop(state: tauri::State<'_, InspectorState>) -> Result<u32, String> {
    let mut mgr = state.manager.lock().await;
    if !mgr.is_connected() {
        mgr.connect(ConnectionTarget::Desktop, 5000)
            .await
            .map_err(|e| format!("connect failed: {}", e))?;
    }
    let snap = mgr
        .capture(None, false)
        .await
        .map_err(|e| format!("capture failed: {}", e))?;
    Ok(snap.total_nodes)
}

/// Start Hover Mode — implemented fully on Windows, no-op on other platforms.
///
/// Spawns a tokio task that polls `GetAsyncKeyState(VK_CONTROL)` every ~100ms.
/// While Ctrl is held, reads `GetCursorPos`, walks the cached tree to find the
/// deepest node whose bounds contain the cursor, and emits `element-hovered`
/// with a `PropertyGrid` payload.
///
/// Rationale for polling vs. global hook: low-level mouse/keyboard hooks
/// (`SetWindowsHookExW` with `WH_MOUSE_LL` / `WH_KEYBOARD_LL`) require a
/// dedicated message-pumping thread and are invasive in ways the scaffold
/// shouldn't pay for yet. Polling at 10 Hz is plenty for an inspector.
#[tauri::command]
async fn start_hover_mode(
    app: tauri::AppHandle,
    state: tauri::State<'_, InspectorState>,
) -> Result<(), String> {
    state
        .hover_active
        .store(true, std::sync::atomic::Ordering::Relaxed);
    let flag = state.hover_active.clone();

    #[cfg(windows)]
    {
        // Ensure we have a captured tree to walk against.
        let mut mgr = state.manager.lock().await;
        if !mgr.is_connected() {
            if let Err(e) = mgr.connect(ConnectionTarget::Desktop, 5000).await {
                return Err(format!("connect failed: {}", e));
            }
        }
        if mgr.snapshot().await.is_none() {
            if let Err(e) = mgr.capture(None, false).await {
                warn!("initial capture failed: {}", e);
            }
        }
        drop(mgr);

        // Clone what the task needs. We can't send `tauri::State` into the task,
        // but we can pull the Arc<Mutex<AccessibilityManager>>-moral-equivalent
        // back out of the app's managed state inside the task.
        let app_handle = app.clone();
        tokio::spawn(async move {
            windows_hover_loop(app_handle, flag).await;
        });
        Ok(())
    }

    #[cfg(not(windows))]
    {
        warn!(
            "Hover Mode currently only implemented on Windows — see Phase 4 \
             plan for Linux (AT-SPI focus events) and macOS (AXUIElementCopy\
             ElementAtPosition) paths."
        );
        Ok(())
    }
}

#[tauri::command]
async fn stop_hover_mode(state: tauri::State<'_, InspectorState>) -> Result<(), String> {
    state
        .hover_active
        .store(false, std::sync::atomic::Ordering::Relaxed);
    Ok(())
}

/// Start Focus Tracking: emit `element-focused` (a `PropertyGrid`) whenever
/// keyboard focus moves. Restarts cleanly if already running. See `focus.rs`.
#[tauri::command]
async fn start_focus_tracking(
    app: tauri::AppHandle,
    state: tauri::State<'_, InspectorState>,
) -> Result<(), String> {
    let mut slot = state.focus_task.lock().await;
    if let Some(previous) = slot.take() {
        previous.stop().await;
    }
    *slot = Some(focus::spawn(app.clone()).await?);
    Ok(())
}

/// Stop Focus Tracking. A no-op when it is not running.
#[tauri::command]
async fn stop_focus_tracking(state: tauri::State<'_, InspectorState>) -> Result<(), String> {
    let task = state.focus_task.lock().await.take();
    if let Some(task) = task {
        task.stop().await;
    }
    Ok(())
}

/// Find `ref_id` (with or without its leading `@` — see `tree::normalize_ref`;
/// the ref manager's refs carry it) in the cached snapshot and
/// hand it, with the snapshot root, to `f`.
async fn with_cached_node<T>(
    state: &InspectorState,
    ref_id: &str,
    f: impl FnOnce(&UnifiedNode, &UnifiedNode) -> T,
) -> Result<T, String> {
    let mgr = state.manager.lock().await;
    let snap = mgr
        .snapshot()
        .await
        .ok_or_else(|| "no tree captured yet — call capture_desktop first".to_string())?;
    let node = snap
        .root
        .find_by_ref(&tree::normalize_ref(ref_id))
        .ok_or_else(|| format!("ref not found: {}", ref_id))?;
    Ok(f(node, &snap.root))
}

/// Show Selector: the `native_accessibility` `query` step fragment that
/// re-finds the element, plus how many cached nodes it matches, e.g.
/// `{ "step": {"a11y_action": "query", "a11y_query_automation_id": "btn_ok"},
///    "strategy": "automation_id", "match_count": 1, "unique": true,
///    "session_ref": "@e3", "session_ref_note": "..." }`. See `selector.rs`.
#[tauri::command]
async fn get_selector_for_ref(
    ref_id: String,
    state: tauri::State<'_, InspectorState>,
) -> Result<SelectorInfo, String> {
    with_cached_node(&state, &ref_id, selector::selector_for).await
}

/// Return a property-grid snapshot for the node identified by `ref_id`.
#[tauri::command]
async fn get_property_grid(
    ref_id: String,
    state: tauri::State<'_, InspectorState>,
) -> Result<PropertyGrid, String> {
    with_cached_node(&state, &ref_id, PropertyGrid::from_node).await
}

// -----------------------------------------------------------------------------
// In-target-app overlay (Phase 4c)
// -----------------------------------------------------------------------------

/// Outline `bounds` (screen coordinates, one or more rects) on screen in
/// `kind`'s colour. Errors with `overlay unsupported on <os>: <reason>` on a
/// platform that cannot do it.
#[tauri::command]
async fn show_overlay(
    bounds: Vec<UnifiedBounds>,
    kind: OverlayKind,
    app: tauri::AppHandle,
    state: tauri::State<'_, InspectorState>,
) -> Result<OverlayDraw, String> {
    overlay::show(&app, &state.overlay, &bounds, kind)
}

/// Hide the overlay.
#[tauri::command]
async fn hide_overlay(
    app: tauri::AppHandle,
    state: tauri::State<'_, InspectorState>,
) -> Result<(), String> {
    overlay::hide(&app, &state.overlay)
}

/// What the overlay should currently draw — pulled by `overlay.html` on load,
/// so a draw emitted before the page was listening is not lost.
#[tauri::command]
async fn get_overlay_state(
    state: tauri::State<'_, InspectorState>,
) -> Result<Option<OverlayDraw>, String> {
    Ok(state.overlay.last())
}

/// Result of `show_selector_matches`.
#[derive(Debug, Clone, serde::Serialize)]
struct SelectorMatches {
    selector: SelectorInfo,
    /// Refs of every match (session refs, for display only).
    match_refs: Vec<String>,
    /// How many matches had bounds and were outlined.
    drawn: usize,
    /// Why nothing was drawn on screen, when it was not (unsupported platform
    /// or no bounds). The match data above is valid either way.
    overlay_error: Option<String>,
}

/// "Show matches": run the element's selector against the cached snapshot and
/// outline every match on screen in green.
#[tauri::command]
async fn show_selector_matches(
    ref_id: String,
    app: tauri::AppHandle,
    state: tauri::State<'_, InspectorState>,
) -> Result<SelectorMatches, String> {
    let (info, refs, rects) = with_cached_node(&state, &ref_id, |node, root| {
        let info = selector::selector_for(node, root);
        let hits = selector::find_matches(&info.step, root).unwrap_or_default();
        let refs = hits
            .iter()
            .map(|n| tree::normalize_ref(&n.ref_id))
            .collect();
        let rects = tree::bounds_of(&hits);
        (info, refs, rects)
    })
    .await?;
    let drawn = rects.iter().filter(|b| b.width > 0 && b.height > 0).count();
    let overlay_error = overlay::show(&app, &state.overlay, &rects, OverlayKind::Match).err();
    Ok(SelectorMatches {
        selector: info,
        match_refs: refs,
        drawn: if overlay_error.is_some() { 0 } else { drawn },
        overlay_error,
    })
}

/// Persist the list of property-grid section IDs currently collapsed, via
/// `tauri-plugin-store`. Stored in `inspect-settings.dat`.
#[tauri::command]
async fn save_collapse_state(sections: Vec<String>, app: tauri::AppHandle) -> Result<(), String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store("inspect-settings.dat")
        .map_err(|e| format!("store open failed: {}", e))?;
    store.set(
        "collapsed_sections",
        serde_json::Value::Array(
            sections
                .into_iter()
                .map(serde_json::Value::String)
                .collect(),
        ),
    );
    store
        .save()
        .map_err(|e| format!("store save failed: {}", e))?;
    Ok(())
}

#[tauri::command]
async fn load_collapse_state(app: tauri::AppHandle) -> Result<Vec<String>, String> {
    use tauri_plugin_store::StoreExt;
    let store = app
        .store("inspect-settings.dat")
        .map_err(|e| format!("store open failed: {}", e))?;
    let value = store.get("collapsed_sections");
    let sections: Vec<String> = match value {
        Some(v) => serde_json::from_value(v).unwrap_or_default(),
        None => Vec::new(),
    };
    Ok(sections)
}

// -----------------------------------------------------------------------------
// Windows hover loop (polling)
// -----------------------------------------------------------------------------

#[cfg(windows)]
async fn windows_hover_loop(
    app: tauri::AppHandle,
    hover_active: Arc<std::sync::atomic::AtomicBool>,
) {
    use std::sync::atomic::Ordering;
    use windows::Win32::Foundation::POINT;
    use windows::Win32::UI::Input::KeyboardAndMouse::{GetAsyncKeyState, VK_CONTROL};
    use windows::Win32::UI::WindowsAndMessaging::GetCursorPos;

    let mut last_ref: Option<String> = None;
    let mut ticks_since_capture: u32 = 0;

    while hover_active.load(Ordering::Relaxed) {
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;

        // Ctrl gating — high-order bit set means key is currently down.
        let ctrl_down = unsafe { GetAsyncKeyState(VK_CONTROL.0 as i32) as u16 & 0x8000 != 0 };
        if !ctrl_down {
            continue;
        }

        let mut pt = POINT { x: 0, y: 0 };
        if unsafe { GetCursorPos(&mut pt) }.is_err() {
            continue;
        }

        // Refresh the tree every ~2s while hovering, so we can see newly
        // appearing elements. Not cheap, but it's the scaffold.
        ticks_since_capture += 1;
        if ticks_since_capture >= 20 {
            ticks_since_capture = 0;
            if let Some(state) = app.try_state::<InspectorState>() {
                let mut mgr = state.manager.lock().await;
                let _ = mgr.capture(None, false).await;
            }
        }

        // Walk the cached tree.
        let grid_opt = if let Some(state) = app.try_state::<InspectorState>() {
            let mgr = state.manager.lock().await;
            let snap = mgr.snapshot().await;
            snap.and_then(|s| {
                find_deepest_at(&s.root, pt.x, pt.y).map(|n| PropertyGrid::from_node(n, &s.root))
            })
        } else {
            None
        };

        if let Some(grid) = grid_opt {
            if last_ref.as_deref() != Some(grid.ref_id.as_str()) {
                last_ref = Some(grid.ref_id.clone());
                if let Err(e) = app.emit("element-hovered", &grid) {
                    warn!("emit element-hovered failed: {}", e);
                }
            }
        }
    }

    info!("hover loop exited");
}

// -----------------------------------------------------------------------------
// Entry point
// -----------------------------------------------------------------------------

/// Initialize tracing and launch the Tauri app.
pub fn run() {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("info")),
        )
        .try_init();

    tauri::Builder::default()
        .plugin(tauri_plugin_store::Builder::new().build())
        .manage(InspectorState::new())
        .invoke_handler(tauri::generate_handler![
            get_backend_name,
            capture_desktop,
            start_hover_mode,
            stop_hover_mode,
            start_focus_tracking,
            stop_focus_tracking,
            get_selector_for_ref,
            get_property_grid,
            save_collapse_state,
            load_collapse_state,
            show_overlay,
            hide_overlay,
            get_overlay_state,
            show_selector_matches,
        ])
        .setup(|app| {
            info!("Qontinui Inspector starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running qontinui-inspect tauri application");
}
