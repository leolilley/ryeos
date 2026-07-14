import init, {
  ryeos_apply_effect_result,
  ryeos_dispatch,
  ryeos_key,
  ryeos_replay_seat_events,
  ryeos_seat_events,
  ryeos_start,
} from "/ui/assets/ryeos_web.js";
import { renderDom } from "/ui/assets/ryeos_dom_adapter.js";
import { failedResultFor, runEffect } from "/ui/assets/ryeos_effects.js";
import {
  hasModifiers,
  isNativeActivationTarget,
  isTypingTarget,
  ryeosKeyEvent,
} from "/ui/assets/ryeos_keyboard.js";

let root = null;
let committing = false;
let queuedEnvelope = null;
let currentEnvelope = null;
let latestDimension = null;
let seatThreadId = null;
let seatHeartbeat = null;
let seatSynced = 0;
let seatSyncing = false;
let overlayOpenLastCommit = false;
let overlayReturnFocus = null;
let sessionEvents = null;
let sessionOpened = false;
let dirtyHintKinds = new Set();
let hintFlushTimer = null;
let threadTail = null;
let threadTailUrl = null;
let threadTailThreadId = null;

export async function bootRyeOs(appRoot) {
  root = appRoot;
  await init("/ui/assets/ryeos_web_bg.wasm");

  const session = await getJson("/ui/api/session/current");
  let envelope = ryeos_start(session, viewport(), BigInt(Date.now()));
  envelope = await attachSeat(session, envelope);
  await commit(envelope);
  if (location.hash) {
    await commit(ryeos_dispatch({ type: "route_changed", route: location.hash.replace(/^#/, "") }));
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
    const overlayOpen = (envelope.view_model?.overlays || []).length > 0;
    if (overlayOpen && !overlayOpenLastCommit) overlayReturnFocus = focus;
    const scroll = captureTileScroll(root);
    renderDom(root, envelope.view_model, envelope.scene_model, dispatchUi, shellController());
    syncThreadTail(envelope.view_model);
    restoreTileScroll(root, scroll);
    revealSelectedRows(root);
    restoreFocus(root, focus);
    if (!overlayOpen && overlayOpenLastCommit) {
      restoreFocus(root, overlayReturnFocus);
      overlayReturnFocus = null;
    }
    overlayOpenLastCommit = overlayOpen;
    if (overlayOpen) {
      requestAnimationFrame(() => root?.querySelector("[data-ryeos-overlay-input]")?.focus());
    }
    for (const effect of envelope.effects || []) {
      runEffect(effect)
        .then((result) => {
          if (result?.kind === "dimension" && result?.data) latestDimension = result.data;
          ryeos_dispatch({ type: "tick", now_ms: BigInt(Date.now()) });
          return commit(ryeos_apply_effect_result(result));
        })
        .catch((error) => commit(ryeos_apply_effect_result(failedResultFor(effect, error))));
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
    if (seatHeartbeat) clearInterval(seatHeartbeat);
    seatHeartbeat = setInterval(() => {
      if (seatThreadId) {
        invokeSeatService("service:ui/seat/touch", { thread_id: seatThreadId }).catch(() => {});
      }
    }, 60_000);

    let replayedEnvelope = envelope;
    if (opened?.reattached) {
      const replay = await invokeSeatService("service:ui/seat/replay", {
        chain_root_id: seatThreadId,
      });
      const events = Array.isArray(replay?.events) ? replay.events : [];
      if (events.length > 0) {
        replayedEnvelope = ryeos_replay_seat_events(events);
      }
    }

    const currentEvents = safeSeatEvents().length;
    seatSynced = currentEvents > seededEvents ? currentEvents : 0;
    return replayedEnvelope;
  } catch (error) {
    console.warn("RyeOS seat attach failed; continuing with local-only seat", error);
    seatThreadId = null;
    if (seatHeartbeat) clearInterval(seatHeartbeat);
    seatHeartbeat = null;
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
    console.warn("RyeOS seat sync failed", error);
  } finally {
    seatSyncing = false;
    if (safeSeatEvents().length > seatSynced) void syncSeatBraid();
  }
}

function safeSeatEvents() {
  try {
    const events = ryeos_seat_events();
    return Array.isArray(events) ? events : [];
  } catch (_error) {
    return [];
  }
}

async function invokeSeatService(commandId, args) {
  const resp = await postJson("/ui/api/invocations/dispatch", {
    target: { kind: "ref", ref: commandId },
    params: args,
  });
  return resp?.result?.result ?? resp?.result ?? resp;
}

function dispatchUi(event) {
  void commit(ryeos_dispatch({ type: "ui", event }));
}

function rerenderShell() {
  if (!currentEnvelope || committing) return;
  renderDom(root, currentEnvelope.view_model, currentEnvelope.scene_model, dispatchUi, shellController());
}

function shellController() {
  return {
    dimension: latestDimension,
    closeOverlay() {
      dispatchUi({ type: "close_overlay" });
    },
    setOverlayQuery(value) {
      dispatchUi({ type: "set_overlay_query", query: value });
    },
    moveOverlay(delta) {
      dispatchUi({ type: "move_overlay_selection", delta });
    },
    chooseOverlay(secondary) {
      dispatchUi({ type: "choose_overlay", secondary: !!secondary });
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
  container?.querySelectorAll(".ryeos-tile").forEach((tile) => {
    const id = tile.dataset.tileId;
    const body = tile.querySelector(".ryeos-tile-body");
    if (id && body) state.set(id, { top: body.scrollTop, left: body.scrollLeft });
  });
  container?.querySelectorAll("[data-scroll-key]").forEach((node) => {
    state.set(`scroll:${node.dataset.scrollKey}`, { top: node.scrollTop, left: node.scrollLeft });
  });
  return state;
}

function restoreTileScroll(container, state) {
  for (const [id, pos] of state || []) {
    const body = container.querySelector(`.ryeos-tile[data-tile-id="${cssEscape(id)}"] .ryeos-tile-body`);
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
  container?.querySelectorAll(".ryeos-rows .ryeos-row.selected").forEach((row) => {
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
  if (sessionEvents) {
    sessionEvents.close();
    if (threadTail) threadTail.close();
    threadTail = null;
    threadTailUrl = null;
    threadTailThreadId = null;
  }
  if (hintFlushTimer) clearTimeout(hintFlushTimer);
  hintFlushTimer = null;
  dirtyHintKinds.clear();
  sessionOpened = false;
  const source = new EventSource(eventsUrl);
  sessionEvents = source;
  const dispatchDaemonEvent = (event) => {
    try {
      const payload = JSON.parse(event.data);
      ryeos_dispatch({ type: "tick", now_ms: BigInt(Date.now()) });
      void commit(ryeos_dispatch({ type: "daemon_event", payload }));
    } catch (error) {
      console.warn("Failed to process RyeOS event stream message", error);
    }
  };
  source.addEventListener("message", dispatchDaemonEvent);
  source.addEventListener("ui_intent.applied", dispatchDaemonEvent);
  source.addEventListener("thread.hint", (event) => {
    try {
      const payload = JSON.parse(event.data);
      const kind = payload?.kind;
      if (!kind) return;
      ryeos_dispatch({ type: "tick", now_ms: BigInt(Date.now()) });
      void commit(ryeos_dispatch({ type: "hint_received", kind, payload }));
      dirtyHintKinds.add(kind);
      if (!hintFlushTimer) {
        hintFlushTimer = setTimeout(() => {
          const kinds = [...dirtyHintKinds];
          dirtyHintKinds.clear();
          hintFlushTimer = null;
          kinds.forEach((dirtyKind) => {
            void commit(ryeos_dispatch({ type: "hint_flush", kind: dirtyKind }));
          });
        }, 500);
      }
    } catch (error) {
      console.warn("Failed to process RyeOS lifecycle hint", error);
    }
  });
  const reconcile = () => {
    ryeos_dispatch({ type: "tick", now_ms: BigInt(Date.now()) });
    void commit(ryeos_dispatch({ type: "transport_reconnected" }));
  };
  source.addEventListener("snapshot_required", reconcile);
  source.addEventListener("open", () => {
    if (sessionOpened) reconcile();
    sessionOpened = true;
  });
  syncThreadTail(currentEnvelope?.view_model);
}

function syncThreadTail(vm) {
  const url = vm?.tail_url || null;
  // The braid URL is stable while a continuation advances its head. Keep the
  // current head separate from the EventSource closure so live deltas are
  // never attributed to the predecessor captured when the stream opened.
  threadTailThreadId = vm?.tail_thread_id || vm?.tail_chain_root_id || null;
  if (url === threadTailUrl) return;
  if (threadTail) threadTail.close();
  threadTail = null;
  threadTailUrl = url;
  if (!url) return;
  const source = new EventSource(url);
  threadTail = source;
  let opened = false;
  const forward = (event) => {
    try {
      const payload = JSON.parse(event.data);
      ryeos_dispatch({ type: "tick", now_ms: BigInt(Date.now()) });
      void commit(ryeos_dispatch({
        type: "thread_tail",
        thread_id: payload?.thread_id || threadTailThreadId,
        event_type: payload?.event_type || payload?._stream_event_type || event.type || "message",
        payload,
      }));
    } catch (error) {
      console.warn("Failed to process RyeOS thread tail", error);
    }
  };
  // The browser-authenticated tail adapter deliberately emits every envelope
  // as an unnamed SSE message. EventSource has no wildcard listener for named
  // events, so this keeps new event kinds forward-compatible.
  source.addEventListener("message", forward);
  source.addEventListener("open", () => {
    if (opened) {
      void commit(ryeos_dispatch({ type: "transport_reconnected" }));
    }
    opened = true;
  });
}

function attachBrowserEvents() {
  window.addEventListener("keydown", (event) => {
    // The binding table is the SHARED ryeos keymap (ryeos_key →
    // ryeos_key_command in base), identical to the terminal, so the two
    // renderers never diverge on what a key does. Only genuinely-web key
    // handling stays here in JS: native text entry (the input-dock textarea
    // and the overlay search field own their own typing, submit, and
    // completion while focused) and native activation controls. Everything
    // else routes through the shared keymap.
    if (isTypingTarget(event.target)) return;
    const key = ryeosKeyEvent(event);
    if (!key) return;
    // Plain Enter on a focused native control triggers its native click.
    if (key.key === "enter" && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;

    let outcome;
    try {
      outcome = ryeos_key(key);
    } catch (error) {
      console.warn("RyeOS key handling failed", error);
      return;
    }
    // An unhandled key (unbound, or Ctrl+C which is native copy in the browser)
    // leaves both the ryeos state and the default browser behavior untouched.
    if (!outcome?.handled) return;
    event.preventDefault();
    if (outcome.envelope) void commit(outcome.envelope);
  });
  window.addEventListener("resize", debounce(() => {
    void commit(ryeos_dispatch({ type: "resize", viewport: viewport() }));
  }, 120));
  window.addEventListener("hashchange", () => {
    void commit(ryeos_dispatch({ type: "route_changed", route: location.hash.replace(/^#/, "") }));
  });
  window.addEventListener("pagehide", () => {
    if (!seatThreadId) return;
    const body = JSON.stringify({
      target: { kind: "ref", ref: "service:ui/seat/close" },
      params: { thread_id: seatThreadId },
    });
    if (navigator.sendBeacon) {
      navigator.sendBeacon("/ui/api/invocations/dispatch", new Blob([body], { type: "application/json" }));
    } else {
      fetch("/ui/api/invocations/dispatch", {
        method: "POST",
        headers: { "Content-Type": "application/json" },
        body,
        keepalive: true,
      }).catch(() => {});
    }
  });
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
