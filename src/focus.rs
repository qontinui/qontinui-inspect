//! Focus Tracking (plan Phase 4a).
//!
//! # Where focus events come from
//!
//! The task consumes `AccessibilityManager::subscribe()` only and reacts to
//! `A11yEvent::FocusChanged`. The manager forwards its native adapter's
//! `PlatformAdapter::subscribe_events()` stream into that bus (UIA focus-changed
//! handler on Windows, AT-SPI `Event.Focus` on Linux), so no second adapter is
//! opened here.
//!
//! Where the connected adapter has no event stream
//! (`AccessibilityManager::has_native_events()` is `false` — the macOS AX and
//! JAB adapters today, or a stream that ended), the task falls back to
//! polling: it re-captures every [`POLL_INTERVAL`] and reports when the focused
//! node changes. The check is made on every tick rather than once at start, so
//! a reconnect elsewhere in the inspector (to a target whose adapter does or
//! does not stream) switches between the two modes by itself.
//!
//! # Resolving the focused node
//!
//! `FocusChanged { ref_id, node_name }` rarely carries a usable ref: the UIA
//! handler sends an empty `ref_id` plus the element's name, and the AT-SPI
//! adapter sends the D-Bus object path, which is not a ref-manager ref. So,
//! first against the CURRENT cached snapshot, with no capture:
//!
//! 1. a `ref_id` that resolves in the cached snapshot wins;
//! 2. otherwise the one cached node whose name equals the event's `node_name`
//!    (only when exactly one does — a guess between duplicates is wrong).
//!
//! The cached `state.is_focused` flags are NOT consulted: they describe focus
//! at capture time, which is exactly what just changed. Only when the cache
//! cannot answer is the desktop re-captured, and then:
//!
//! 3. the deepest node whose `state.is_focused` is set is taken;
//! 4. failing that, the unique-name match against the fresh tree;
//! 5. failing that, `element-focused` is still emitted, built from what the
//!    event carries, with `ref_id` empty and `note` saying it is unresolved.
//!
//! A re-capture walks the whole desktop under the manager lock, so they are
//! rate-limited to one per [`MIN_RECAPTURE_INTERVAL`]: a hint that needs one
//! sooner is kept and retried when the interval has passed (a newer hint
//! replaces it meanwhile). Bursts of focus events (a dialog opening moves
//! focus several times) are also debounced by [`DEBOUNCE`].
//!
//! # Deduplication
//!
//! Refs are renumbered on every capture, so "same element as last time" is
//! decided on [`NodeIdentity`] (role, automation id, class name, bounds), not
//! on the ref — otherwise every re-capture would re-emit the same element.
//!
//! `has_native_events()` is re-read at most every [`NATIVE_EVENTS_RECHECK`],
//! and immediately after a `ConnectionChanged` event, rather than locking the
//! manager on every poll tick.

use std::time::Duration;

use tauri::{Emitter, Manager};
use tokio::sync::{broadcast, oneshot};
use tokio::task::JoinHandle;
use tokio::time::Instant;
use tracing::{debug, info, warn};

use qontinui_runner_lib::accessibility::{
    events::A11yEvent,
    model::{NodeSource, UnifiedNode, UnifiedRole, UnifiedState},
    traits::ConnectionTarget,
};

use crate::tree::{
    find_focused, find_unique_by_name, is_new_identity, normalize_ref, NodeIdentity,
};
use crate::{InspectorState, PropertyGrid};

/// Tauri event emitted on each resolved focus change. Payload: `PropertyGrid`,
/// the same shape as `element-hovered`.
pub const ELEMENT_FOCUSED_EVENT: &str = "element-focused";

/// Quiet period after the last focus event before resolving.
pub const DEBOUNCE: Duration = Duration::from_millis(150);

/// Re-capture period when the connected adapter offers no event stream.
pub const POLL_INTERVAL: Duration = Duration::from_millis(1000);

/// Minimum spacing between desktop re-captures made by the focus task.
pub const MIN_RECAPTURE_INTERVAL: Duration = Duration::from_millis(1000);

/// How long a `has_native_events()` reading is trusted before it is re-read.
pub const NATIVE_EVENTS_RECHECK: Duration = Duration::from_secs(5);

/// A running focus-tracking task and its stop signal.
pub struct FocusTask {
    stop: oneshot::Sender<()>,
    handle: JoinHandle<()>,
}

