//! Focus Tracking (plan Phase 4a).
//!
//! # Where focus events actually come from
//!
//! The brief for this mode is "subscribe to `AccessibilityManager::subscribe()`
//! and react to `A11yEvent::FocusChanged`". That subscription is held here, but
//! on its own it would never fire: as of this writing the manager's broadcast
//! channel carries only what the manager itself sends (`ConnectionChanged`,
//! `TreeReplaced`). Nothing forwards the platform adapter's
//! `PlatformAdapter::subscribe_events()` stream into it, and the manager's
//! adapter is private. So this task ALSO opens a dedicated platform adapter,
//! connected to the desktop, purely as an event source, and merges both
//! streams. If the runner later starts forwarding adapter events into the
//! manager's bus, the two sources may report the same change; the task
//! de-duplicates by resolved ref, so that is harmless.
//!
//! Where the platform adapter has no event stream (the macOS AX adapter
//! returns `None` today), the task falls back to polling: it re-captures every
//! [`POLL_INTERVAL`] and reports when the focused node changes.
//!
//! # Resolving the focused node
//!
//! `FocusChanged { ref_id, node_name }` rarely carries a usable ref: the UIA
//! handler sends an empty `ref_id` plus the element's name, and the AT-SPI
//! adapter sends the D-Bus object path, which is not a ref-manager ref. So:
//!
//! 1. a `ref_id` that resolves in the cached snapshot wins;
//! 2. otherwise the tree is re-captured and the deepest node whose
//!    `state.is_focused` is set is taken;
//! 3. failing that, the one node whose name equals the event's `node_name`
//!    (only when exactly one does — a guess between duplicates is wrong);
//! 4. failing that, `element-focused` is still emitted, built from what the
//!    event carries, with `ref_id` empty and `note` saying it is unresolved.
//!
//! Bursts of focus events (a dialog opening moves focus several times) are
//! debounced by [`DEBOUNCE`] so one re-capture serves the whole burst.

use std::time::Duration;

use tauri::{Emitter, Manager};
use tokio::sync::{broadcast, mpsc, oneshot};
use tokio::task::JoinHandle;
use tracing::{debug, info, warn};

use qontinui_runner_lib::accessibility::{
    adapters::create_platform_adapter,
    events::A11yEvent,
    model::{NodeSource, UnifiedNode, UnifiedRole, UnifiedState},
    traits::{ConnectionTarget, PlatformAdapter},
};

use crate::tree::{find_focused, find_unique_by_name, normalize_ref};
use crate::{InspectorState, PropertyGrid};

/// Tauri event emitted on each resolved focus change. Payload: `PropertyGrid`,
/// the same shape as `element-hovered`.
pub const ELEMENT_FOCUSED_EVENT: &str = "element-focused";

/// Quiet period after the last focus event before resolving.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Re-capture period when the platform offers no event stream.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// A running focus-tracking task and its stop signal.
pub struct FocusTask {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl FocusTask {
    /// Signal the task to stop and wait for it to release its event adapter.
    /// Falls back to aborting it if it does not exit promptly.
    pub async fn stop(self) {
        let _ = self.stop.send(());
        let mut handle = self.handle;
        if tokio::time::timeout(Duration::from_secs(3), &mut handle)
            .await
            .is_err()
        {
            warn!("focus task did not stop within 3s — aborting it");
            handle.abort();
        }
    }
}

/// What prompted a resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
enum FocusHint {
    Event {
        ref_id: String,
        node_name: Option<String>,
    },
    Poll,
}

/// Start the focus task. The caller must have stopped any previous one.
pub async fn spawn(app: tauri::AppHandle) -> Result<FocusTask, String> {
    let bus = {
        let state = app.state::<InspectorState>();
        let mut mgr = state.manager.lock().await;
        if !mgr.is_connected() {
            mgr.connect(ConnectionTarget::Desktop, 5000)
                .await
                .map_err(|e| format!("connect failed: {}", e))?;
        }
        if mgr.snapshot().await.is_none() {
            if let Err(e) = mgr.capture(None, false).await {
                warn!("initial capture for focus tracking failed: {}", e);
            }
        }
        mgr.subscribe()
    };

    let (event_adapter, adapter_rx) = open_event_adapter().await;
    let (stop_tx, stop_rx) = oneshot::channel();
    let handle = tokio::spawn(run(app, bus, event_adapter, adapter_rx, stop_rx));
    Ok(FocusTask {
        stop: stop_tx,
        handle,
    })
}

/// Open a platform adapter used only for its event stream.
async fn open_event_adapter() -> (
    Option<Box<dyn PlatformAdapter>>,
    Option<mpsc::Receiver<A11yEvent>>,
) {
    let mut adapter = create_platform_adapter();
    if let Err(e) = adapter.connect(ConnectionTarget::Desktop, 5000).await {
        warn!(
            "focus: event adapter ({}) failed to connect: {} — polling instead",
            adapter.backend_name(),
            e
        );
        return (None, None);
    }
    match adapter.subscribe_events().await {
        Ok(Some(rx)) => {
            info!(
                "focus: subscribed to {} focus events",
                adapter.backend_name()
            );
            (Some(adapter), Some(rx))
        }
        Ok(None) => {
            info!(
                "focus: {} adapter has no event stream — polling every {:?}",
                adapter.backend_name(),
                POLL_INTERVAL
            );
            let _ = adapter.disconnect().await;
            (None, None)
        }
        Err(e) => {
            warn!("focus: event subscription failed: {} — polling instead", e);
            let _ = adapter.disconnect().await;
            (None, None)
        }
    }
}

async fn run(
    app: tauri::AppHandle,
    mut bus: broadcast::Receiver<A11yEvent>,
    mut event_adapter: Option<Box<dyn PlatformAdapter>>,
    mut adapter_rx: Option<mpsc::Receiver<A11yEvent>>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let polling = adapter_rx.is_none();
    let mut bus_open = true;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: Option<FocusHint> = None;
    let mut deadline = tokio::time::Instant::now();
    let mut last_ref: Option<String> = None;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,

            ev = bus.recv(), if bus_open => match ev {
                Ok(A11yEvent::FocusChanged { ref_id, node_name }) => {
                    pending = Some(FocusHint::Event { ref_id, node_name });
                    deadline = tokio::time::Instant::now() + DEBOUNCE;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("focus: manager bus lagged by {}", n);
                }
                Err(broadcast::error::RecvError::Closed) => bus_open = false,
            },

            ev = recv_opt(&mut adapter_rx), if adapter_rx.is_some() => match ev {
                Some(A11yEvent::FocusChanged { ref_id, node_name }) => {
                    pending = Some(FocusHint::Event { ref_id, node_name });
                    deadline = tokio::time::Instant::now() + DEBOUNCE;
                }
                Some(_) => {}
                None => {
                    warn!("focus: platform event stream ended");
                    adapter_rx = None;
                }
            },

            _ = poll.tick(), if polling && pending.is_none() => {
                pending = Some(FocusHint::Poll);
                deadline = tokio::time::Instant::now();
            }

            _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                if let Some(hint) = pending.take() {
                    if let Some(grid) = resolve(&app, &hint).await {
                        let same = !grid.ref_id.is_empty()
                            && last_ref.as_deref() == Some(grid.ref_id.as_str());
                        if !same {
                            last_ref = Some(grid.ref_id.clone());
                            if let Err(e) = app.emit(ELEMENT_FOCUSED_EVENT, &grid) {
                                warn!("emit {} failed: {}", ELEMENT_FOCUSED_EVENT, e);
                            }
                        }
                    }
                }
            }
        }
    }

    if let Some(mut adapter) = event_adapter.take() {
        drop(adapter_rx);
        if let Err(e) = adapter.disconnect().await {
            debug!("focus: event adapter disconnect: {}", e);
        }
    }
    info!("focus tracking stopped");
}

