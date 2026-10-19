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

/** Enter the installed browser surface from the generated index document. */
export async function bootRyeOsDocument(): Promise<RunningRyeOs> {
  const root = document.getElementById("app");
  if (!root) throw new Error("RyeOS browser root #app is missing");
  setBootStage("Joining node", "Opening the authenticated browser session and durable seat.");
  try {
    return await bootRyeOs(root);
  } catch (error) {
    renderBootFailure(root, error);
    throw error;
  }
}

export async function bootRyeOs(root: Element): Promise<RunningRyeOs> {
  const wasm = await loadWasm();
  await wasm.default({ module_or_path: "/ui/assets/ryeos_web_bg.wasm" });
  const sessionUnknown = await getJson("/ui/api/session/current");
  const surfaceAttachment = sessionSurfaceAttachment(sessionUnknown);
  const initial = wasm.ryeos_start(sessionUnknown, viewport(), BigInt(Date.now()));

  let runtime: UiRuntime;
  let sessionRuntime: SessionRuntime | null = null;
  let preferences: LayoutPreferencePersistence | null = null;
  let renderer: RyeOsRenderer;
  let closed = false;
  let seatAttached = false;
  const queuedUiEvents: RyeOsUiEvent[] = [];
  const dispatchUi = (event: RyeOsUiEvent) => {
    if (!seatAttached) {
      queuedUiEvents.push(event);
      return;
    }
    runtime.enqueueEvent({ type: "ui", event });
  };
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
    sessionId: sessionId(sessionUnknown),
    eventsUrl: sessionEventsUrl(sessionUnknown),
    surfaceAttachment,
    seatEvents: () => wasm.ryeos_seat_events(),
    commitEvent: (event) => runtime.enqueueEvent(event),
    replaySeatEvents: (events) => runtime.commitMutation(() => wasm.ryeos_replay_seat_events(events)),
  });
  runtime.enqueueEnvelope(initial);
  await sessionRuntime.attachSeat();
  seatAttached = true;
  for (const event of queuedUiEvents.splice(0)) runtime.enqueueEvent({ type: "ui", event });
  configurePreferences(wasm, runtime, (next) => { preferences = next; });
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

export interface SessionBindingCoordinate {
  readonly binding_attachment_id: string;
  readonly binding_generation: number;
  readonly binding_digest: string;
}

/** Resolve the immutable authored launch attachment without treating array order as authority. */
export function sessionSurfaceAttachment(session: unknown): SessionBindingCoordinate {
  const record = sessionRecord(session);
  const surfaceAttachmentId = requiredNonEmptyString(record, "surface_attachment_id");
  if (!Array.isArray(record.binding_attachments)) {
    throw new Error("authenticated browser session binding_attachments is invalid");
  }
  const matches = record.binding_attachments.filter((attachment) =>
    typeof attachment === "object" && attachment !== null && !Array.isArray(attachment)
      && (attachment as Record<string, unknown>).binding_attachment_id === surfaceAttachmentId
  );
  if (matches.length !== 1) {
    throw new Error("authenticated browser session does not contain exactly one surface attachment");
  }
  const attachment = matches[0] as Record<string, unknown>;
  const generation = attachment.binding_generation;
  if (typeof generation !== "number" || !Number.isSafeInteger(generation) || generation < 1) {
    throw new Error("authenticated browser session surface attachment generation is invalid");
  }
  return {
    binding_attachment_id: requiredNonEmptyString(attachment, "binding_attachment_id"),
    binding_generation: generation,
    binding_digest: requiredNonEmptyString(attachment, "binding_digest"),
  };
}

function sessionRecord(session: unknown): Record<string, unknown> {
  if (typeof session !== "object" || session === null || Array.isArray(session)) {
    throw new Error("authenticated browser session is not an object");
  }
  return session as Record<string, unknown>;
}

function requiredNonEmptyString(record: Record<string, unknown>, field: string): string {
  const value = record[field];
  if (typeof value !== "string" || value.length === 0) {
    throw new Error(`authenticated browser session ${field} is invalid`);
  }
  return value;
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

function sessionId(session: unknown): string {
  const value = sessionRecord(session).session_id;
  if (typeof value !== "string" || value.length === 0) {
    throw new Error("authenticated browser session has no session_id");
  }
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
    // Mounted controls may already have translated this key into an exact
    // addressed shared event. Never feed the same physical key through the
    // global keymap a second time.
    if (event.defaultPrevented) return;
    if (isTypingTarget(event.target)) return;
    const key = keyEvent(event);
    if (!key) return;
    if ((event.key === "Enter" || event.key === " ") && !hasModifiers(key) && isNativeActivationTarget(event.target)) return;
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

function setBootStage(stage: string, detail: string): void {
  const stageNode = document.getElementById("ryeos-boot-stage");
  const detailNode = document.getElementById("ryeos-boot-detail");
  if (stageNode) stageNode.textContent = stage;
  if (detailNode) detailNode.textContent = detail;
}

function renderBootFailure(root: HTMLElement, error: unknown): void {
  console.error("RyeOS boot failed", error);
  const main = document.createElement("main");
  main.className = "ryeos-boot ryeos-boot-failed";
  main.setAttribute("role", "alert");
  const kicker = document.createElement("p");
  kicker.className = "ryeos-boot-kicker";
  kicker.textContent = "RyeOS / connection interrupted";
  const title = document.createElement("h1");
  title.textContent = "The node surface did not open";
  const diagnostic = document.createElement("pre");
  diagnostic.className = "ryeos-boot-error-detail";
  diagnostic.textContent = error instanceof Error ? error.message : String(error);
  const guidance = document.createElement("p");
  guidance.className = "ryeos-boot-detail";
  guidance.textContent = "Run ryeos web again to mint a fresh one-time launch, or check ryeos node status if the node is offline.";
  const retry = document.createElement("button");
  retry.className = "ryeos-boot-retry";
  retry.type = "button";
  retry.textContent = "Retry this session";
  retry.addEventListener("click", () => location.reload());
  main.append(kicker, title, diagnostic, guidance, retry);
  root.replaceChildren(main);
}