impl FocusTask {
    /// Signal the task to stop and wait for it to exit. Falls back to aborting
    /// it if it does not exit promptly (e.g. it is waiting on the manager lock
    /// behind a slow capture).
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
        if mgr.has_native_events() {
            info!("focus: using {} focus events", mgr.backend_name());
        } else {
            info!(
                "focus: {} adapter has no event stream — polling every {:?}",
                mgr.backend_name(),
                POLL_INTERVAL
            );
        }
        mgr.subscribe()
    };

    let (stop_tx, stop_rx) = oneshot::channel();
    let handle = tokio::spawn(run(app, bus, stop_rx));
    Ok(FocusTask {
        stop: stop_tx,
        handle,
    })
}

/// Whether a poll tick should re-capture: connected, but with no live native
/// event stream to wait on.
async fn should_poll(app: &tauri::AppHandle) -> bool {
    let Some(state) = app.try_state::<InspectorState>() else {
        return false;
    };
    let mgr = state.manager.lock().await;
    mgr.is_connected() && !mgr.has_native_events()
}

/// How long to wait before a re-capture is allowed, or `None` when one may run
/// now. Pure so the rate limit is testable.
pub fn recapture_wait(
    last_capture: Option<Instant>,
    now: Instant,
    min_interval: Duration,
) -> Option<Duration> {
    let elapsed = now.saturating_duration_since(last_capture?);
    min_interval.checked_sub(elapsed).filter(|d| !d.is_zero())
}

/// A resolved focus report: the grid to emit and, when it was read from a
/// node, that node's identity for deduplication.
struct Resolved {
    grid: PropertyGrid,
    identity: Option<NodeIdentity>,
}

impl Resolved {
    fn from_node(node: &UnifiedNode, root: &UnifiedNode, generation: u64) -> Self {
        Self {
            grid: PropertyGrid::from_node(node, root, generation),
            identity: Some(NodeIdentity::of(node)),
        }
    }

    fn unresolved(event_ref: &str, node_name: Option<&str>, why: &str) -> Self {
        Self {
            grid: unresolved_grid(event_ref, node_name, why),
            identity: None,
        }
    }
}

async fn run(
    app: tauri::AppHandle,
    mut bus: broadcast::Receiver<A11yEvent>,
    mut stop_rx: oneshot::Receiver<()>,
) {
    let mut bus_open = true;
    let mut poll = tokio::time::interval(POLL_INTERVAL);
    poll.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    let mut pending: Option<FocusHint> = None;
    let mut deadline = Instant::now();
    let mut last_identity: Option<NodeIdentity> = None;
    let mut last_capture: Option<Instant> = None;
    // (should poll, when read) — see NATIVE_EVENTS_RECHECK.
    let mut poll_mode: Option<(bool, Instant)> = None;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,

            ev = bus.recv(), if bus_open => match ev {
                Ok(A11yEvent::FocusChanged { ref_id, node_name }) => {
                    pending = Some(FocusHint::Event { ref_id, node_name });
                    deadline = Instant::now() + DEBOUNCE;
                }
                Ok(A11yEvent::ConnectionChanged { .. }) => {
                    // The adapter (and whether it streams) may have changed.
                    poll_mode = None;
                }
                Ok(_) => {}
                Err(broadcast::error::RecvError::Lagged(n)) => {
                    debug!("focus: manager bus lagged by {}", n);
                }
                Err(broadcast::error::RecvError::Closed) => {
                    warn!("focus: manager event bus closed");
                    bus_open = false;
                }
            },

            _ = poll.tick(), if pending.is_none() => {
                let now = Instant::now();
                let stale = poll_mode
                    .is_none_or(|(_, at)| now.saturating_duration_since(at) >= NATIVE_EVENTS_RECHECK);
                if stale {
                    poll_mode = Some((should_poll(&app).await, now));
                }
                if poll_mode.is_some_and(|(should, _)| should) {
                    pending = Some(FocusHint::Poll);
                    deadline = now;
                }
            }

            _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                let Some(hint) = pending.take() else { continue };
                let mut resolved = resolve_cached(&app, &hint).await;
                if resolved.is_none() {
                    let now = Instant::now();
                    if let Some(wait) = recapture_wait(last_capture, now, MIN_RECAPTURE_INTERVAL) {
                        // Rate-limited: keep the hint, retry once allowed.
                        pending = Some(hint);
                        deadline = now + wait;
                        continue;
                    }
                    last_capture = Some(now);
                    resolved = resolve_by_capture(&app, &hint).await;
                }
                if let Some(r) = resolved {
                    if is_new_identity(last_identity.as_ref(), r.identity.as_ref()) {
                        last_identity = r.identity;
                        if let Err(e) = app.emit(ELEMENT_FOCUSED_EVENT, &r.grid) {
                            warn!("emit {} failed: {}", ELEMENT_FOCUSED_EVENT, e);
                        }
                    }
                }
            }
        }
    }

    info!("focus tracking stopped");
}