async fn recv_opt(rx: &mut Option<mpsc::Receiver<A11yEvent>>) -> Option<A11yEvent> {
    match rx {
        Some(rx) => rx.recv().await,
        None => std::future::pending().await,
    }
}

/// Resolve a hint to a property grid; see the module docs for the order.
async fn resolve(app: &tauri::AppHandle, hint: &FocusHint) -> Option<PropertyGrid> {
    let state = app.try_state::<InspectorState>()?;
    let mut mgr = state.manager.lock().await;

    if let FocusHint::Event { ref_id, .. } = hint {
        if !ref_id.trim().trim_start_matches('@').is_empty() {
            if let Some(snap) = mgr.snapshot().await {
                if let Some(node) = snap.root.find_by_ref(&normalize_ref(ref_id)) {
                    return Some(PropertyGrid::from_node(node, &snap.root));
                }
            }
        }
    }

    let snap = match mgr.capture(None, false).await {
        Ok(s) => s,
        Err(e) => {
            warn!("focus: re-capture failed: {}", e);
            return match hint {
                FocusHint::Event { ref_id, node_name } => Some(unresolved_grid(
                    ref_id,
                    node_name.as_deref(),
                    "re-capture failed",
                )),
                FocusHint::Poll => None,
            };
        }
    };
    if let Some(node) = find_focused(&snap.root) {
        return Some(PropertyGrid::from_node(node, &snap.root));
    }
    match hint {
        FocusHint::Event { ref_id, node_name } => {
            if let Some(name) = node_name.as_deref() {
                if let Some(node) = find_unique_by_name(&snap.root, name) {
                    return Some(PropertyGrid::from_node(node, &snap.root));
                }
            }
            Some(unresolved_grid(
                ref_id,
                node_name.as_deref(),
                "focused element not found in a fresh capture",
            ))
        }
        FocusHint::Poll => None,
    }
}

/// A grid built only from what a `FocusChanged` event carried.
fn unresolved_grid(event_ref: &str, node_name: Option<&str>, why: &str) -> PropertyGrid {
    let node = UnifiedNode {
        ref_id: String::new(),
        role: UnifiedRole::Unknown,
        name: node_name.map(str::to_string),
        value: None,
        description: None,
        bounds: None,
        state: UnifiedState {
            is_focused: true,
            ..UnifiedState::default()
        },
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
    };
    let mut grid = PropertyGrid::from_node(&node, &node);
    // A selector for a node that is not in the tree would be computed against
    // nothing and report itself as trivially unique — omit it instead.
    grid.selector = None;
    grid.note = Some(if event_ref.is_empty() {
        format!("unresolved focus event: {}", why)
    } else {
        format!("unresolved focus event (source id {}): {}", event_ref, why)
    });
    grid
}
