import type { BrowserViewport, RyeOsEnvelope, RyeOsEvent, RyeOsUiEvent } from "../generated";
import { mountRyeOsRenderer, type RyeOsRenderer } from "../renderer";
import { captureBrowserPresentation, restoreBrowserPresentation } from "./browser-state";
import { createCommitRuntime, type CommitRuntime } from "./commit";
import { failedEffectResult, runEffect } from "./effects";
import { hasModifiers, isNativeActivationTarget, isTypingTarget, keyEvent } from "./keyboard";
import { createLayoutPreferencePersistence, type LayoutPreferencePersistence } from "./layout-preferences";
import { createSessionRuntime, type SessionRuntime } from "./session";
import { getJson } from "./transport";
import { loadWasm, type RyeOsWasmApi } from "./wasm-api";

type UiRuntime = CommitRuntime<RyeOsEvent, RyeOsEnvelope>;

export interface RunningRyeOs {
  close(): Promise<void>;
}

export async function bootRyeOs(root: Element): Promise<RunningRyeOs> {
  const wasm = await loadWasm();
  await wasm.default({ module_or_path: "/ui/assets/ryeos_web_bg.wasm" });
  const sessionUnknown = await getJson("/ui/api/session/current");
  const initial = wasm.ryeos_start(sessionUnknown, viewport(), BigInt(Date.now()));

  let runtime: UiRuntime;
  let sessionRuntime: SessionRuntime | null = null;
  let preferences: LayoutPreferencePersistence | null = null;
  let renderer: RyeOsRenderer;
  let closed = false;
  const dispatchUi = (event: RyeOsUiEvent) => runtime.enqueueEvent({ type: "ui", event });
  renderer = mountRyeOsRenderer(root, initial, dispatchUi);

  runtime = createCommitRuntime({
    reduce: (event) => wasm.ryeos_dispatch(event),
    applyEffectResult: (result) => wasm.ryeos_apply_effect_result(result),
    startEffect: runEffect,
    failedEffectResult,
    async render(envelope) {
      const browserState = captureBrowserPresentation(root);
      renderer.replaceEnvelope(envelope);
      await Promise.resolve();
      restoreBrowserPresentation(root, browserState);
      preferences?.observeAcceptedState();
      sessionRuntime?.observe(envelope.view_model);
    },
  });

  sessionRuntime = createSessionRuntime({
    eventsUrl: sessionEventsUrl(sessionUnknown),
    seatEvents: () => wasm.ryeos_seat_events(),
    commitEvent: (event) => runtime.enqueueEvent(event),
    replaySeatEvents: (events) => runtime.commitMutation(() => wasm.ryeos_replay_seat_events(events)),
  });
  runtime.enqueueEnvelope(initial);
  configurePreferences(wasm, runtime, (next) => { preferences = next; });
  await sessionRuntime.attachSeat();
  if (location.hash) runtime.enqueueEvent({ type: "route_changed", route: location.hash.replace(/^#/, "") });
  const detachBrowserEvents = attachBrowserEvents(wasm, runtime);

  return {
    async close() {
      if (closed) return;
      closed = true;
      detachBrowserEvents();
      preferences?.flush();
      sessionRuntime?.close();
      await runtime.idle();
      await renderer.destroy();
    },
  };
}

function sessionEventsUrl(session: unknown): string | null {
  if (typeof session !== "object" || session === null || Array.isArray(session)) {
    throw new Error("authenticated browser session is not an object");
  }
  const value = (session as Record<string, unknown>).events_url;
  if (value === undefined || value === null) return null;
  if (typeof value !== "string") throw new Error("authenticated browser session events_url is invalid");
  return value;
}

function configurePreferences(
  wasm: RyeOsWasmApi,
  runtime: UiRuntime,
  assign: (preferences: LayoutPreferencePersistence | null) => void,
): void {
  try {
    const key = wasm.ryeos_layout_preference_key();
    const saved = localStorage.getItem(key);
    const reportError = (error: unknown) => console.warn("RyeOS layout preference was not applied", error);
    if (saved !== null) {
      try { runtime.commitMutation(() => wasm.ryeos_restore_layout_preferences(saved)); }
      catch (error) { reportError(error); }
    }
    assign(createLayoutPreferencePersistence({
      key, persisted: saved, readCurrent: wasm.ryeos_export_layout_preferences,
      storage: localStorage, reportError,
    }));
  } catch (error) {
    console.warn("RyeOS layout preference storage is unavailable", error);
    assign(null);
  }
}

function attachBrowserEvents(wasm: RyeOsWasmApi, runtime: UiRuntime): () => void {
  const abort = new AbortController();
  const options = { signal: abort.signal };
  window.addEventListener("keydown", (event) => {
    if (isTypingTarget(event.target)) return;
    const key = keyEvent(event);
    if (!key) return;
    if (key.key === "enter" && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;
    let handled = false;
    runtime.commitMutation(() => {
      const outcome = wasm.ryeos_key(key);
      handled = outcome.handled;
      return outcome.envelope;
    });
    if (handled) event.preventDefault();
  }, options);
  let resizeTimer: number | null = null;
  window.addEventListener("resize", () => {
    if (resizeTimer !== null) window.clearTimeout(resizeTimer);
    resizeTimer = window.setTimeout(() => {
      resizeTimer = null;
      runtime.enqueueEvent({ type: "resize", viewport: viewport() });
    }, 120);
  }, options);
  window.addEventListener("hashchange", () => {
    runtime.enqueueEvent({ type: "route_changed", route: location.hash.replace(/^#/, "") });
  }, options);
  return () => {
    abort.abort();
    if (resizeTimer !== null) window.clearTimeout(resizeTimer);
  };
}

function viewport(): BrowserViewport {
  return {
    width: window.innerWidth,
    height: window.innerHeight,
    device_pixel_ratio: window.devicePixelRatio || 1,
  };
}
