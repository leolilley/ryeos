import init, {
  studio_apply_effect_result,
  studio_dispatch,
  studio_start,
} from "/ui/assets/ryeos_web.js";
import { renderDom } from "/ui/assets/studio_dom_adapter.js";
import { failedResultFor, runEffect } from "/ui/assets/studio_effects.js";

let root = null;
let committing = false;
let queuedEnvelope = null;
let currentEnvelope = null;
let latestSnapshot = null;

export async function bootStudio(appRoot) {
  root = appRoot;
  await init("/ui/assets/ryeos_web_bg.wasm");

  const session = await getJson("/ui/api/session/current");
  let envelope = studio_start(session, viewport(), BigInt(Date.now()));
  await commit(envelope);
  if (location.hash) {
    await commit(studio_dispatch({ type: "route_changed", route: location.hash.replace(/^#/, "") }));
  }

  attachSessionEvents(session);
  attachBrowserEvents();
}

async function commit(envelope) {
  if (committing) {
    queuedEnvelope = envelope;
    return;
  }
  committing = true;
  try {
    currentEnvelope = envelope;
    const focus = captureFocus(root);
    const scroll = captureTileScroll(root);
    renderDom(root, envelope.view_model, envelope.scene_model, dispatchUi, shellController());
    restoreTileScroll(root, scroll);
    revealSelectedRows(root);
    restoreFocus(root, focus);
    if (envelope.view_model?.launcher?.open) {
      requestAnimationFrame(() => root?.querySelector("[data-studio-launcher-input]")?.focus());
    }
    for (const effect of envelope.effects || []) {
      runEffect(effect)
        .then((result) => {
          if (result?.kind === "snapshot" && result?.data) latestSnapshot = result.data;
          return commit(studio_apply_effect_result(result));
        })
        .catch((error) => commit(studio_apply_effect_result(failedResultFor(effect, error))));
    }
  } finally {
    committing = false;
    if (queuedEnvelope) {
      const next = queuedEnvelope;
      queuedEnvelope = null;
      await commit(next);
    }
  }
}

function dispatchUi(event) {
  void commit(studio_dispatch({ type: "ui", event }));
}

function rerenderShell() {
  if (!currentEnvelope || committing) return;
  renderDom(root, currentEnvelope.view_model, currentEnvelope.scene_model, dispatchUi, shellController());
}

function shellController() {
  return {
    snapshot: latestSnapshot,
    openLauncher() {
      dispatchUi({ type: "open_launcher" });
    },
    closeLauncher() {
      dispatchUi({ type: "close_launcher" });
    },
    setLauncherQuery(value) {
      dispatchUi({ type: "set_launcher_query", query: value });
    },
    moveLauncher(delta) {
      dispatchUi({ type: "move_launcher_selection", delta });
    },
    chooseLauncher(secondary) {
      dispatchUi({ type: "choose_launcher", secondary: !!secondary });
    },
  };
}

function captureFocus(container) {
  const active = document.activeElement;
  if (!active || !container?.contains(active)) return null;
  const focusKey = active.getAttribute("data-focus-key");
  if (!focusKey) return null;
  return {
    focusKey,
    selectionStart: typeof active.selectionStart === "number" ? active.selectionStart : null,
    selectionEnd: typeof active.selectionEnd === "number" ? active.selectionEnd : null,
  };
}

function restoreFocus(container, focus) {
  if (!focus) return;
  const target = container.querySelector(`[data-focus-key="${cssEscape(focus.focusKey)}"]`);
  if (!target) return;
  target.focus({ preventScroll: true });
  if (focus.selectionStart !== null && typeof target.setSelectionRange === "function") {
    target.setSelectionRange(focus.selectionStart, focus.selectionEnd ?? focus.selectionStart);
  }
}

function captureTileScroll(container) {
  const state = new Map();
  container?.querySelectorAll(".studio-tile").forEach((tile) => {
    const id = tile.dataset.tileId;
    const body = tile.querySelector(".studio-tile-body");
    if (id && body) state.set(id, { top: body.scrollTop, left: body.scrollLeft });
  });
  container?.querySelectorAll("[data-scroll-key]").forEach((node) => {
    state.set(`scroll:${node.dataset.scrollKey}`, { top: node.scrollTop, left: node.scrollLeft });
  });
  return state;
}

function restoreTileScroll(container, state) {
  for (const [id, pos] of state || []) {
    const body = container.querySelector(`.studio-tile[data-tile-id="${cssEscape(id)}"] .studio-tile-body`);
    if (!body) continue;
    body.scrollTop = pos.top;
    body.scrollLeft = pos.left;
  }
  container?.querySelectorAll("[data-scroll-key]").forEach((node) => {
    const pos = state.get(`scroll:${node.dataset.scrollKey}`);
    if (!pos) return;
    node.scrollTop = pos.top;
    node.scrollLeft = pos.left;
  });
}

function revealSelectedRows(container) {
  container?.querySelectorAll(".studio-rows .studio-row.selected").forEach((row) => {
    row.scrollIntoView({ block: "nearest" });
  });
}

function cssEscape(value) {
  if (window.CSS?.escape) return window.CSS.escape(value);
  return String(value).replace(/["\\]/g, "\\$&");
}

function attachSessionEvents(session) {
  const eventsUrl = session?.events_url || session?.event_url;
  if (!eventsUrl) return;
  const source = new EventSource(eventsUrl);
  source.addEventListener("message", (event) => {
    try {
      const payload = JSON.parse(event.data);
      void commit(studio_dispatch({ type: "daemon_event", payload }));
    } catch (error) {
      console.warn("Failed to process RyeOS event stream message", error);
    }
  });
}

function attachBrowserEvents() {
  window.addEventListener("keydown", (event) => {
    const launcherOpen = !!currentEnvelope?.view_model?.launcher?.open;
    if (event.altKey && !event.ctrlKey && !event.metaKey && event.key.toLowerCase() === "k") {
      event.preventDefault();
      shellController().openLauncher();
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey && event.key.toLowerCase() === "q") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "close_focused" } });
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey && event.key.toLowerCase() === "m") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "toggle_focused_master" } });
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey && event.key.toLowerCase() === "t") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "toggle_top_status_bar" } });
      return;
    }
    if (event.altKey && !event.ctrlKey && !event.metaKey && event.key.toLowerCase() === "b") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "toggle_bottom_status_bar" } });
      return;
    }
    if (event.ctrlKey && event.shiftKey && !event.altKey && !event.metaKey) {
      const direction = arrowDirection(event.key);
      if (direction) {
        event.preventDefault();
        dispatchUi({ type: "activate", action: { type: "resize_focused", direction } });
      }
      return;
    }
    if (event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && event.key === "ArrowUp") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "move_focused_tile", direction: "up" } });
      return;
    }
    if (event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && event.key === "ArrowDown") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "move_focused_tile", direction: "down" } });
      return;
    }
    if (event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && event.key === "ArrowLeft") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "cycle_tab", direction: "up" } });
      return;
    }
    if (event.ctrlKey && !event.shiftKey && !event.altKey && !event.metaKey && event.key === "ArrowRight") {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "cycle_tab", direction: "down" } });
      return;
    }
    if (event.key === "Escape" && launcherOpen) {
      event.preventDefault();
      shellController().closeLauncher();
      return;
    }
    if (event.key === "Escape" && !isTypingTarget(event.target)) {
      event.preventDefault();
      dispatchUi({ type: "activate", action: { type: "close_focused" } });
      return;
    }
    if (event.key === "Enter" && !isTypingTarget(event.target) && !isNativeActivationTarget(event.target)) {
      event.preventDefault();
      dispatchUi({ type: "activate_focused" });
      return;
    }
    if (launcherOpen || isTypingTarget(event.target) || event.altKey || event.ctrlKey || event.metaKey) {
      return;
    }
    const cursorDelta = rowCursorDelta(event.key);
    if (cursorDelta && moveFocusedRowCursor(cursorDelta)) {
      event.preventDefault();
      return;
    }
    const direction = arrowDirection(event.key);
    if (direction) {
      event.preventDefault();
      dispatchUi({ type: "focus_direction", direction });
    }
  });
  window.addEventListener("resize", debounce(() => {
    void commit(studio_dispatch({ type: "resize", viewport: viewport() }));
  }, 120));
  window.addEventListener("hashchange", () => {
    void commit(studio_dispatch({ type: "route_changed", route: location.hash.replace(/^#/, "") }));
  });
}

function rowCursorDelta(key) {
  if (key === "ArrowUp") return -1;
  if (key === "ArrowDown") return 1;
  return 0;
}

function moveFocusedRowCursor(delta) {
  const tile = focusedTileNode(currentEnvelope?.view_model?.workspace?.root);
  const rows = tile?.view?.rows;
  if (!tile?.tile_id || !Array.isArray(rows) || rows.length === 0) return false;
  const selected = rows.findIndex((row) => row.selected);
  const current = selected >= 0 ? selected : 0;
  const next = Math.max(0, Math.min(rows.length - 1, current + delta));
  if (next === current) return false;
  dispatchUi({ type: "set_tile_cursor", tile_id: tile.tile_id, index: next });
  return true;
}

function focusedTileNode(node) {
  if (!node) return null;
  if (node.type === "tile") return node.focused ? node : null;
  if (node.type === "split") return focusedTileNode(node.first) || focusedTileNode(node.second);
  return null;
}

function arrowDirection(key) {
  switch (key) {
    case "ArrowLeft":
      return "left";
    case "ArrowRight":
      return "right";
    case "ArrowUp":
      return "up";
    case "ArrowDown":
      return "down";
    default:
      return null;
  }
}

function isTypingTarget(target) {
  return !!target?.closest?.("input, textarea, select, [contenteditable='true']");
}

function isNativeActivationTarget(target) {
  return !!target?.closest?.("button, a, summary");
}

function viewport() {
  return {
    width: window.innerWidth,
    height: window.innerHeight,
    device_pixel_ratio: window.devicePixelRatio || 1,
  };
}

async function getJson(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status}`);
  return response.json();
}

function debounce(fn, wait) {
  let timer = null;
  return (...args) => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  };
}