/// Resolve an event hint against `root` alone (steps 1-2 of the module docs).
/// Pure so the cache-first order is testable.
pub fn resolve_in_snapshot<'a>(
    ref_id: &str,
    node_name: Option<&str>,
    root: &'a UnifiedNode,
) -> Option<&'a UnifiedNode> {
    if !ref_id.trim().trim_start_matches('@').is_empty() {
        if let Some(node) = root.find_by_ref(&normalize_ref(ref_id)) {
            return Some(node);
        }
    }
    node_name.and_then(|name| find_unique_by_name(root, name))
}

/// Steps 1-2: resolve from the cached snapshot, no capture. `None` for a poll
/// hint (polling exists to see changes the cache cannot show) or when the
/// cache cannot answer.
async fn resolve_cached(app: &tauri::AppHandle, hint: &FocusHint) -> Option<Resolved> {
    let FocusHint::Event { ref_id, node_name } = hint else {
        return None;
    };
    let state = app.try_state::<InspectorState>()?;
    let mgr = state.manager.lock().await;
    let snap = mgr.snapshot().await?;
    resolve_in_snapshot(ref_id, node_name.as_deref(), &snap.root)
        .map(|node| Resolved::from_node(node, &snap.root, snap.generation))
}

/// Steps 3-5: re-capture and resolve against the fresh tree.
async fn resolve_by_capture(app: &tauri::AppHandle, hint: &FocusHint) -> Option<Resolved> {
    let state = app.try_state::<InspectorState>()?;
    let mut mgr = state.manager.lock().await;

    let snap = match mgr.capture(None, false).await {
        Ok(s) => s,
        Err(e) => {
            warn!("focus: re-capture failed: {}", e);
            return match hint {
                FocusHint::Event { ref_id, node_name } => Some(Resolved::unresolved(
                    ref_id,
                    node_name.as_deref(),
                    "re-capture failed",
                )),
                FocusHint::Poll => None,
            };
        }
    };
    if let Some(node) = find_focused(&snap.root) {
        return Some(Resolved::from_node(node, &snap.root, snap.generation));
    }
    match hint {
        FocusHint::Event { ref_id, node_name } => {
            if let Some(name) = node_name.as_deref() {
                if let Some(node) = find_unique_by_name(&snap.root, name) {
                    return Some(Resolved::from_node(node, &snap.root, snap.generation));
                }
            }
            Some(Resolved::unresolved(
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
    let mut grid = PropertyGrid::from_node(&node, &node, 0);
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::test_support::node;

    fn tree() -> UnifiedNode {
        let mut root = node("@e1", UnifiedRole::Window);
        let mut save = node("@e2", UnifiedRole::Button);
        save.name = Some("Save".into());
        let mut ok_a = node("@e3", UnifiedRole::Button);
        ok_a.name = Some("OK".into());
        let mut ok_b = node("@e4", UnifiedRole::Button);
        ok_b.name = Some("OK".into());
        root.children = vec![save, ok_a, ok_b];
        root
    }

    #[test]
    fn cached_resolution_prefers_a_resolvable_ref() {
        let t = tree();
        let hit = resolve_in_snapshot("e3", Some("Save"), &t).unwrap();
        assert_eq!(hit.ref_id, "@e3");
    }

    #[test]
    fn cached_resolution_falls_back_to_a_unique_name() {
        let t = tree();
        // AT-SPI object path: not a ref.
        let hit = resolve_in_snapshot("/org/a11y/atspi/accessible/42", Some("Save"), &t).unwrap();
        assert_eq!(hit.ref_id, "@e2");
        assert_eq!(
            resolve_in_snapshot("", Some("Save"), &t).unwrap().ref_id,
            "@e2"
        );
    }

    #[test]
    fn cached_resolution_refuses_duplicate_or_missing_names() {
        let t = tree();
        assert!(resolve_in_snapshot("", Some("OK"), &t).is_none());
        assert!(resolve_in_snapshot("", Some("Missing"), &t).is_none());
        assert!(resolve_in_snapshot("", None, &t).is_none());
    }

    #[test]
    fn recapture_is_rate_limited() {
        let t0 = Instant::now();
        let min = Duration::from_millis(1000);
        assert_eq!(recapture_wait(None, t0, min), None);
        assert_eq!(
            recapture_wait(Some(t0), t0 + Duration::from_millis(300), min),
            Some(Duration::from_millis(700))
        );
        assert_eq!(recapture_wait(Some(t0), t0 + min, min), None);
        assert_eq!(
            recapture_wait(Some(t0), t0 + Duration::from_secs(5), min),
            None
        );
    }
}
