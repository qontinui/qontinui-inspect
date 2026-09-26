// Qontinui Inspector frontend — vanilla JS using the global `window.__TAURI__`
// binding (enabled by `withGlobalTauri: true` in tauri.conf.json).
//
// Commands wired:
//   - get_backend_name
//   - capture_desktop
//   - start_hover_mode / stop_hover_mode
//   - start_focus_tracking / stop_focus_tracking
//   - get_selector_for_ref / show_selector_matches
//   - get_property_grid
//   - show_overlay / hide_overlay
//   - save_collapse_state / load_collapse_state
//
// Events listened to: element-hovered, element-focused.
//
// Highlight palette (in-UI and on-screen overlay alike):
//   hover yellow #eab308, selected blue #3b82f6, focus / match green #10b981.

const { invoke } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;

// ---- DOM refs ---------------------------------------------------------------

const backendEl = document.getElementById("backend-name");
const statusEl = document.getElementById("status-message");
const modeRadios = document.querySelectorAll('input[name="mode"]');
const paneHover = document.getElementById("hover-pane");
const paneFocus = document.getElementById("focus-pane");
const paneSelector = document.getElementById("selector-pane");
const captureBtn = document.getElementById("capture-btn");
const refInput = document.getElementById("ref-input");
const getSelectorBtn = document.getElementById("get-selector-btn");
const showMatchesBtn = document.getElementById("show-matches-btn");
const selectorOutput = document.getElementById("selector-output");
const matchesOutput = document.getElementById("matches-output");
const toggleAllBtn = document.getElementById("toggle-all-btn");
const selectCurrentBtn = document.getElementById("select-current-btn");
const currentRefEl = document.getElementById("current-ref");
const focusToggleBtn = document.getElementById("focus-toggle-btn");
const focusPreview = document.getElementById("focus-preview");
const overlayToggle = document.getElementById("overlay-toggle");
const hideOverlayBtn = document.getElementById("hide-overlay-btn");
const overlayStatusEl = document.getElementById("overlay-status");

const propRef = document.getElementById("prop-ref");
const propRole = document.getElementById("prop-role");
const propName = document.getElementById("prop-name");
const propValue = document.getElementById("prop-value");
const propAutoId = document.getElementById("prop-automation-id");
const propClassName = document.getElementById("prop-class-name");
const propHtmlTag = document.getElementById("prop-html-tag");
const propState = document.getElementById("prop-state");
const propBounds = document.getElementById("prop-bounds");
const propSelector = document.getElementById("prop-selector");

const panes = {
  hover: paneHover,
  focus: paneFocus,
  selector: paneSelector,
};

// Refs from the runner's ref manager already carry the sigil ("@e3"); users
// may type them either way. Display every ref with exactly one "@".
function refLabel(ref) {
  return `@${String(ref).trim().replace(/^@+/, "")}`;
}

// The grid currently shown, and why it is shown ("hover" | "selected" |
// "focus"), which fixes its highlight colour.
let shownGrid = null;
let shownKind = null;

// ---- On-screen overlay ------------------------------------------------------

// Sticky once the backend reports the platform unsupported, so every hover
// does not re-ask; in-UI highlighting keeps working regardless.
let overlayUnsupported = null;

async function drawOverlay(boundsList, kind) {
  if (!overlayToggle.checked || overlayUnsupported) return;
  const rects = boundsList.filter((b) => b && b.width > 0 && b.height > 0);
  if (rects.length === 0) {
    await hideOverlay();
    return;
  }
  try {
    await invoke("show_overlay", { bounds: rects, kind });
    overlayStatusEl.textContent = `outlined (${kind})`;
  } catch (e) {
    const msg = String(e);
    if (msg.startsWith("overlay unsupported on")) {
      overlayUnsupported = msg;
    }
    overlayStatusEl.textContent = msg;
  }
}

