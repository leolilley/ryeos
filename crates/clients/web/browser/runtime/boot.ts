import type { BrowserViewport, RyeOsEnvelope, RyeOsEvent, RyeOsUiEvent } from "../generated";
import { mountRyeOsRenderer, type RyeOsRenderer } from "../renderer";
import { captureBrowserPresentation, restoreBrowserPresentation } from "./browser-state";
import { createCommitRuntime, type CommitRuntime } from "./commit";
import { failedEffectResult, runEffect } from "./effects";
import { hasModifiers, isLauncherChord, isNativeActivationTarget, isTypingEvent, keyEvent } from "./keyboard";
import { isShellChromeEscape } from "./chrome-keyboard.js";
import { createScopedLayoutPreferencePersistence, type LayoutPreferencePersistence } from "./layout-preferences";
import { createSessionRuntime, type PresentationSeatCoordinate, type SessionRuntime } from "./session";
import { encodeCanonicalJsonBody, getJson, postEncodedJson } from "./transport";
import { awaitBootRecovery } from "./reconnect-controls";
import { requireAttachmentSubject } from "./attachment-subject";
import { loadWasm, type RyeOsWasmApi } from "./wasm-api";
import { createPresentationDocumentHandoff, preparedDocumentUrl, type PresentationDocumentHandoff } from "./presentation-document.js";

type UiRuntime = CommitRuntime<RyeOsEvent, RyeOsEnvelope>;

export interface RunningRyeOs {
  close(): Promise<void>;
}

/** Enter the installed browser surface from the generated index document. */
export async function bootRyeOsDocument(
  signal?: AbortSignal,
  predecessorSettlement?: Promise<void>,
): Promise<RunningRyeOs> {
  const root = document.getElementById("app");
  if (!root) throw new Error("RyeOS browser root #app is missing");
  setBootStage(
    predecessorSettlement ? "Settling previous presentation" : "Joining node",
    predecessorSettlement
      ? "Waiting for the previous document to release its runtime before attaching."
      : "Opening the authenticated browser session and durable seat.",
  );
  try {
    await predecessorSettlement;
    signal?.throwIfAborted();
    setBootStage("Joining node", "Opening the authenticated browser session and durable seat.");
    return await bootRyeOs(root, signal);
  } catch (error) {
    if (!signal?.aborted) renderBootFailure(root, error);
    throw error;
  }
}

