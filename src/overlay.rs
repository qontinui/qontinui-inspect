//! In-target-app overlay (plan Phase 4c).
//!
//! A second Tauri window, label [`OVERLAY_LABEL`], that is transparent,
//! undecorated, shadowless, always-on-top, skip-taskbar, non-focusable and
//! click-through (`set_ignore_cursor_events(true)`). The backend positions it
//! over the target element's screen bounds and emits what to draw to
//! `src-frontend/overlay.html`, which paints coloured borders:
//! hover yellow, selected blue, focus / selector-match green.
//!
//! # One window over the union, not one window per match
//!
//! "Show matches" can outline several elements at once. This module uses ONE
//! window sized to the union of all rects, with the page drawing one border
//! per rect at its offset inside the union. Rejected alternative: one window
//! per match. Each Tauri window is a full webview (a WebView2 / WebKitGTK
//! process-side instance), so N matches would cost N webviews, a create /
//! destroy cycle every time the match set changes, and visible flicker while
//! they come up — and the window count would be unbounded by construction.
//! The union window's only cost is that it may span screen area between
//! distant matches, which is harmless: it is transparent and click-through,
//! so nothing beneath it loses input.
//!
//! # Coordinates
//!
//! `UnifiedBounds` are screen coordinates from the platform accessibility API,
//! which are physical pixels on UIA (per-monitor DPI-aware process) and on
//! X11. The window is therefore placed with `PhysicalPosition` /
//! `PhysicalSize`, never logical units.
//!
//! # Platform support
//!
//! [`overlay_support`] returns `Err("overlay unsupported on <os>: <reason>")`
//! instead of panicking where the platform cannot do this: macOS (transparent
//! windows require tauri's `macos-private-api` feature, which this crate
//! deliberately does not enable — it also requires `macOSPrivateApi` in
//! tauri.conf.json, and a feature/config mismatch fails `tauri-build` on every
//! OS), and Linux under a Wayland session (a Wayland client cannot place its
//! own window at absolute screen coordinates). The inspector's in-UI
//! highlighting keeps working in both cases.

use std::sync::{Mutex, MutexGuard};

use serde::{Deserialize, Serialize};
use tauri::{Emitter, Manager};

use qontinui_runner_lib::accessibility::model::UnifiedBounds;

use crate::tree::OVERLAY_WINDOW_TITLE;

/// Window label of the overlay.
pub const OVERLAY_LABEL: &str = "overlay";

/// Event carrying an [`OverlayDraw`] to the overlay page.
pub const OVERLAY_DRAW_EVENT: &str = "overlay-draw";

/// Border thickness in physical pixels. The border is drawn OUTSIDE the
/// element (the window is inflated by this much on every side) so it never
/// covers the element's own edge pixels.
pub const BORDER_PX: i32 = 3;

/// What an overlay outline means, which fixes its colour.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OverlayKind {
    Hover,
    Selected,
    Focus,
    Match,
}

impl OverlayKind {
    /// Colour, matching the inspector's in-UI highlight palette.
    pub fn color(self) -> &'static str {
        match self {
            OverlayKind::Hover => "#eab308",
            OverlayKind::Selected => "#3b82f6",
            OverlayKind::Focus | OverlayKind::Match => "#10b981",
        }
    }
}

/// A rectangle relative to the overlay window's top-left corner.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayRect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

/// Where the overlay window goes and what it draws inside itself.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OverlayGeometry {
    /// Window position, screen physical pixels.
    pub x: i32,
    pub y: i32,
    /// Window size, physical pixels.
    pub width: u32,
    pub height: u32,
    /// One border rect per element, window-relative, already inflated by the
    /// border so the stroke sits outside the element.
    pub rects: Vec<OverlayRect>,
}