async function hideOverlay() {
  try {
    await invoke("hide_overlay");
  } catch (e) {
    console.warn("hide_overlay failed:", e);
  }
  if (!overlayUnsupported) overlayStatusEl.textContent = "";
}

overlayToggle.addEventListener("change", () => {
  if (!overlayToggle.checked) {
    hideOverlay();
  } else if (shownGrid && shownGrid.bounds) {
    drawOverlay([shownGrid.bounds], shownKind);
  }
});
hideOverlayBtn.addEventListener("click", hideOverlay);

// ---- Mode handling ----------------------------------------------------------

let currentMode = "hover";
let focusTracking = false;
// Set while start_focus_tracking is in flight, so a second click or a mode
// switch during the start does not race it.
let focusStartPending = false;

async function startFocusTracking() {
  if (focusStartPending) return;
  focusStartPending = true;
  try {
    await invoke("start_focus_tracking");
    focusTracking = true;
    focusToggleBtn.textContent = "Stop focus tracking";
    statusEl.textContent = "focus tracking active";
  } catch (e) {
    focusTracking = false;
    focusToggleBtn.textContent = "Start focus tracking";
    statusEl.textContent = `focus tracking error: ${e}`;
  } finally {
    focusStartPending = false;
  }
  // The user left focus mode while the start was in flight: stop it again.
  if (currentMode !== "focus") {
    await stopFocusTracking();
  }
}

async function stopFocusTracking() {
  try {
    await invoke("stop_focus_tracking");
  } catch (e) {
    console.warn("stop_focus_tracking failed:", e);
  }
  focusTracking = false;
  focusToggleBtn.textContent = "Start focus tracking";
}

function setMode(mode) {
  currentMode = mode;
  for (const [name, el] of Object.entries(panes)) {
    el.classList.toggle("active", name === mode);
  }
  if (mode === "hover") {
    invoke("start_hover_mode").then(() => {
      statusEl.textContent = "hover mode active (hold Ctrl)";
    });
  } else {
    invoke("stop_hover_mode");
  }
  if (mode === "focus") {
    startFocusTracking();
  } else {
    // Always stop when leaving focus mode — even when `focusTracking` is not
    // (yet) true, a start may be in flight. The backend treats it as a no-op
    // when nothing runs.
    stopFocusTracking();
  }
  if (mode === "selector") {
    statusEl.textContent = "selector mode";
  }
}

for (const radio of modeRadios) {
  radio.addEventListener("change", (e) => setMode(e.target.value));
}

focusToggleBtn.addEventListener("click", () => {
  if (focusStartPending) return;
  if (focusTracking) {
    stopFocusTracking().then(() => {
      statusEl.textContent = "focus tracking stopped";
    });
  } else {
    startFocusTracking();
  }
});

// ---- Capture ----------------------------------------------------------------

captureBtn.addEventListener("click", async () => {
  statusEl.textContent = "capturing...";
  try {
    const n = await invoke("capture_desktop");
    statusEl.textContent = `captured ${n} nodes`;
  } catch (e) {
    statusEl.textContent = `capture error: ${e}`;
  }
});

// ---- Selector rendering -----------------------------------------------------

async function copyText(text) {
  try {
    await navigator.clipboard.writeText(text);
    return true;
  } catch (_) {
    // Fallback for webviews without the async clipboard API.
    const ta = document.createElement("textarea");
    ta.value = text;
    document.body.appendChild(ta);
    ta.select();
    const ok = document.execCommand("copy");
    ta.remove();
    return ok;
  }
}

