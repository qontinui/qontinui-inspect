# qontinui-inspect

Native accessibility inspector for [qontinui](https://github.com/jspinak/qontinui).
Identify UI elements (role, automation ID, bounds, state) in Windows/Linux/macOS
apps while authoring automation specs. Parity with FlaUInspect's hover and focus
modes, plus three-state highlighting to validate qontinui selector round-trips.

## Status

Scaffold. Hover Mode works end-to-end on Windows. Focus Tracking and Show
Selector are stubs. In-target-app overlay highlighting is deferred (inspector
currently shows highlights in its own UI only).

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

- **Hover Mode** — Ctrl+hover highlights the element under the cursor; property
  grid updates live.
- **Focus Tracking** — *stub*. Will subscribe to `UIA_AutomationFocusChangedEventId`.
- **Show Selector** — *stub*. Returns `@<ref_id>` placeholder; will render full
  qontinui-selector strings once the grammar stabilizes.

## Platform

- Windows: functional via runner's UIA and JAB adapters.
- Linux (AT-SPI) and macOS (AX): hover loop not yet wired; returns early.
