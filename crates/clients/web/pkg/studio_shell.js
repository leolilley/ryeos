import init, {
  studio_apply_effect_result,
  studio_dispatch,
  studio_key,
  studio_replay_seat_events,
  studio_seat_events,
  studio_start,
} from "/ui/assets/ryeos_web.js";
import { renderDom } from "/ui/assets/studio_dom_adapter.js";
import { failedResultFor, runEffect } from "/ui/assets/studio_effects.js";

let root = null;
let committing = false;
let queuedEnvelope = null;
let currentEnvelope = null;
let latestDimension = null;
let seatThreadId = null;
let seatSynced = 0;
let seatSyncing = false;

export async function bootStudio(appRoot) {
  root = appRoot;
  await init("/ui/assets/ryeos_web_bg.wasm");

  const session = await getJson("/ui/api/session/current");
  let envelope = studio_start(session, viewport(), BigInt(Date.now()));
  envelope = await attachSeat(session, envelope);
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
          if (result?.kind === "dimension" && result?.data) latestDimension = result.data;
          return commit(studio_apply_effect_result(result));
        })
        .catch((error) => commit(studio_apply_effect_result(failedResultFor(effect, error))));
    }
    void syncSeatBraid();
  } finally {
    committing = false;
    if (queuedEnvelope) {
      const next = queuedEnvelope;
      queuedEnvelope = null;
      await commit(next);
    }
  }
}

async function attachSeat(session, envelope) {
  const seededEvents = safeSeatEvents().length;
  try {
    const opened = await invokeSeatService("service:ui/seat/open", {
      surface_ref: session.surface_ref,
      client_ref: "client:ryeos/web",
    });
    seatThreadId = opened?.thread_id || null;
    if (!seatThreadId) return envelope;

    let replayedEnvelope = envelope;
    if (opened?.reattached) {
      const replay = await invokeSeatService("service:ui/seat/replay", {
        chain_root_id: seatThreadId,
      });
      const events = Array.isArray(replay?.events) ? replay.events : [];
      if (events.length > 0) {
        replayedEnvelope = studio_replay_seat_events(events);
      }
    }

    const currentEvents = safeSeatEvents().length;
    seatSynced = currentEvents > seededEvents ? currentEvents : 0;
    return replayedEnvelope;
  } catch (error) {
    console.warn("RyeOS Studio seat attach failed; continuing with local-only seat", error);
    seatThreadId = null;
    seatSynced = 0;
    return envelope;
  }
}

async function syncSeatBraid() {
  if (!seatThreadId || seatSyncing) return;
  const events = safeSeatEvents();
  if (events.length <= seatSynced) return;
  const targetLen = events.length;
  const batch = events.slice(seatSynced).map((event) => ({
    event_type: event.event_type,
    payload: {
      seq: event.seq,
      payload: event.payload,
    },
  })).filter((event) => event.event_type);
  if (batch.length === 0) {
    seatSynced = targetLen;
    return;
  }

  seatSyncing = true;
  try {
    await invokeSeatService("service:ui/seat/append", {
      thread_id: seatThreadId,
      events: batch,
    });
    seatSynced = targetLen;
  } catch (error) {
    console.warn("RyeOS Studio seat sync failed", error);
  } finally {
    seatSyncing = false;
    if (safeSeatEvents().length > seatSynced) void syncSeatBraid();
  }
}

function safeSeatEvents() {
  try {
    const events = studio_seat_events();
    return Array.isArray(events) ? events : [];
  } catch (_error) {
    return [];
  }
}

async function invokeSeatService(commandId, args) {
  const resp = await postJson("/ui/api/actions/invoke", { command_id: commandId, args });
  return resp?.result?.result ?? resp?.result ?? resp;
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
    dimension: latestDimension,
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
    // The binding table is the SHARED studio keymap (studio_key →
    // studio_key_command in base), identical to the terminal, so the two
    // renderers never diverge on what a key does. Only genuinely-web key
    // handling stays here in JS: native text entry (the input-dock textarea
    // and the launcher search field own their own typing, submit, and
    // completion while focused) and native activation controls. Everything
    // else routes through the shared keymap.
    if (isTypingTarget(event.target)) return;
    const key = studioKeyEvent(event);
    if (!key) return;
    // Plain Enter on a focused native control triggers its native click.
    if (key.key === "enter" && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;

    let outcome;
    try {
      outcome = studio_key(key);
    } catch (error) {
      console.warn("RyeOS Studio key handling failed", error);
      return;
    }
    // An unhandled key (unbound, or Ctrl+C which is native copy in the browser)
    // leaves both the studio state and the default browser behavior untouched.
    if (!outcome?.handled) return;
    event.preventDefault();
    if (outcome.envelope) void commit(outcome.envelope);
  });
  window.addEventListener("resize", debounce(() => {
    void commit(studio_dispatch({ type: "resize", viewport: viewport() }));
  }, 120));
  window.addEventListener("hashchange", () => {
    void commit(studio_dispatch({ type: "route_changed", route: location.hash.replace(/^#/, "") }));
  });
  window.addEventListener("pagehide", () => {
    if (!seatThreadId) return;
    const body = JSON.stringify({
      command_id: "service:ui/seat/close",
      args: { thread_id: seatThreadId },
    });
    if (navigator.sendBeacon) {
      navigator.sendBeacon("/ui/api/actions/invoke", new Blob([body], { type: "application/json" }));
    } else {
      fetch("/ui/api/actions/invoke", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body,
        keepalive: true,
      }).catch(() => {});
    }
  });
}

// Translate a DOM KeyboardEvent into the neutral StudioKeyEvent the shared
// keymap consumes (`{ key, modifiers }`). Named keys map to their StudioKey
// variant; a single printable character maps to `Char(ch)` (serialized as
// `{ char }`). Keys with no shared binding (F-keys, Home, PageUp, dead keys)
// return null so the browser keeps them native.
function studioKeyEvent(event) {
  const key = studioKeyName(event.key);
  if (!key) return null;
  return {
    key,
    modifiers: {
      ctrl: event.ctrlKey,
      alt: event.altKey,
      shift: event.shiftKey,
      meta: event.metaKey,
    },
  };
}

function studioKeyName(domKey) {
  switch (domKey) {
    case "ArrowUp": return "arrow_up";
    case "ArrowDown": return "arrow_down";
    case "ArrowLeft": return "arrow_left";
    case "ArrowRight": return "arrow_right";
    case "Enter": return "enter";
    case "Escape": return "escape";
    case "Backspace": return "backspace";
    case "Tab": return "tab";
    default:
      return domKey.length === 1 ? { char: domKey } : null;
  }
}

function hasModifiers(key) {
  const m = key.modifiers || {};
  return !!(m.ctrl || m.alt || m.shift || m.meta);
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

async function postJson(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
  if (!response.ok) throw new Error(`${url}: ${response.status} ${await response.text()}`);
  return response.json();
}

function debounce(fn, wait) {
  let timer = null;
  return (...args) => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => fn(...args), wait);
  };
}