/// Compute the overlay window geometry for `bounds`.
///
/// Rects with a non-positive width or height are dropped (offscreen and
/// collapsed elements report those). Returns `None` when nothing drawable
/// remains. Each rect is inflated by `border` on every side; the window is the
/// union of the inflated rects.
pub fn overlay_geometry(bounds: &[UnifiedBounds], border: i32) -> Option<OverlayGeometry> {
    let border = border.max(0);
    let inflated: Vec<(i64, i64, i64, i64)> = bounds
        .iter()
        .filter(|b| b.width > 0 && b.height > 0)
        .map(|b| {
            let (x, y) = (
                i64::from(b.x) - i64::from(border),
                i64::from(b.y) - i64::from(border),
            );
            (
                x,
                y,
                x + i64::from(b.width) + 2 * i64::from(border),
                y + i64::from(b.height) + 2 * i64::from(border),
            )
        })
        .collect();
    let left = inflated.iter().map(|r| r.0).min()?;
    let top = inflated.iter().map(|r| r.1).min()?;
    let right = inflated.iter().map(|r| r.2).max()?;
    let bottom = inflated.iter().map(|r| r.3).max()?;

    let clamp_i32 = |v: i64| v.clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32;
    let clamp_u32 = |v: i64| v.clamp(1, i64::from(u32::MAX)) as u32;
    Some(OverlayGeometry {
        x: clamp_i32(left),
        y: clamp_i32(top),
        width: clamp_u32(right - left),
        height: clamp_u32(bottom - top),
        rects: inflated
            .iter()
            .map(|&(l, t, r, b)| OverlayRect {
                x: clamp_i32(l - left),
                y: clamp_i32(t - top),
                width: clamp_i32(r - l),
                height: clamp_i32(b - t),
            })
            .collect(),
    })
}

/// Payload of [`OVERLAY_DRAW_EVENT`], and what `get_overlay_state` returns.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OverlayDraw {
    /// Monotonic per process (starts at 1). The page ignores any draw whose
    /// `seq` is not newer than the last one it rendered, so an event and the
    /// `get_overlay_state` pull can arrive in either order.
    pub seq: u64,
    pub kind: OverlayKind,
    pub color: String,
    pub border: i32,
    pub rects: Vec<OverlayRect>,
}

/// Linux support decision, from the session's environment. Pure so it can be
/// tested without touching the process environment.
///
/// GTK picks its Wayland backend whenever `WAYLAND_DISPLAY` is set unless
/// `GDK_BACKEND` forces X11 (XWayland), and on Wayland a client cannot position
/// its own toplevel window at absolute coordinates.
pub fn linux_session_support(
    wayland_display: Option<&str>,
    gdk_backend: Option<&str>,
) -> Result<(), String> {
    let forced_x11 = gdk_backend
        .map(|b| b.split(',').next().unwrap_or("").trim() == "x11")
        .unwrap_or(false);
    let on_wayland = wayland_display.map(|d| !d.is_empty()).unwrap_or(false);
    if on_wayland && !forced_x11 {
        return Err(
            "overlay unsupported on linux: this is a Wayland session, and a Wayland \
                    client cannot place its window at absolute screen coordinates \
                    (run the inspector with GDK_BACKEND=x11 to use XWayland)"
                .to_string(),
        );
    }
    Ok(())
}

/// Whether this platform can show the overlay; `Err` carries the structured
/// `overlay unsupported on <os>: <reason>` message.
pub fn overlay_support() -> Result<(), String> {
    #[cfg(target_os = "macos")]
    {
        Err(
            "overlay unsupported on macos: transparent windows require tauri's \
             `macos-private-api` feature, which this build does not enable"
                .to_string(),
        )
    }
    #[cfg(target_os = "linux")]
    {
        linux_session_support(
            std::env::var("WAYLAND_DISPLAY").ok().as_deref(),
            std::env::var("GDK_BACKEND").ok().as_deref(),
        )
    }
    #[cfg(not(any(target_os = "macos", target_os = "linux")))]
    {
        Ok(())
    }
}

/// Last drawn state, so an overlay page that loads after an emit can pull it
/// (`get_overlay_state`) instead of missing it.
///
/// ONE mutex serializes every `show` / `hide` end to end — sequence
/// allocation, window geometry, the recorded state and the emit. With the
/// sequence, the recorded state and the window moves locked separately, two
/// concurrent shows could interleave so that the window ends up placed for
/// one draw while the page renders the other (newer seq), or a `hide` could
/// land between a show's state write and its `show()` and be undone. Nothing
/// in `show` / `hide` awaits, so a `std` mutex is enough.
#[derive(Default)]
pub struct OverlayState {
    inner: Mutex<OverlayInner>,
}

