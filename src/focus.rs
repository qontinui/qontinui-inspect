//! Focus Tracking (plan Phase 4a).
//!
//! # Where focus events come from
//!
//! The task consumes `AccessibilityManager::subscribe()` only and reacts to
//! `A11yEvent::FocusChanged`. The manager forwards its native adapter's
//! `PlatformAdapter::subscribe_events()` stream into that bus (UIA focus-changed
//! handler on Windows; on Linux AT-SPI `Event.Focus` and
//! `Object:StateChanged:focused`), so no second adapter is opened here.
//!
//! `has_native_events()` only says a stream is attached, not that it carries
//! focus: an AT-SPI toolkit bridge may emit nothing we match. So the task
//! polls — re-captures every [`POLL_INTERVAL`] and reports when the focused
//! node changes — whenever the connected adapter has no event stream (macOS
//! AX and JAB today, or a stream that ended) OR no `FocusChanged` has actually
//! arrived since the task started or the manager last (re)connected. The
//! first real focus event switches it to relying on events; a
//! `ConnectionChanged` switches it back to polling until the next one (see
//! [`poll_needed`]).
//!
//! # Resolving the focused node
//!
//! `FocusChanged { ref_id, node_name }` rarely carries a usable ref: the UIA
//! handler sends an empty `ref_id` plus the element's name, and the AT-SPI
//! adapter sends the D-Bus object path, which is not a ref-manager ref. So:
//!
//! 1. a `ref_id` that resolves in the CURRENT cached snapshot wins, with no
//!    capture — an exact ref is authoritative;
//! 2. anything else re-captures the desktop: a unique name match in the
//!    cached snapshot is only a hint (the cache predates the focus change, and
//!    the name may since belong to another element), so it is never emitted
//!    on its own.
//!
//! The cached `state.is_focused` flags are NOT consulted either: they describe
//! focus at capture time, which is exactly what just changed. Against the
//! fresh capture:
//!
//! 3. the deepest node whose `state.is_focused` is set is taken;
//! 4. failing that, the unique-name match against the fresh tree;
//! 5. failing that, `element-focused` is still emitted, built from what the
//!    event carries, with `ref_id` empty and `note` saying it is unresolved.
//!
//! A re-capture walks the whole desktop under the manager lock, so they are
//! rate-limited to one per [`MIN_RECAPTURE_INTERVAL`]: a hint that needs one
//! sooner is kept and retried when the interval has passed (a newer hint
//! replaces it meanwhile) — never answered from the cache instead
//! ([`next_step`]). Bursts of focus events (a dialog opening moves focus
//! several times) are also debounced by [`DEBOUNCE`].
//!
//! # The inspector's own window
//!
//! Clicking a button in the inspector moves keyboard focus into it. A
//! resolved node lying entirely inside the inspector's main window
//! (`tree::bounds_within` against the window's outer position and size) is
//! not reported, so using the inspector does not replace the grid it shows.
//!
//! # Deduplication
//!
//! Refs are renumbered on every capture, so "same element as last time" is
//! decided on [`NodeIdentity`] (role, name, automation id, class name,
//! bounds), not on the ref — otherwise every re-capture would re-emit the
//! same element.
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
    bounds_within, find_focused, find_unique_by_name, is_new_identity, normalize_ref, NodeIdentity,
    WindowRect,
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
            info!(
                "focus: {} event stream attached — polling until its first focus event",
                mgr.backend_name()
            );
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

/// `(connected, has_native_events)` of the manager; `(false, false)` when the
/// state is gone.
async fn read_event_source(app: &tauri::AppHandle) -> (bool, bool) {
    let Some(state) = app.try_state::<InspectorState>() else {
        return (false, false);
    };
    let mgr = state.manager.lock().await;
    (mgr.is_connected(), mgr.has_native_events())
}

/// Whether a poll tick should re-capture. Pure so the rule is testable.
///
/// Connected, and not provably receiving focus events: either no native
/// stream is attached, or one is but no `FocusChanged` has come through it
/// since the task started or the manager last (re)connected. An attached
/// stream alone is not trusted — a toolkit bridge may emit nothing it matches.
pub fn poll_needed(connected: bool, native_events: bool, focus_event_seen: bool) -> bool {
    connected && !(native_events && focus_event_seen)
}

/// What to do with a debounced hint.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NextStep {
    /// The cached snapshot resolved the hint's ref exactly: emit that.
    EmitCached,
    /// Re-capture now and resolve against the fresh tree.
    Recapture,
    /// A re-capture is needed but rate-limited: keep the hint and retry
    /// after this long.
    Defer(Duration),
}