// Render a SelectorInfo ({step, strategy, match_count, unique, session_ref,
// session_ref_note}) into `container`: pretty JSON, a unique/ambiguous badge,
// and a copy button.
function renderSelector(container, info) {
  container.replaceChildren();
  if (!info) {
    const p = document.createElement("p");
    p.className = "hint";
    p.textContent = "(no selector — element not resolved in the tree)";
    container.appendChild(p);
    return;
  }
  const json = JSON.stringify(info.step, null, 2);

  const bar = document.createElement("div");
  bar.className = "selector-bar";

  const badge = document.createElement("span");
  badge.className = `badge ${info.unique ? "badge-unique" : "badge-ambiguous"}`;
  badge.textContent = info.unique
    ? "unique"
    : info.match_count === 0
      ? "no match"
      : `ambiguous (${info.match_count} matches on the captured desktop)`;
  badge.title =
    `strategy: ${info.strategy}; ${info.match_count} match(es) on the captured desktop ` +
    "(upper bound for a narrower step target)";
  bar.appendChild(badge);

  const copyBtn = document.createElement("button");
  copyBtn.className = "secondary-btn";
  copyBtn.textContent = "Copy";
  copyBtn.addEventListener("click", async () => {
    const ok = await copyText(json);
    copyBtn.textContent = ok ? "Copied" : "Copy failed";
    setTimeout(() => (copyBtn.textContent = "Copy"), 1200);
  });
  bar.appendChild(copyBtn);
  container.appendChild(bar);

  const pre = document.createElement("pre");
  pre.className = "selector-json";
  pre.textContent = json;
  container.appendChild(pre);

  const ref = document.createElement("p");
  ref.className = "hint session-ref";
  ref.textContent = `${info.session_ref} — ${info.session_ref_note}`;
  container.appendChild(ref);
}

// ---- Selector pane ----------------------------------------------------------

// The backend accepts refs with or without "@", so pass the input through.
function refFromInput() {
  return refInput.value.trim();
}

// Capture generation of the ref in the input box, when it was filled from a
// shown element (null when typed by hand). Sent with every ref command so the
// backend refuses a ref the tree has since renumbered ("stale ref — re-select").
let refInputGeneration = null;
refInput.addEventListener("input", () => {
  refInputGeneration = null;
});

getSelectorBtn.addEventListener("click", async () => {
  const refId = refFromInput();
  matchesOutput.textContent = "";
  if (!refId) {
    selectorOutput.textContent = "(enter a ref id)";
    return;
  }
  try {
    const generation = refInputGeneration;
    const info = await invoke("get_selector_for_ref", { refId, generation });
    renderSelector(selectorOutput, info);
    // Entering a ref selects that element (blue).
    try {
      const grid = await invoke("get_property_grid", { refId, generation });
      showGrid(grid, "selected");
    } catch (_) {
      // ignore — grid may not be loaded
    }
  } catch (e) {
    selectorOutput.textContent = `error: ${e}`;
  }
});

showMatchesBtn.addEventListener("click", async () => {
  const typed = refFromInput();
  const refId = typed || (shownGrid && shownGrid.ref_id);
  if (!refId) {
    matchesOutput.textContent = "(enter a ref id or select an element)";
    return;
  }
  const generation = typed ? refInputGeneration : shownGrid.generation;
  try {
    const res = await invoke("show_selector_matches", { refId, generation });
    renderSelector(selectorOutput, res.selector);
    const lines = [
      `${res.match_refs.length} match(es) on the captured desktop ` +
        `(upper bound for a narrower step target): ${res.match_refs.join(", ")}`,
    ];
    if (res.overlay_error) {
      lines.push(`on-screen outline unavailable: ${res.overlay_error}`);
      overlayStatusEl.textContent = res.overlay_error;
    } else {
      lines.push(`outlined ${res.drawn} on screen (green)`);
      overlayStatusEl.textContent = "outlined (match)";
    }
    matchesOutput.textContent = lines.join("\n");
  } catch (e) {
    matchesOutput.textContent = `error: ${e}`;
  }
});

// ---- Property grid ----------------------------------------------------------

const KIND_CLASS = {
  hover: "highlight-hover",
  selected: "highlight-selected",
  focus: "highlight-target",
};

