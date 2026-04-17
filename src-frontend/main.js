// Qontinui Inspector frontend — vanilla JS using the global `window.__TAURI__`
// binding (enabled by `withGlobalTauri: true` in tauri.conf.json).
//
// Commands wired:
//   - get_backend_name
//   - capture_desktop
//   - start_hover_mode / stop_hover_mode
//   - start_focus_tracking (scaffold)
//   - get_selector_for_ref
//   - get_property_grid
//   - save_collapse_state / load_collapse_state

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
const selectorOutput = document.getElementById("selector-output");
const toggleAllBtn = document.getElementById("toggle-all-btn");
const currentRefEl = document.getElementById("current-ref");

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

// ---- Mode handling ----------------------------------------------------------

let currentMode = "hover";

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
    invoke("start_focus_tracking").then(() => {
      statusEl.textContent = "focus tracking stub — see plan phase 4";
    });
  }
  if (mode === "selector") {
    statusEl.textContent = "selector mode";
  }
}

for (const radio of modeRadios) {
  radio.addEventListener("change", (e) => setMode(e.target.value));
}

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

// ---- Selector ---------------------------------------------------------------

getSelectorBtn.addEventListener("click", async () => {
  const refId = refInput.value.replace(/^@/, "").trim();
  if (!refId) {
    selectorOutput.textContent = "(enter a ref id)";
    return;
  }
  try {
    const sel = await invoke("get_selector_for_ref", { refId });
    selectorOutput.textContent = sel;
    // Also populate property grid if we can.
    try {
      const grid = await invoke("get_property_grid", { refId });
      renderPropertyGrid(grid);
    } catch (_) {
      // ignore — grid may not be loaded
    }
  } catch (e) {
    selectorOutput.textContent = `error: ${e}`;
  }
});

// ---- Property grid ----------------------------------------------------------

function renderPropertyGrid(grid) {
  currentRefEl.textContent = `@${grid.ref_id}`;
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
  propSelector.textContent = grid.selector;
}

// ---- Hover events -----------------------------------------------------------

listen("element-hovered", (event) => {
  const grid = event.payload;
  renderPropertyGrid(grid);
  statusEl.textContent = `hovered @${grid.ref_id} (${grid.role})`;
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