#[derive(Default)]
struct OverlayInner {
    /// Last sequence number handed out.
    seq: u64,
    last: Option<OverlayDraw>,
}

impl OverlayInner {
    /// The next draw sequence number (1, 2, …).
    fn next_seq(&mut self) -> u64 {
        self.seq += 1;
        self.seq
    }

    /// Allocate the next draw for `rects` and record it as the current one.
    fn begin_draw(&mut self, kind: OverlayKind, rects: Vec<OverlayRect>) -> OverlayDraw {
        let draw = OverlayDraw {
            seq: self.next_seq(),
            kind,
            color: kind.color().to_string(),
            border: BORDER_PX,
            rects,
        };
        self.last = Some(draw.clone());
        draw
    }
}

impl OverlayState {
    /// Hold the overlay lock. A poisoned lock is taken over: the state it
    /// guards is plain data that is valid at every point a panic could leave.
    fn lock(&self) -> MutexGuard<'_, OverlayInner> {
        self.inner
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    pub fn last(&self) -> Option<OverlayDraw> {
        self.lock().last.clone()
    }
}

/// Get the overlay window, creating it (hidden) on first use.
#[cfg(not(target_os = "macos"))]
fn ensure_window(app: &tauri::AppHandle) -> Result<tauri::WebviewWindow, String> {
    if let Some(w) = app.get_webview_window(OVERLAY_LABEL) {
        return Ok(w);
    }
    let window = tauri::WebviewWindowBuilder::new(
        app,
        OVERLAY_LABEL,
        tauri::WebviewUrl::App("overlay.html".into()),
    )
    .title(OVERLAY_WINDOW_TITLE)
    .transparent(true)
    .decorations(false)
    .shadow(false)
    .always_on_top(true)
    .skip_taskbar(true)
    .resizable(false)
    .focused(false)
    .focusable(false)
    .visible(false)
    .build()
    .map_err(|e| format!("overlay window creation failed: {}", e))?;
    window
        .set_ignore_cursor_events(true)
        .map_err(|e| format!("overlay click-through failed: {}", e))?;
    Ok(window)
}

/// Position the overlay over `bounds` and draw them in `kind`'s colour.
/// Serialized with every other `show` / `hide` (see [`OverlayState`]).
pub fn show(
    app: &tauri::AppHandle,
    state: &OverlayState,
    bounds: &[UnifiedBounds],
    kind: OverlayKind,
) -> Result<OverlayDraw, String> {
    overlay_support()?;
    let geometry = overlay_geometry(bounds, BORDER_PX)
        .ok_or_else(|| "overlay: no element bounds to draw (all empty or missing)".to_string())?;

    let mut inner = state.lock();

    #[cfg(not(target_os = "macos"))]
    {
        let window = ensure_window(app)?;
        window
            .set_position(tauri::PhysicalPosition::new(geometry.x, geometry.y))
            .map_err(|e| format!("overlay position failed: {}", e))?;
        window
            .set_size(tauri::PhysicalSize::new(geometry.width, geometry.height))
            .map_err(|e| format!("overlay resize failed: {}", e))?;
        let draw = inner.begin_draw(kind, geometry.rects);
        app.emit_to(OVERLAY_LABEL, OVERLAY_DRAW_EVENT, &draw)
            .map_err(|e| format!("overlay emit failed: {}", e))?;
        window
            .show()
            .map_err(|e| format!("overlay show failed: {}", e))?;
        // Re-assert click-through after show: some window managers reset the
        // input shape when a window is mapped.
        let _ = window.set_ignore_cursor_events(true);
        Ok(draw)
    }

    #[cfg(target_os = "macos")]
    {
        // Unreachable: `overlay_support` refuses macOS above.
        Ok(inner.begin_draw(kind, geometry.rects))
    }
}