export async function bootRyeOs(root: Element, signal?: AbortSignal): Promise<RunningRyeOs> {
  signal?.throwIfAborted();
  const wasm = await loadWasm();
  signal?.throwIfAborted();
  await wasm.default({ module_or_path: "/ui/assets/ryeos_web_bg.wasm" });
  signal?.throwIfAborted();
  const sessionUnknown = await getJson("/ui/api/session/current");
  signal?.throwIfAborted();
  const selection = preparedPresentationSelection(sessionUnknown, new URL(location.href));
  const rootAttachment = selection?.rootAttachment ?? sessionRootAttachment(sessionUnknown);
  let coreStarted = false;
  let replaySessionIdentity: string | null = null;

  let runtime: UiRuntime | null = null;
  let sessionRuntime: SessionRuntime | null = null;
  let preferences: LayoutPreferencePersistence | null = null;
  let renderer: RyeOsRenderer | null = null;
  let detachBrowserEvents: (() => void) | null = null;
  let closePromise: Promise<void> | null = null;
  let closed = false;
  const pendingDocuments = new Set<PresentationDocumentHandoff>();
  const documentSettlements = new Set<Promise<void>>();
  let recoveryTask: Promise<void> | null = null;
  const recoveryAbort = new AbortController();
  let seatAttached = false;
  const queuedEvents: RyeOsEvent[] = [];
  // Replay can synchronously fold Rust state before restoration is complete.
  // Register its effects in the existing FIFO, but start no platform work
  // until seat claim and replay are both acknowledged. Disposal resolves the
  // gate negatively; it never manufactures a producer or cancels admitted work.
  let releaseEffects: (ready: boolean) => void = () => {};
  let documentEpoch = 0;
  let recovering = false;
  let effectsReady = new Promise<boolean>((resolve) => { releaseEffects = resolve; });
  const dispatchEvent = (event: RyeOsEvent) => {
    if (closed) return;
    // A replacement reconstructs from the durable braid. Live recovery must
    // not accumulate an unbounded second event log while awaiting a decision.
    if (recovering) return;
    if (!seatAttached) {
      queuedEvents.push(event);
      return;
    }
    runtime?.enqueueEvent(event);
  };
  const dispatchUi = (event: RyeOsUiEvent) => {
    if (!sessionRuntime?.canAcceptMutations()) return;
    dispatchEvent({ type: "ui", event });
  };
  const abortBoot = () => { void close().catch((error: unknown) => console.error("RyeOS boot cleanup failed", error)); };
  signal?.addEventListener("abort", abortBoot, { once: true });

  function disposePendingDocuments(): void {
    for (const frame of pendingDocuments) {
      frame.dispose();
      const settlement=frame.settle();
      documentSettlements.add(settlement);
      void settlement.finally(()=>documentSettlements.delete(settlement));
    }
    pendingDocuments.clear();
  }

  function close(): Promise<void> {
    if (closePromise) return closePromise;
    closed = true;
    disposePendingDocuments();
    recoveryAbort.abort(new Error("document disposed during presentation recovery"));
    documentEpoch += 1;
    signal?.removeEventListener("abort", abortBoot);
    detachBrowserEvents?.();
    queuedEvents.length = 0;
    releaseEffects(false);
    runtime?.close();
    const closingSession = sessionRuntime;
    closingSession?.close();
    wasm.ryeos_abort_seat_replay();
    const cleanup = (async () => {
      try { preferences?.flush(); }
      finally { await renderer?.destroy(); }
    })();
    closePromise = (async () => {
      // Both branches must settle even if renderer teardown fails. This joins
      // browser-owned recovery/claim decisions, not daemon-owned executions.
      const outcomes = await Promise.allSettled([cleanup, recoveryTask ?? Promise.resolve(), closingSession?.settle() ?? Promise.resolve(), ...documentSettlements]);
      for (const outcome of outcomes) {
        if (outcome.status === "rejected") throw outcome.reason;
      }
    })();
    return closePromise;
  }

  async function admitCurrent() {
    if (!sessionRuntime) throw new Error("presentation transport is unavailable");
    if (closed) throw new Error("document disposed during presentation admission");
    signal?.throwIfAborted();
        await sessionRuntime.attachSeat();
        signal?.throwIfAborted();
        const presentation = sessionRuntime.currentPresentation();
        if (!presentation) throw new Error("presentation claim did not acknowledge a current producer");
        const admittedSession = await getJson("/ui/api/session/current");
        signal?.throwIfAborted();
        if (closed) throw new Error("document disposed during presentation admission");
        if (replaySessionIdentity !== encodeCanonicalJsonBody(admittedSession)) {
          throw new Error("authenticated session changed after candidate history reconstruction");
        }
        const currentProducer = sessionRuntime.currentProducer();
        if (!currentProducer || currentProducer.presentation_id !== presentation.presentation_id
          || currentProducer.thread_id !== presentation.thread_id) {
          throw new Error("presentation producer became unavailable during core bootstrap");
        }
        if (sessionId(admittedSession) !== sessionId(sessionUnknown)) throw new Error("authenticated session changed during presentation admission");
        const membershipUrl = new URL(location.href);
        membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
        const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
        if (!admitted || admitted.presentation.thread_id !== presentation.thread_id
          || admitted.presentation.chain_root_id !== presentation.chain_root_id
          || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id
          || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation
          || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) {
          throw new Error("acknowledged presentation membership changed before core construction");
        }
        return { admittedSession, presentation };
  }

  function installCore(admittedPresentation: PresentationSeatCoordinate) {
    const activeSession = sessionRuntime;
    if (!activeSession) throw new Error("presentation transport is unavailable");
    disposePendingDocuments();
    const installedEpoch = ++documentEpoch;
    effectsReady = new Promise<boolean>((resolve) => { releaseEffects = resolve; });
    // The private WASM candidate has folded only a completed fixed-anchor
    // stream. Promotion happens after the second membership check, before any
    // renderer or platform effect exists for this document.
    const completion = activeSession.currentReplayCompletion();
    if (!completion) throw new Error("presentation replay has no authenticated completion fence");
    const initial = wasm.ryeos_commit_seat_replay(completion);
    coreStarted = true;
    renderer = mountRyeOsRenderer(root, initial, dispatchUi);

    runtime = createCommitRuntime({
      reduce: (event) => wasm.ryeos_dispatch(event),
      applyEffectResult: (result) => wasm.ryeos_apply_effect_result(result),
      async startEffect(effect) {
        if (effect.kind.type === "show_prepared_presentation_document") {
          if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner is obsolete");
          const presentationId = effect.kind.presentation_id;
          const frame: PresentationDocumentHandoff = createPresentationDocumentHandoff(document, document.body, new URL(location.href), async () => presentationId, () => pendingDocuments.delete(frame));
          pendingDocuments.add(frame);
          await frame.ready;
          return { id: effect.id, kind: "browser_only" as const, ok: true, data: null, error: null };
        }
        // Prepare under the existing seat owner, then offer a user-activated
        // link. Browser refusal cannot erase the retained node preparation.
        if (effect.kind.type === "open_new_presentation_document") {
          if (closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner is obsolete");
          const kind=effect.kind;
          const encoded=encodeCanonicalJsonBody({action:"prepare",creation_request_id:crypto.randomUUID(),...kind.root_binding});
          const frame:PresentationDocumentHandoff=createPresentationDocumentHandoff(document,document.body,new URL(location.href),async()=>{
            if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner was disposed");
            const current = await getJson("/ui/api/session/current");
            const root = verifiedDocumentRoot(current, kind.expected_session_id, kind.root_binding);
            if (closed || installedEpoch !== documentEpoch) throw new Error("presentation document owner was disposed");
            const response=sessionRecord(await postEncodedJson("/ui/api/session/seat/open",encoded));
            const prepared=sessionRecord(response.result ?? response);
            const actual=sessionRecord(prepared.root_binding);
            if(actual.binding_attachment_id!==root.binding_attachment_id || actual.binding_generation!==root.binding_generation || actual.binding_digest!==root.binding_digest) throw new Error("prepared document root changed");
            const id=requiredNonEmptyString(prepared,"presentation_id");
            requiredNonEmptyString(prepared,"thread_id");
            requiredNonEmptyString(prepared,"chain_root_id");
            return id;
          },()=>{pendingDocuments.delete(frame);const settling=frame.settle();documentSettlements.add(settling);void settling.finally(()=>documentSettlements.delete(settling));});
          pendingDocuments.add(frame);
          await frame.ready;
          return { id: effect.id, kind: "browser_only" as const, ok: true, data: null, error: null };
        }
        if (!await effectsReady || closed || installedEpoch !== documentEpoch) throw new Error("presentation bootstrap did not admit platform effects");
        return runEffect(effect, sessionRuntime?.currentProducer() ?? null);
      },
      failedEffectResult,
      async render(envelope) {
        if (closed || installedEpoch !== documentEpoch) return;
        const browserState = captureBrowserPresentation(root);
        renderer?.replaceEnvelope(envelope);
        await Promise.resolve();
        if (closed || installedEpoch !== documentEpoch) return;
        restoreBrowserPresentation(root, browserState);
        preferences?.observeAcceptedState();
        sessionRuntime?.observe(envelope.view_model);
      },
    });

    runtime.enqueueEnvelope(initial);
    // The completed cold replay envelope already carries the one merged
    // startup effect set. Do not emit a second initial source pass here.
    activeSession.completeEngineBootstrap();
    const readyProducer = activeSession.currentProducer();
    const nextEngineSeq = activeSession.currentEngineFrontier();
    if (!readyProducer || nextEngineSeq === null) throw new Error("presentation producer became unavailable before initial effects");
    runtime.enqueueEvent({ type: "presentation_producer_ready", producer: readyProducer, next_engine_seq: nextEngineSeq });
    // CommitRuntime applies queued reducer mutations synchronously. Check the
    // exact installed incarnation/frontier before releasing platform effects;
    // an ignored stale readiness event is not an admission success.
    wasm.ryeos_require_presentation_producer_ready(readyProducer, nextEngineSeq);
    seatAttached = true;
    releaseEffects(true);
    for (const event of queuedEvents.splice(0)) runtime.enqueueEvent(event);
    configurePreferences(wasm, (next) => { preferences = next; });
    if (location.hash && activeSession.canAcceptMutations()) runtime.enqueueEvent({ type: "route_changed", route: location.hash.replace(/^#/, "") });
    detachBrowserEvents = attachBrowserEvents(wasm, runtime, () => sessionRuntime?.canAcceptMutations() ?? false);
    // The address becomes a resume coordinate only after authenticated
    // membership, producer claim, retained replay and Core installation all
    // succeed. Independent replacement must replace the predecessor address.
    history.replaceState(history.state, "", retainedPresentationUrl(new URL(location.href), admittedPresentation.presentation_id).href);
  }

  async function disposeInstalledCore(): Promise<void> {
    detachBrowserEvents?.(); detachBrowserEvents = null;
    runtime?.close(); runtime = null;
    releaseEffects(false);
    try { preferences?.flush(); }
    finally { preferences = null; await renderer?.destroy(); renderer = null; }
    coreStarted = false;
  }

  async function recoverLive(failure: unknown) {
    if (closed || recovering || !seatAttached || !sessionRuntime) return;
    recovering = true;
    seatAttached = false;
    disposePendingDocuments();
    documentEpoch += 1;
    detachBrowserEvents?.(); detachBrowserEvents = null;
    runtime?.close(); runtime = null;
    releaseEffects(false);
    try {
      preferences?.flush(); preferences = null;
      await renderer?.destroy(); renderer = null;
      if (closed) return;
      const previous = sessionRuntime;
      const replacement = await awaitBootRecovery(root, previous, failure, recoveryAbort.signal, { allowExactRetry: false });
      if (closed || signal?.aborted) {
        if (replacement !== previous) replacement.close();
        return;
      }
      previous.close();
      sessionRuntime = replacement;
      coreStarted = false; wasm.ryeos_abort_seat_replay(); replaySessionIdentity = null; queuedEvents.length = 0;
      for (;;) {
        let attemptedInstall = false;
        try {
          const admitted = await admitCurrent();
          if (closed) return;
          attemptedInstall = true;
          installCore(admitted.presentation);
          break;
        } catch (error) {
          signal?.throwIfAborted();
          if (closed) throw error;
          // A partially installed Core is not another usable producer. Keep
          // only the exact transport/receipt owner while showing recovery.
          seatAttached = false;
          disposePendingDocuments();
          documentEpoch += 1;
          await disposeInstalledCore();
          const current: SessionRuntime | null = sessionRuntime;
          if (!current) throw new Error("presentation recovery lost its exact session owner");
          // A completed runtime bootstrap cannot be reset into a second Core
          // under the same writer. Failed admission alone still permits its
          // exact cached control retry; attempted installation requires an
          // explicit observed replacement.
          const replacement = await awaitBootRecovery(root, current, error, recoveryAbort.signal, { allowExactRetry: !attemptedInstall });
          if (closed || signal?.aborted) {
            if (replacement !== current) replacement.close();
            return;
          }
          sessionRuntime = replacement;
          if (sessionRuntime !== current) { current.close(); wasm.ryeos_abort_seat_replay(); replaySessionIdentity = null; queuedEvents.length = 0; }
        }
      }
    } catch (error) {
      void close().catch((cleanupError: unknown) => console.error("RyeOS recovery cleanup failed", cleanupError));
      if (!closed && !signal?.aborted) renderBootFailure(root as HTMLElement, error);
    } finally { recovering = false; }
  }

  async function reproofSession(presentation: PresentationSeatCoordinate): Promise<unknown> {
    const admittedSession = await getJson("/ui/api/session/current");
    if (closed || signal?.aborted || sessionId(admittedSession) !== sessionId(sessionUnknown)) {
      throw new Error("authenticated session changed during paired reproof");
    }
    const membershipUrl = new URL(location.href);
    membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
    const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
    if (!admitted || admitted.presentation.thread_id !== presentation.thread_id
      || admitted.presentation.chain_root_id !== presentation.chain_root_id
      || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id
      || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation
      || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) {
      throw new Error("paired reproof has no exact current presentation membership");
    }
    return admittedSession;
  }

  try {
    sessionRuntime = createSessionRuntime({
      sessionId: sessionId(sessionUnknown),
      eventsUrl: sessionEventsUrl(sessionUnknown),
      rootAttachment,
      maxSeatAppendBytes: sessionSeatAppendBound(sessionUnknown, rootAttachment.binding_attachment_id),
      onUnavailable: (failure) => {
        if (closed || recoveryTask) return;
        const task = recoverLive(failure.error);
        recoveryTask = task;
        void task.then(
          () => { if (recoveryTask === task) recoveryTask = null; },
          () => { if (recoveryTask === task) recoveryTask = null; },
        );
      },
      ...(selection ? { presentation: selection.presentation } : {}),
      seatEvents: () => coreStarted ? wasm.ryeos_seat_events() : [],
      deriveSeatAppendIdentity: (identity) => wasm.ryeos_seat_append_identity(identity),
      deriveSeatControlIdentity: (request) => wasm.ryeos_seat_control_identity(request),
      commitEvent: dispatchEvent,
      replayAcknowledgedSeatEvents: (producer, captured, events) => {
        if (!coreStarted || !runtime || closed) throw new Error("acknowledged seat evidence has no live Core owner");
        runtime.enqueueEnvelope(wasm.ryeos_replay_acknowledged_seat_events(producer, captured, events));
      },
      async beginSeatReplay(presentation) {
        const admittedSession = await getJson("/ui/api/session/current");
        if (closed || signal?.aborted || sessionId(admittedSession) !== sessionId(sessionUnknown)) {
          throw new Error("authenticated session changed before seat history");
        }
        const membershipUrl = new URL(location.href);
        membershipUrl.searchParams.set("presentation_id", presentation.presentation_id);
        const admitted = preparedPresentationSelection(admittedSession, membershipUrl);
        if (!admitted || admitted.presentation.thread_id !== presentation.thread_id
          || admitted.presentation.chain_root_id !== presentation.chain_root_id
          || admitted.rootAttachment.binding_attachment_id !== rootAttachment.binding_attachment_id
          || admitted.rootAttachment.binding_generation !== rootAttachment.binding_generation
          || admitted.rootAttachment.binding_digest !== rootAttachment.binding_digest) {
          throw new Error("seat history has no exact authenticated presentation membership");
        }
        wasm.ryeos_begin_seat_replay(admittedSession, presentation.presentation_id, viewport(), BigInt(Date.now()));
        replaySessionIdentity = encodeCanonicalJsonBody(admittedSession);
      },
      async beginSeatReproof(presentation, requireOwner) {
        if (!coreStarted || !runtime || closed || recovering) throw new Error("paired reproof has no live Core owner");
        const admittedSession = await reproofSession(presentation);
        requireOwner();
        if (closed || recovering) throw new Error("paired reproof owner changed before staging");
        wasm.ryeos_begin_seat_reproof(admittedSession, presentation.presentation_id, viewport(), BigInt(Date.now()));
      },
      async completeSeatReproof(presentation, completion, requireOwner) {
        await reproofSession(presentation);
        requireOwner();
        if (!coreStarted || !runtime || closed || recovering) throw new Error("paired reproof owner changed before promotion");
        // The shared Core guard rechecks the exact live producer/prefix. No
        // renderer or commit runtime replacement, and no new claim occurs.
        const envelope = wasm.ryeos_commit_seat_reproof(completion);
        const acknowledgedPrefix = wasm.ryeos_acknowledged_seat_prefix_len();
        if (acknowledgedPrefix < 0n || acknowledgedPrefix > BigInt(Number.MAX_SAFE_INTEGER)) {
          throw new Error("paired reproof acknowledged prefix cannot be represented exactly");
        }
        const acknowledgedEventCount = Number(acknowledgedPrefix);
        return {
          acknowledgedEventCount,
          publish: () => {
            requireOwner();
            if (!runtime || closed || recovering) throw new Error("paired reproof document changed before rendering");
            runtime.enqueueEnvelope(envelope);
          },
        };
      },
      replaySeatEvents: (events) => wasm.ryeos_stage_seat_replay_events(events),
      replaySeatTransferProof: (proof) => wasm.ryeos_stage_seat_transfer_proof(proof),
      replaySeatMoveFreezeCancellationProof: (proof) => wasm.ryeos_stage_seat_move_freeze_cancellation_proof(proof),
      abortSeatReplay: () => wasm.ryeos_abort_seat_replay(),
    });
    let admittedPresentation: PresentationSeatCoordinate;
    for (;;) {
      try {
        const admitted = await admitCurrent();
        admittedPresentation = admitted.presentation;
        break;
      } catch (error) {
        signal?.throwIfAborted();
        if (closed) throw error;
        // Keep the exact request owner while the operator decides. An entry
        // reload is not a retry and must not silently rotate its producer.
        const previous: SessionRuntime | null = sessionRuntime;
        if (!previous) throw new Error("presentation admission lost its exact session owner");
        sessionRuntime = await awaitBootRecovery(root, previous, error, signal);
        if (sessionRuntime !== previous) {
          previous.close();
          wasm.ryeos_abort_seat_replay(); replaySessionIdentity = null;
          queuedEvents.length = 0;
        }
        signal?.throwIfAborted();
        setBootStage("Joining node", "Claiming the selected presentation and replaying its retained history.");
      }
    }
    signal?.throwIfAborted();
    installCore(admittedPresentation);

    return { close };
  } catch (error) {
    try { await close(); }
    catch (cleanupError) { console.error("RyeOS boot cleanup failed", cleanupError); }
    throw error;
  }
}

export function retainedPresentationUrl(current: URL, presentationId: string): URL {
  if (!presentationId || presentationId.trim() !== presentationId) {
    throw new Error("installed presentation identity is invalid");
  }
  const next=preparedDocumentUrl(current,presentationId);
  next.hash=current.hash;
  return next;
}

export interface SessionBindingCoordinate {
  readonly binding_attachment_id: string;
  readonly binding_generation: bigint;
  readonly binding_digest: string;
}

function verifiedDocumentRoot(session: unknown, expectedSessionId: string, expected: SessionBindingCoordinate): SessionBindingCoordinate {
  if (sessionId(session) !== expectedSessionId) throw new Error("fresh presentation belongs to another authenticated session");
  const actual = sessionAttachment(sessionRecord(session), expected.binding_attachment_id);
  if (actual.binding_generation !== expected.binding_generation || actual.binding_digest !== expected.binding_digest) {
    throw new Error("fresh presentation root attachment changed");
  }
  return actual;
}

/** Read only the selected retained signed root's projected append route bound. */
export function sessionSeatAppendBound(session: unknown, attachmentId: string): bigint {
  const record = sessionRecord(session);
  sessionAttachment(record, attachmentId);
  const attachment = (record.binding_attachments as Record<string, unknown>[])
    .find((entry) => entry.binding_attachment_id === attachmentId)!;
  const bound = sessionRecord(attachment.binding_request_bounds).max_seat_append_bytes;
  if (typeof bound !== "bigint" || bound < 1n || bound > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("authenticated browser session signed seat append byte bound is invalid");
  }
  return bound;
}

/** Resolve the immutable authored launch attachment without treating array order as authority. */
export function sessionRootAttachment(session: unknown): SessionBindingCoordinate {
  const record = sessionRecord(session);
  const rootAttachmentId = requiredNonEmptyString(record, "root_attachment_id");
  return sessionAttachment(record, rootAttachmentId);
}

/**
 * Prepared links select retained membership under the existing cookie. They
 * contain no bearer grant and never request resume/takeover automatically.
 * Copying a live presentation link therefore reaches the owner's fresh-claim
 * conflict instead of rotating its producer.
 */
export function preparedPresentationSelection(session: unknown, url: URL): {
  readonly rootAttachment: SessionBindingCoordinate;
  readonly presentation: PresentationSeatCoordinate;
} | null {
  const selections = url.searchParams.getAll("presentation_id");
  if (selections.length === 0) return null;
  const selected = selections[0];
  if (selections.length !== 1 || selected === undefined || selected.length === 0) {
    throw new Error("prepared presentation selection is ambiguous or empty");
  }
  const record = sessionRecord(session);
  if (!Array.isArray(record.presentations)) throw new Error("authenticated presentation memberships are invalid");
  const matches = record.presentations.filter((membership) =>
    typeof membership === "object" && membership !== null && !Array.isArray(membership)
    && (membership as Record<string, unknown>).presentation_id === selected);
  if (matches.length !== 1) throw new Error("prepared presentation is not an exact authenticated membership");
  const membership = matches[0] as Record<string, unknown>;
  const coordinate = sessionRecord(membership.root_binding);
  const rootAttachment = sessionAttachment(record, requiredNonEmptyString(coordinate, "binding_attachment_id"));
  if (coordinate.binding_generation !== rootAttachment.binding_generation
    || coordinate.binding_digest !== rootAttachment.binding_digest) {
    throw new Error("prepared presentation root binding is stale or invalid");
  }
  return {
    rootAttachment,
    presentation: {
      presentation_id: requiredNonEmptyString(membership, "presentation_id"),
      thread_id: requiredNonEmptyString(membership, "thread_id"),
      chain_root_id: requiredNonEmptyString(membership, "chain_root_id"),
    },
  };
}

function sessionAttachment(record: Record<string, unknown>, attachmentId: string): SessionBindingCoordinate {
  if (!Array.isArray(record.binding_attachments)) {
    throw new Error("authenticated browser session binding_attachments is invalid");
  }
  const matches = record.binding_attachments.filter((attachment) =>
    typeof attachment === "object" && attachment !== null && !Array.isArray(attachment)
      && (attachment as Record<string, unknown>).binding_attachment_id === attachmentId
  );
  if (matches.length !== 1) {
    throw new Error("authenticated browser session does not contain exactly one root attachment");
  }
  const attachment = matches[0] as Record<string, unknown>;
  requireAttachmentSubject(attachment);
  const generation = attachment.binding_generation;
  // The transport decodes JSON integer tokens as BigInt, preserving Rust u64
  // authority coordinates exactly. Do not round-trip this through Number.
  if (typeof generation !== "bigint" || generation < 1n || generation > 0xffff_ffff_ffff_ffffn) {
    throw new Error("authenticated browser session root attachment generation is invalid");
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
  assign: (preferences: LayoutPreferencePersistence | null) => void,
): void {
  try {
    const reportError = (error: unknown) => console.warn("RyeOS arrangement cache was not saved", error);
    const preferences = createScopedLayoutPreferencePersistence({
      readKey: wasm.ryeos_layout_preference_key,
      readCurrent: wasm.ryeos_export_layout_preferences,
      storage: localStorage, reportError,
    });
    assign(preferences);
    preferences.observeAcceptedState();
  } catch (error) {
    console.warn("RyeOS layout preference storage is unavailable", error);
    assign(null);
  }
}

function attachBrowserEvents(wasm: RyeOsWasmApi, runtime: UiRuntime, canAcceptMutations: () => boolean): () => void {
  const abort = new AbortController();
  const options = { signal: abort.signal };
  window.addEventListener("keydown", (event) => {
    if (!canAcceptMutations()) return;
    // Mounted controls may already have translated this key into an exact
    // addressed shared event. Never feed the same physical key through the
    // global keymap a second time.
    if (event.defaultPrevented) return;
    // IME composition keys are browser/editor state, never RyeOS commands.
    if (event.isComposing) return;
    if (isShellChromeEscape(event)) {
      event.preventDefault();
      return;
    }
    if (isTypingEvent(event) && !isLauncherChord(event)) return;
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
    if (!canAcceptMutations()) return;
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