function renderPropertyGrid(grid) {
  currentRefEl.textContent = grid.ref_id
    ? refLabel(grid.ref_id)
    : grid.note || "(unresolved element)";
  propRef.textContent = grid.ref_id;
  propRole.textContent = grid.role;
  propName.textContent = grid.name ?? "";
  propValue.textContent = grid.value ?? "";
  propAutoId.textContent = grid.automation_id ?? "";
  propClassName.textContent = grid.class_name ?? "";
  propHtmlTag.textContent = grid.html_tag ?? "";
  propState.textContent = JSON.stringify(grid.state, null, 2);
  propBounds.textContent = grid.bounds
    ? JSON.stringify(grid.bounds, null, 2)
    : "(no bounds)";
  renderSelector(propSelector, grid.selector);
}

// Show `grid` in the property grid, highlighted as `kind`, and outline it on
// screen in the same colour.
function showGrid(grid, kind) {
  shownGrid = grid;
  shownKind = kind;
  renderPropertyGrid(grid);
  currentRefEl.classList.remove(...Object.values(KIND_CLASS));
  currentRefEl.classList.add("current-ref-badge", KIND_CLASS[kind]);
  if (grid.bounds) {
    drawOverlay([grid.bounds], kind);
  } else {
    hideOverlay();
  }
}

selectCurrentBtn.addEventListener("click", () => {
  if (shownGrid && shownGrid.ref_id) {
    showGrid(shownGrid, "selected");
    refInput.value = refLabel(shownGrid.ref_id);
    refInputGeneration = shownGrid.generation;
    statusEl.textContent = `selected ${refLabel(shownGrid.ref_id)}`;
  }
});

// ---- Backend events ---------------------------------------------------------

listen("element-hovered", (event) => {
  const grid = event.payload;
  showGrid(grid, "hover");
  statusEl.textContent = `hovered ${refLabel(grid.ref_id)} (${grid.role})`;
});

listen("element-focused", (event) => {
  const grid = event.payload;
  // Auto-select the focused element.
  showGrid(grid, "focus");
  focusPreview.hidden = false;
  focusPreview.textContent = grid.ref_id
    ? `focused ${refLabel(grid.ref_id)} (${grid.role}${grid.name ? ` "${grid.name}"` : ""})`
    : `focused: ${grid.note ?? "unresolved"}`;
  statusEl.textContent = grid.ref_id
    ? `focused ${refLabel(grid.ref_id)} (${grid.role})`
    : "focus changed (unresolved)";
});

// ---- Collapse state persistence --------------------------------------------

const detailsEls = document.querySelectorAll("#property-grid details");

async function saveCollapseState() {
  const collapsed = [];
  detailsEls.forEach((d) => {
    if (!d.open) collapsed.push(d.dataset.section);
  });
  try {
    await invoke("save_collapse_state", { sections: collapsed });
  } catch (e) {
    console.warn("save_collapse_state failed:", e);
  }
}

async function loadCollapseState() {
  try {
    const sections = await invoke("load_collapse_state");
    const collapsed = new Set(sections);
    detailsEls.forEach((d) => {
      d.open = !collapsed.has(d.dataset.section);
    });
  } catch (e) {
    console.warn("load_collapse_state failed:", e);
  }
}

detailsEls.forEach((d) => d.addEventListener("toggle", saveCollapseState));

toggleAllBtn.addEventListener("click", () => {
  const anyOpen = Array.from(detailsEls).some((d) => d.open);
  detailsEls.forEach((d) => (d.open = !anyOpen));
  saveCollapseState();
});

// ---- Init -------------------------------------------------------------------

(async () => {
  try {
    const backend = await invoke("get_backend_name");
    backendEl.textContent = `backend: ${backend}`;
  } catch (e) {
    backendEl.textContent = `backend: (error: ${e})`;
  }
  await loadCollapseState();
  setMode("hover");
})();