/// Decide a hint's next step from whether the cache resolved its ref EXACTLY
/// and from the re-capture rate limit (`recapture_wait`). A cached name match
/// is not an exact hit — it is never emitted, even while the limiter blocks.
/// Pure so the rule is testable.
pub fn next_step(exact_cache_hit: bool, recapture_wait: Option<Duration>) -> NextStep {
    if exact_cache_hit {
        return NextStep::EmitCached;
    }
    match recapture_wait {
        Some(wait) => NextStep::Defer(wait),
        None => NextStep::Recapture,
    }
}

/// The inspector main window's outer rectangle, in screen physical pixels.
/// `None` when it is missing, hidden or minimized — nothing to exclude then.
fn main_window_rect(app: &tauri::AppHandle) -> Option<WindowRect> {
    let window = app.get_webview_window("main")?;
    if !window.is_visible().unwrap_or(false) || window.is_minimized().unwrap_or(false) {
        return None;
    }
    let pos = window.outer_position().ok()?;
    let size = window.outer_size().ok()?;
    Some((pos.x, pos.y, size.width, size.height))
}

/// Whether a resolved report is about an element of the inspector's own main
/// window (see the module docs).
fn is_inspector_own(app: &tauri::AppHandle, resolved: &Resolved) -> bool {
    match (&resolved.grid.bounds, main_window_rect(app)) {
        (Some(bounds), Some(window)) => bounds_within(bounds, window),
        _ => false,
    }
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
    // (connected, has_native_events, when read) — see NATIVE_EVENTS_RECHECK.
    let mut source: Option<(bool, bool, Instant)> = None;
    // Whether a FocusChanged has actually arrived since start / reconnect.
    let mut focus_event_seen = false;

    loop {
        tokio::select! {
            _ = &mut stop_rx => break,

            ev = bus.recv(), if bus_open => match ev {
                Ok(A11yEvent::FocusChanged { ref_id, node_name }) => {
                    focus_event_seen = true;
                    pending = Some(FocusHint::Event { ref_id, node_name });
                    deadline = Instant::now() + DEBOUNCE;
                }
                Ok(A11yEvent::ConnectionChanged { .. }) => {
                    // The adapter (and whether it streams) may have changed;
                    // poll again until the new stream proves itself.
                    source = None;
                    focus_event_seen = false;
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
                let stale = source
                    .is_none_or(|(_, _, at)| now.saturating_duration_since(at) >= NATIVE_EVENTS_RECHECK);
                if stale {
                    let (connected, native) = read_event_source(&app).await;
                    source = Some((connected, native, now));
                }
                if source.is_some_and(|(connected, native, _)| {
                    poll_needed(connected, native, focus_event_seen)
                }) {
                    pending = Some(FocusHint::Poll);
                    deadline = now;
                }
            }

            _ = tokio::time::sleep_until(deadline), if pending.is_some() => {
                let Some(hint) = pending.take() else { continue };
                let cached = resolve_cached(&app, &hint).await;
                let now = Instant::now();
                let resolved = match next_step(
                    cached.is_some(),
                    recapture_wait(last_capture, now, MIN_RECAPTURE_INTERVAL),
                ) {
                    NextStep::EmitCached => cached,
                    NextStep::Defer(wait) => {
                        // Rate-limited: keep the hint, retry once allowed.
                        pending = Some(hint);
                        deadline = now + wait;
                        continue;
                    }
                    NextStep::Recapture => {
                        last_capture = Some(now);
                        resolve_by_capture(&app, &hint).await
                    }
                };
                if let Some(r) = resolved {
                    if is_inspector_own(&app, &r) {
                        debug!("focus: ignoring focus inside the inspector's own window");
                        continue;
                    }
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

/// What the cached snapshot says about an event hint.
#[derive(Debug)]
pub enum CacheLookup<'a> {
    /// The hint's `ref_id` resolved exactly — authoritative.
    Exact(&'a UnifiedNode),
    /// Only the name matched, uniquely — a hint for the re-capture, never an
    /// answer (see the module docs).
    NameHint(&'a UnifiedNode),
    Miss,
}

/// Look an event hint up in `root` alone (step 1 of the module docs). Pure so
/// the exact-vs-hint distinction is testable.
pub fn resolve_in_snapshot<'a>(
    ref_id: &str,
    node_name: Option<&str>,
    root: &'a UnifiedNode,
) -> CacheLookup<'a> {
    if !ref_id.trim().trim_start_matches('@').is_empty() {
        if let Some(node) = root.find_by_ref(&normalize_ref(ref_id)) {
            return CacheLookup::Exact(node);
        }
    }
    match node_name.and_then(|name| find_unique_by_name(root, name)) {
        Some(node) => CacheLookup::NameHint(node),
        None => CacheLookup::Miss,
    }
}

/// Step 1: an EXACT ref hit in the cached snapshot, no capture. `None` for a
/// poll hint (polling exists to see changes the cache cannot show), for a
/// name-only hint, or when the cache cannot answer.
async fn resolve_cached(app: &tauri::AppHandle, hint: &FocusHint) -> Option<Resolved> {
    let FocusHint::Event { ref_id, node_name } = hint else {
        return None;
    };
    let state = app.try_state::<InspectorState>()?;
    let mgr = state.manager.lock().await;
    let snap = mgr.snapshot().await?;
    match resolve_in_snapshot(ref_id, node_name.as_deref(), &snap.root) {
        CacheLookup::Exact(node) => Some(Resolved::from_node(node, &snap.root, snap.generation)),
        CacheLookup::NameHint(_) | CacheLookup::Miss => None,
    }
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
    grid.identity = None;
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

    fn exact(lookup: CacheLookup<'_>) -> Option<&str> {
        match lookup {
            CacheLookup::Exact(n) => Some(n.ref_id.as_str()),
            _ => None,
        }
    }

    fn name_hint(lookup: CacheLookup<'_>) -> Option<&str> {
        match lookup {
            CacheLookup::NameHint(n) => Some(n.ref_id.as_str()),
            _ => None,
        }
    }

    #[test]
    fn cached_resolution_prefers_a_resolvable_ref() {
        let t = tree();
        assert_eq!(
            exact(resolve_in_snapshot("e3", Some("Save"), &t)),
            Some("@e3")
        );
    }

    #[test]
    fn a_cached_name_match_is_only_a_hint() {
        let t = tree();
        // AT-SPI object path: not a ref.
        let path = "/org/a11y/atspi/accessible/42";
        assert_eq!(
            name_hint(resolve_in_snapshot(path, Some("Save"), &t)),
            Some("@e2")
        );
        assert_eq!(
            name_hint(resolve_in_snapshot("", Some("Save"), &t)),
            Some("@e2")
        );
        assert!(exact(resolve_in_snapshot("", Some("Save"), &t)).is_none());
    }

    #[test]
    fn cached_resolution_misses_duplicate_or_missing_names() {
        let t = tree();
        for name in [Some("OK"), Some("Missing"), None] {
            assert!(matches!(
                resolve_in_snapshot("", name, &t),
                CacheLookup::Miss
            ));
        }
    }

    #[test]
    fn a_name_hint_recaptures_and_defers_rather_than_emitting_the_cache() {
        let t = tree();
        let lookup = resolve_in_snapshot("", Some("Save"), &t);
        let exact_hit = matches!(lookup, CacheLookup::Exact(_));
        // Limiter open: re-capture.
        assert_eq!(next_step(exact_hit, None), NextStep::Recapture);
        // Limiter closed: wait and retry — never fall back to the cached guess.
        let wait = Duration::from_millis(400);
        assert_eq!(next_step(exact_hit, Some(wait)), NextStep::Defer(wait));
    }

    #[test]
    fn an_exact_cached_ref_is_emitted_even_while_rate_limited() {
        assert_eq!(next_step(true, None), NextStep::EmitCached);
        assert_eq!(
            next_step(true, Some(Duration::from_millis(900))),
            NextStep::EmitCached
        );
    }

    #[test]
    fn polls_until_a_focus_event_has_actually_arrived() {
        // Disconnected: nothing to poll.
        assert!(!poll_needed(false, true, true));
        assert!(!poll_needed(false, false, false));
        // No stream at all.
        assert!(poll_needed(true, false, false));
        assert!(poll_needed(true, false, true));
        // A stream is attached but has not delivered focus yet.
        assert!(poll_needed(true, true, false));
        // A stream that has delivered focus is relied on.
        assert!(!poll_needed(true, true, true));
    }

    #[test]
    fn an_unresolved_grid_has_no_identity() {
        let grid = unresolved_grid("", Some("Save"), "why");
        assert!(grid.identity.is_none() && grid.selector.is_none());
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