/// Hide the overlay (kept alive for reuse). A no-op when it was never shown.
/// Serialized with every other `show` / `hide` (see [`OverlayState`]).
pub fn hide(app: &tauri::AppHandle, state: &OverlayState) -> Result<(), String> {
    let mut inner = state.lock();
    inner.last = None;
    if let Some(window) = app.get_webview_window(OVERLAY_LABEL) {
        window
            .hide()
            .map_err(|e| format!("overlay hide failed: {}", e))?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tree::test_support::bounds;

    #[test]
    fn single_rect_is_inflated_by_the_border() {
        let g = overlay_geometry(&[bounds(100, 200, 50, 20)], 3).unwrap();
        assert_eq!((g.x, g.y, g.width, g.height), (97, 197, 56, 26));
        assert_eq!(
            g.rects,
            vec![OverlayRect {
                x: 0,
                y: 0,
                width: 56,
                height: 26
            }]
        );
    }

    #[test]
    fn multiple_rects_cover_the_union_with_relative_offsets() {
        let g = overlay_geometry(&[bounds(100, 100, 10, 10), bounds(200, 150, 20, 5)], 2).unwrap();
        assert_eq!((g.x, g.y), (98, 98));
        assert_eq!((g.width, g.height), (124, 59));
        assert_eq!(
            g.rects[1],
            OverlayRect {
                x: 100,
                y: 50,
                width: 24,
                height: 9
            }
        );
    }

    #[test]
    fn negative_screen_coordinates_are_kept() {
        // A monitor left of the primary has negative x.
        let g = overlay_geometry(&[bounds(-1900, 10, 100, 40)], 3).unwrap();
        assert_eq!((g.x, g.y), (-1903, 7));
    }

    #[test]
    fn empty_rects_are_dropped_and_all_empty_is_none() {
        assert!(overlay_geometry(&[], 3).is_none());
        assert!(overlay_geometry(&[bounds(0, 0, 0, 10), bounds(5, 5, 10, -1)], 3).is_none());
        let g = overlay_geometry(&[bounds(0, 0, 0, 10), bounds(10, 10, 4, 4)], 0).unwrap();
        assert_eq!(g.rects.len(), 1);
        assert_eq!((g.x, g.y, g.width, g.height), (10, 10, 4, 4));
    }

    #[test]
    fn draw_sequence_is_monotonic_from_one() {
        let state = OverlayState::default();
        let mut inner = state.lock();
        assert_eq!(inner.next_seq(), 1);
        assert_eq!(inner.next_seq(), 2);
        assert_eq!(inner.next_seq(), 3);
    }

    #[test]
    fn concurrent_draws_leave_the_newest_one_recorded() {
        // Sequence allocation and the recorded draw share one lock, so the
        // recorded draw is always the highest sequence handed out.
        let state = std::sync::Arc::new(OverlayState::default());
        let workers: Vec<_> = (0..8)
            .map(|_| {
                let state = state.clone();
                std::thread::spawn(move || {
                    for _ in 0..200 {
                        state.lock().begin_draw(OverlayKind::Hover, vec![]);
                    }
                })
            })
            .collect();
        for w in workers {
            w.join().unwrap();
        }
        assert_eq!(state.last().unwrap().seq, 8 * 200);
    }

    #[test]
    fn colours_match_the_palette() {
        assert_eq!(OverlayKind::Hover.color(), "#eab308");
        assert_eq!(OverlayKind::Selected.color(), "#3b82f6");
        assert_eq!(OverlayKind::Focus.color(), "#10b981");
        assert_eq!(OverlayKind::Match.color(), "#10b981");
    }

    #[test]
    fn wayland_is_unsupported_unless_x11_forced() {
        assert!(linux_session_support(None, None).is_ok());
        assert!(linux_session_support(Some(""), None).is_ok());
        let err = linux_session_support(Some("wayland-0"), None).unwrap_err();
        assert!(err.starts_with("overlay unsupported on linux:"));
        assert!(linux_session_support(Some("wayland-0"), Some("x11")).is_ok());
        assert!(linux_session_support(Some("wayland-0"), Some("x11,wayland")).is_ok());
        assert!(linux_session_support(Some("wayland-0"), Some("wayland,x11")).is_err());
    }
}
