# qontinui-inspect

Native accessibility inspector for [qontinui](https://github.com/jspinak/qontinui).
Identify UI elements (role, automation ID, bounds, state) in Windows/Linux/macOS
apps while authoring automation specs. Parity with FlaUInspect's hover and focus
modes, plus three-state highlighting to validate qontinui selector round-trips.

## Status

Phase 4 of plan `scout-2026-04-16-native-accessibility-expansion`
(qontinui-dev-notes/plans) is implemented: Hover Mode (Windows), Focus
Tracking, Show Selector, and the in-target-app overlay. Linux/macOS hover
hit-testing is not wired yet, and the overlay is unavailable on macOS and on
Linux Wayland sessions (see [Platform](#platform)).

## Layout requirement

This crate has a path dependency on `qontinui-runner`'s accessibility library.
Clone both repos as siblings:

```
<parent>/
├── qontinui-runner/
└── qontinui-inspect/   ← this repo
```

`Cargo.toml` references `../qontinui-runner/src-tauri` — a different layout
will not resolve.

## Build

```sh
cargo check                # Rust-only typecheck
cargo tauri dev            # full app (needs frontend build toolchain)
```

## Modes

- **Hover Mode** — Ctrl+hover highlights the element under the cursor (yellow);
  property grid updates live. **Select** pins the shown element (blue).
- **Focus Tracking** — while the mode is active, every keyboard-focus change in
  any application selects the focused element in the property grid and
  outlines it in green. Events come from the runner's `AccessibilityManager`
  event bus, which forwards the platform adapter's focus stream (UIA
  focus-changed handler on Windows, AT-SPI `Event.Focus` on Linux); where the
  connected adapter has no event stream (macOS AX and Java/JAB today) it polls
  the tree once a second.
  An event that cannot be resolved to a captured node is still shown, marked
  unresolved.
- **Show Selector** — for any element, the `native_accessibility` workflow step
  that re-finds it, ready to paste:

  ```json
  { "a11y_action": "query", "a11y_query_automation_id": "btn_save" }
  ```

  The automation id is used alone when present; otherwise class name + role +
  label. The inspector runs the same query against its captured tree and badges
  the result **unique** (exactly one match, and it is this element) or
  **ambiguous** (with the count). **Show matches** outlines every match on
  screen in green. The `@eN` session ref is shown too, but it is only valid for
  the current capture session.
- **On-screen overlay** — hover, selection, focus and matches are outlined over
  the target application itself by a transparent, click-through, always-on-top
  window, in the same colours as the in-UI highlights. Toggle it with **Draw on
  screen**.

## Platform

- Windows: functional via runner's UIA and JAB adapters; all modes and the
  overlay.
- Linux (AT-SPI): Focus Tracking, Show Selector, and the overlay under X11
  (or XWayland via `GDK_BACKEND=x11`). The overlay needs a compositing window
  manager for transparency, and is refused on a Wayland session with
  `overlay unsupported on linux: …`, because a Wayland client cannot place its
  window at absolute screen coordinates. Hover loop not yet wired.
- macOS (AX): Focus Tracking (polling) and Show Selector. The overlay is
  refused with `overlay unsupported on macos: …`: transparent windows need
  tauri's `macos-private-api`, which this crate does not enable. Hover loop not
  yet wired.

## License

Licensed under the GNU Affero General Public License v3.0 or later (AGPL-3.0-or-later). See [LICENSE](LICENSE) for full terms. Contributing requires signing the [CLA](CLA.md) — see [CONTRIBUTING.md](CONTRIBUTING.md).
