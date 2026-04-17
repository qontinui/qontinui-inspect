//! Qontinui Inspector — Phase 4 scaffold.
//!
//! A Tauri application providing FlaUInspect-style inspection of native apps
//! via the existing `qontinui_runner_lib::accessibility` API. This is the
//! Phase 4 scaffold, not the full implementation — see
//! `D:/qontinui-root/plans/scout-2026-04-16-native-accessibility-expansion.md`
//! section "Phase 4 — Native app inspector UI" for the complete spec.
//!
//! # Scope of this scaffold
//!
//! - [x] Crate compiles (`cargo check -p qontinui-inspect` passes).
//! - [x] Tauri window launches (800x600, title "Qontinui Inspector").
//! - [x] Three-mode selector UI (Hover / Focus / Selector).
//! - [x] Hover Mode — implemented end-to-end on Windows using cursor-position
//!       polling (not a global mouse hook). Polls `GetAsyncKeyState(VK_CONTROL)`
//!       every ~100ms; when Ctrl is held, emits `element-hovered` events.
//! - [x] Property grid — reads `role`, `automation_id`, `class_name`, `state`,
//!       `bounds` from the cached `UnifiedNode`.
//! - [x] `tauri-plugin-store` persists `collapsed_sections`.
//! - [ ] Focus Tracking — stub only (see `start_focus_tracking`).
//! - [ ] Show Selector — placeholder `@<ref_id>` (see `get_selector_for_ref`).
//! - [ ] In-target-app overlay drawing (the Phase 4 novel piece — transparent
//!       overlay window + GDI paint loop) is deferred. For now, highlight
//!       colors render only in the inspector's own UI:
//!           hover    = yellow  (#eab308)
//!           selected = blue    (#3b82f6)
//!           focused  = green   (#10b981)  -- reserved, not yet emitted
//!
//! # Architecture note
//!
//! Consumes the runner's `AccessibilityManager` public API only
//! (`qontinui_runner_lib::accessibility`). Does NOT touch any adapter files,
//! matching the parallel-Phase-2 refactor constraint in the plan.

use std::sync::Arc;
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use tracing::{info, warn};

use qontinui_runner_lib::accessibility::{
    model::{UnifiedBounds, UnifiedNode, UnifiedRole, UnifiedState},
    traits::ConnectionTarget,
    AccessibilityManager,
};

// -----------------------------------------------------------------------------
// Shared state
// -----------------------------------------------------------------------------

/// Shared accessibility manager + hover-loop control flag.
///
/// Tauri stores this via `app.manage(InspectorState::new())`.
pub struct InspectorState {
    /// The accessibility manager used for all tree captures and lookups.
    manager: Mutex<AccessibilityManager>,

    /// When `true`, the hover loop task polls cursor position + Ctrl key.
    /// Setting to `false` lets the spawned task exit on its next tick.
    hover_active: Arc<std::sync::atomic::AtomicBool>,
}

impl InspectorState {
    pub fn new() -> Self {
        Self {
            manager: Mutex::new(AccessibilityManager::new()),
            hover_active: Arc::new(std::sync::atomic::AtomicBool::new(false)),
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
/// (identifier / state / bounds) for UX parity with the web inspector.
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
    /// Placeholder qontinui-selector. Phase 4 full scope will replace this
    /// with the real selector grammar (see `get_selector_for_ref` TODO).
    pub selector: String,
}

impl PropertyGrid {
    fn from_node(node: &UnifiedNode) -> Self {
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
            selector: format!("@{}", node.ref_id),
        }
    }
}

// -----------------------------------------------------------------------------
// Tree walk: find the deepest node whose bounds contain (x, y).
// -----------------------------------------------------------------------------

fn node_contains(node: &UnifiedNode, x: i32, y: i32) -> bool {
    match &node.bounds {
        Some(b) => x >= b.x && x < b.x + b.width && y >= b.y && y < b.y + b.height,
        None => false,
    }
}

fn find_deepest_at<'a>(node: &'a UnifiedNode, x: i32, y: i32) -> Option<&'a UnifiedNode> {
    if !node_contains(node, x, y) {
        return None;
    }
    // Prefer the deepest matching descendant.
    for child in &node.children {
        if let Some(hit) = find_deepest_at(child, x, y) {
            return Some(hit);
        }
    }
    Some(node)
}

// -----------------------------------------------------------------------------
// Tauri commands
// -----------------------------------------------------------------------------

#[tauri::command]
async fn get_backend_name(
    state: tauri::State<'_, InspectorState>,
) -> Result<String, String> {
    let mgr = state.manager.lock().await;
    Ok(mgr.backend_name().to_string())
}

#[tauri::command]
async fn capture_desktop(
    state: tauri::State<'_, InspectorState>,
) -> Result<u32, String> {
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

/// Scaffold — logs a warning and returns Ok. See plan phase 4 item 2 bullet 2:
/// requires subscribing to `UIA_AutomationFocusChangedEventId`, which needs
/// UIA event-sink glue that isn't wired through the current
/// `PlatformAdapter::subscribe_events` surface for focus-change events yet.
#[tauri::command]
async fn start_focus_tracking() -> Result<(), String> {
    warn!("Focus tracking not implemented yet — see plan phase 4");
    Ok(())
}

/// Scaffold — returns `@<ref_id>` as a placeholder. Full Phase 4 scope: emit
/// a qontinui-selector string (role, automation_id, ancestor chain, etc.)
/// matching the grammar the runner's selector engine speaks.
#[tauri::command]
async fn get_selector_for_ref(ref_id: String) -> Result<String, String> {
    // TODO(phase4): generate real qontinui-selector (role + automation_id +
    // ancestor chain with :nth-of-type disambiguation). See plan.
    Ok(format!("@{}", ref_id))
}

/// Return a property-grid snapshot for the node identified by `ref_id`.
#[tauri::command]
async fn get_property_grid(
    ref_id: String,
    state: tauri::State<'_, InspectorState>,
) -> Result<PropertyGrid, String> {
    let mgr = state.manager.lock().await;
    let snap = mgr
        .snapshot()
        .await
        .ok_or_else(|| "no tree captured yet — call capture_desktop first".to_string())?;
    let node = snap
        .root
        .find_by_ref(&ref_id)
        .ok_or_else(|| format!("ref not found: {}", ref_id))?;
    Ok(PropertyGrid::from_node(node))
}

/// Persist the list of property-grid section IDs currently collapsed, via
/// `tauri-plugin-store`. Stored in `inspect-settings.dat`.
#[tauri::command]
async fn save_collapse_state(
    sections: Vec<String>,
    app: tauri::AppHandle,
) -> Result<(), String> {
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
                find_deepest_at(&s.root, pt.x, pt.y).map(PropertyGrid::from_node)
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
            get_selector_for_ref,
            get_property_grid,
            save_collapse_state,
            load_collapse_state,
        ])
        .setup(|app| {
            info!("Qontinui Inspector starting");
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("error while running qontinui-inspect tauri application");
}
