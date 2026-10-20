import type {
  RyeOsEvent,
  RyeOsTransportChannel,
  RyeOsTransportFreshness,
  RyeOsViewModel,
  SeatEvent,
  UiPresentationProducer,
  UiPresentationProducerObservation,
  UiSeatControlPredecessor,
  UiSeatControlRequest,
} from "../generated";
import { requireAttachmentSubject } from "./attachment-subject";
import { consumeSeatHistory, SeatHistoryIncompleteError, SeatHistoryReproofRequiredError, type SeatHistoryClaim, type SeatHistoryHandoff, type SeatHistoryCompletion, type SeatVerifiedAppend } from "./seat-history-stream";
import { focusedThreadTailFrame } from "./thread-tail";
import {
  decodeJsonBody,
  encodeJsonBody,
  encodeSeatPayloadDigest,
  errorMessage,
  getJson,
  HttpResponseError,
  postEncodedJson,
  postJson,
  UnknownDeliveryError,
} from "./transport";

const MAX_SEAT_BATCH_EVENTS = 64;
const MAX_COLD_REPLAY_ATTEMPTS = 3;
const MAX_U64 = 18_446_744_073_709_551_615n;
const MAX_SEAT_APPEND_ATTEMPTS = 5;
const MAX_PENDING_SEAT_EVENTS = 512;
const MAX_PENDING_SEAT_BYTES = 512 * 1_024;
const MAX_REMEMBERED_APPENDS = 128;

export interface PresentationSeatCoordinate {
  readonly presentation_id: string;
  readonly thread_id: string;
  readonly chain_root_id: string;
}

/** Input to the shared Rust derivation; no browser-side identity canonicalizer. */
export interface SeatAppendIdentityInput extends PresentationSeatCoordinate {
  readonly producer_incarnation: string;
  readonly first_engine_seq: bigint;
  readonly last_engine_seq: bigint;
  readonly event_count: bigint;
  readonly payload_digest: string;
}

export interface SessionRuntimeOptions {
  readonly onUnavailable?: (failure: { readonly kind: "writer_refused" | "append_unknown"; readonly error: unknown }) => void;
  readonly sessionId: string;
  readonly eventsUrl: string | null;
  readonly rootAttachment: BindingAttachmentCoordinate;
  readonly maxSeatAppendBytes: bigint;
  readonly presentation?: PresentationSeatCoordinate;
  readonly claim?: { readonly kind: "resume" | "takeover"; readonly expected_producer_incarnation: string; readonly predecessor: UiSeatControlPredecessor };
  readonly seatEvents: () => SeatEvent[];
  readonly deriveSeatAppendIdentity: (identity: SeatAppendIdentityInput) => string;
  readonly deriveSeatControlIdentity: (request: UiSeatControlRequest) => string;
  readonly commitEvent: (event: RyeOsEvent) => void;
  readonly beginSeatReplay: (coordinate: PresentationSeatCoordinate) => Promise<void>;
  readonly beginSeatReproof: (coordinate: PresentationSeatCoordinate, requireOwner: () => void) => Promise<void>;
  readonly completeSeatReproof: (coordinate: PresentationSeatCoordinate, completion: SeatHistoryCompletion, requireOwner: () => void) => Promise<{ readonly publish: () => void; readonly acknowledgedEventCount: number }>;
  readonly replaySeatEvents: (events: unknown[]) => void;
  readonly replaySeatTransferProof: (proof: unknown) => void;
  readonly replayAcknowledgedSeatEvents: (producer: UiPresentationProducer, captured: SeatEvent[], events: unknown[]) => void;
  readonly replaySeatMoveFreezeCancellationProof: (proof: unknown) => void;
  readonly abortSeatReplay: () => void;
}

export interface SessionRuntime {
  attachSeat(): Promise<void>;
  observe(model: RyeOsViewModel): void;
  relinquish(): Promise<void>;
  currentProducer(): UiPresentationProducer | null;
  currentEngineFrontier(): bigint | null;
  canAcceptMutations(): boolean;
  /** Creates an unattached replacement. The document owner installs it only after admission/replay. */
  reconnect(choice: PresentationReconnectChoice): Promise<SessionRuntime>;
  reconcilePendingAppend(): Promise<void>;
  currentPresentation(): PresentationSeatCoordinate | null;
  currentReplayCompletion(): SeatHistoryCompletion | null;
  /** Retained transport coordinate, not proof of a writer or a successful claim. */
  preparedPresentation(): PresentationSeatCoordinate | null;
  observeProducer(): Promise<UiPresentationProducerObservation>;
  completeEngineBootstrap(): void;
  close(): void;
  /** Join document-owned work after disposal; not evidence of seat retirement. */
  settle(): Promise<void>;
}

export type PresentationReconnectChoice =
  | { readonly kind: "resume" | "takeover"; readonly expected_producer_incarnation: string; readonly predecessor: UiSeatControlPredecessor }
  | { readonly kind: "independent" };

export function createSessionRuntime(options: SessionRuntimeOptions): SessionRuntime {
  if (typeof options.maxSeatAppendBytes !== "bigint" || options.maxSeatAppendBytes < 1n
      || options.maxSeatAppendBytes > BigInt(Number.MAX_SAFE_INTEGER)) {
    throw new Error("signed seat append byte bound is invalid");
  }
  if (typeof options.deriveSeatAppendIdentity !== "function") {
    throw new Error("shared Rust seat append identity bridge is required");
  }
  if (typeof options.deriveSeatControlIdentity !== "function") {
    throw new Error("shared Rust seat control identity bridge is required");
  }
  if (typeof options.replaySeatTransferProof !== "function") {
    throw new Error("shared Rust provisional transfer proof bridge is required");
  }
  if (typeof options.replayAcknowledgedSeatEvents !== "function") {
    throw new Error("shared Rust acknowledged seat evidence bridge is required");
  }
  if (typeof options.replaySeatMoveFreezeCancellationProof !== "function") {
    throw new Error("session requires move freeze cancellation proof replay bridge");
  }
  const maxSeatAppendBytes = Number(options.maxSeatAppendBytes);
  let presentation: PresentationSeatCoordinate | null = options.presentation ?? null;
  let pendingStorageKey: string | null = presentation ? storageKey(options.sessionId, presentation.presentation_id) : null;
  // Mechanical request evidence belongs to this document evaluation only.
  // Never reuse a claim operation from copied storage in another document.
  const creationRequestId = crypto.randomUUID();
  const claimIntentNonce = crypto.randomUUID();
  let claimRequest: UiSeatControlRequest | null = null;
  let relinquishRequest: UiSeatControlRequest | null = null;
  let relinquishing: Promise<void> | null = null;
  let attaching: Promise<void> | null = null;
  const controlRequests = new Map<string, string>();
  let seatThreadId: string | null = null;
  let seatProducer: string | null = null;
  let nextEngineSeq = 0n;
  // The claim witness is immutable. Append receipts advance nextEngineSeq,
  // never the frontier recorded by the acknowledged producer claim.
  let acknowledgedClaim: SeatHistoryClaim | null = null;
  let seatObserved = 0;
  let replayHandoff: SeatHistoryHandoff | null = null;
  let replayAbort: AbortController | null = null;
  let replayInvalidated = false;
  let engineBootstrapped = false;
  const pendingSeatEvents: SeatEvent[] = [];
  let pendingSeatBytes = 0;
  let seatAttaching = false;
  let seatReady = false;
  let seatSyncing = false;
  let seatSyncPromise: Promise<void> | null = null;
  let reproofing: Promise<void> | null = null;
  let reproofPending = false;
  let seatSyncBlocked = false;
  // Exact transport evidence for this producer's one unresolved append. This
  // is not another event journal: queued events retain their existing owner.
  let currentAppend: RetainedPendingAppend | null = null;
  // ACK and verified stream frames can arrive in either order. These are
  // identities, not a second event journal or authority to fold the seat.
  const acknowledgedAppends = new Map<string, RetainedPendingAppend>();
  const acknowledgedReceipts = new Map<string, SeatReceiptCoordinate>();
  const verifiedAppends = new Map<string, SeatVerifiedAppend>();
  let closed = false;
  let seatEpoch = 0;
  let heartbeat: number | null = null;
  let seatTouchInflight = false;
  let events: EventSource | null = null;
  let eventsOpened = false;
  let tail: EventSource | null = null;
  let tailUrl: string | null = null;
  let tailThreadId: string | null = null;
  let tailChainRootId: string | null = null;
  let hintTimer: number | null = null;
  const dirtyHints = new Set<string>();

  function commitTransport(channel: RyeOsTransportChannel, freshness: RyeOsTransportFreshness): void {
    options.commitEvent({
      type: "transport_state_changed", channel, freshness,
      observed_at_ms: BigInt(Date.now()), error: null,
    });
  }

  function fenceSeatHistory(error: Error): void {
    replayInvalidated = true;
    seatReady = false;
    seatSyncBlocked = true;
    options.onUnavailable?.({ kind: "writer_refused", error });
  }

  function receiveHistoryFailure(error: Error): void {
    if (!(error instanceof SeatHistoryReproofRequiredError) || !engineBootstrapped) {
      fenceSeatHistory(error);
      return;
    }
    if (closed || replayInvalidated) return;
    if (reproofing) { reproofPending = true; return; }
    reproofPending = false;
    capturePendingSeatEvents();
    seatReady = false;
    const taskEpoch = seatEpoch;
    const taskProducer = seatProducer;
    const task = reproveCurrentSeat();
    reproofing = task;
    void task.catch((reason: unknown) => {
      if (!closed && taskEpoch === seatEpoch && taskProducer === seatProducer) {
        fenceSeatHistory(reason instanceof Error ? reason : new Error(String(reason)));
      }
    }).finally(() => {
      if (reproofing !== task) return;
      reproofing = null;
      if (reproofPending && !closed && !replayInvalidated && taskEpoch === seatEpoch && taskProducer === seatProducer) {
        receiveHistoryFailure(new SeatHistoryReproofRequiredError("paired cut advanced during promotion"));
      }
    });
  }

  async function reproveCurrentSeat(): Promise<void> {
    const coordinate = presentation ? { ...presentation } : null;
    const claim = acknowledgedClaim;
    const epoch = seatEpoch;
    const producer = seatProducer;
    if (!coordinate || !claim || !producer || replayInvalidated || closed) {
      throw new Error("paired reproof has no current acknowledged owner");
    }
    const requireOwner = () => {
      if (closed || replayInvalidated || epoch !== seatEpoch || producer !== seatProducer
        || presentation?.presentation_id !== coordinate.presentation_id) {
        throw new Error("paired reproof owner changed");
      }
    };
    // Wait for the exact admitted append, then reconcile an uncertain receipt.
    // Never clear or rebase pending bytes merely to make refresh possible.
    if (seatSyncPromise) await seatSyncPromise;
    requireOwner();
    if (currentAppend || (pendingStorageKey && loadRetainedAppend(pendingStorageKey))) {
      await reconcileRetainedAppend();
      requireOwner();
    }
    capturePendingSeatEvents();
    while (pendingSeatEvents.length > 0) {
      const frontier = nextEngineSeq;
      await syncSeat();
      requireOwner();
      if (seatSyncBlocked || nextEngineSeq === frontier) {
        throw new Error("paired reproof must retain unresolved seat mutations");
      }
      capturePendingSeatEvents();
    }
    if (seatSyncBlocked || currentAppend) throw new Error("paired reproof append is not settled");
    replayHandoff?.close();
    replayHandoff = null;
    replayAbort?.abort();
    replayAbort = new AbortController();
    for (let attempt = 0; attempt < MAX_COLD_REPLAY_ATTEMPTS; attempt++) {
      let changed = false;
      let promoted = false;
      let handoff: SeatHistoryHandoff | null = null;
      try {
        await options.beginSeatReproof(coordinate, requireOwner);
        requireOwner();
        handoff = await consumeSeatHistory(coordinate, claim, replayAbort.signal,
          (events) => { requireOwner(); options.replaySeatEvents(events); }, (error) => {
            if (closed || epoch !== seatEpoch) return;
            if (error instanceof SeatHistoryReproofRequiredError) {
              if (promoted) receiveHistoryFailure(error);
              else changed = true;
            }
            else fenceSeatHistory(error);
          }, (append) => {
            if (!closed && epoch === seatEpoch) receiveVerifiedAppend(append);
          }, (proof) => { requireOwner(); options.replaySeatTransferProof(proof); },
          (proof) => { requireOwner(); options.replaySeatMoveFreezeCancellationProof(proof); });
        requireOwner();
        if (changed) throw new SeatHistoryIncompleteError("paired reproof cut advanced");
        const promotedCore = await options.completeSeatReproof(coordinate, handoff.completion, requireOwner);
        requireOwner();
        if (!Number.isSafeInteger(promotedCore.acknowledgedEventCount)
          || promotedCore.acknowledgedEventCount < 0
          || promotedCore.acknowledgedEventCount > options.seatEvents().length
          || pendingSeatEvents.length > 0 || currentAppend) {
          throw new Error("paired reproof has no exact acknowledged transport offset");
        }
        seatObserved = promotedCore.acknowledgedEventCount;
        replayHandoff = handoff;
        handoff = null;
        promoted = true;
        seatReady = true;
        promotedCore.publish();
        // A wake after promotion starts another bounded reproof; it never
        // authorizes installing unverified suffix rows into the live Core.
        if (changed) reproofPending = true;
        capturePendingSeatEvents();
        void syncSeat();
        return;
      } catch (error) {
        handoff?.close();
        if (epoch === seatEpoch && producer === seatProducer) options.abortSeatReplay();
        if (!(error instanceof SeatHistoryIncompleteError) || attempt + 1 === MAX_COLD_REPLAY_ATTEMPTS
          || closed || replayInvalidated || replayAbort.signal.aborted) throw error;
      }
    }
  }

  function appendMatches(frame: SeatVerifiedAppend, append: RetainedPendingAppend): boolean {
    return frame.operation_id === append.operation_id
      && frame.producer_incarnation === append.producer_incarnation
      && frame.first_engine_seq === BigInt(append.first_engine_seq)
      && frame.last_engine_seq === BigInt(append.last_engine_seq)
      && frame.event_count === BigInt(append.event_count)
      && frame.payload_digest === append.payload_digest;
  }

  function receiveVerifiedAppend(frame: SeatVerifiedAppend): void {
    const known = currentAppend?.operation_id === frame.operation_id ? currentAppend
      : acknowledgedAppends.get(frame.operation_id);
    if (!known || !appendMatches(frame, known)) {
      throw new Error("verified seat append is not this producer's exact pending or acknowledged operation");
    }
    const receipt = acknowledgedReceipts.get(frame.operation_id);
    if (receipt && (receipt.event_hash !== frame.event_hash || receipt.thread_seq !== frame.thread_seq)) {
      throw new Error("verified seat append differs from its exact signed receipt");
    }
    const previous = verifiedAppends.get(frame.operation_id);
    if (previous && (previous.event_hash !== frame.event_hash || previous.thread_seq !== frame.thread_seq)) {
      throw new Error("verified seat append identity changed its retained position");
    }
    if (!previous) {
      verifiedAppends.set(frame.operation_id, frame);
    }
    trimSettledAppendMemory();
    // A verified notification never acknowledges the local provisional prefix.
    // Only the exact HTTP receipt/replay path can adopt the original canonical
    // interval together with its captured local prefix in the Core ACK owner.
  }

  function rememberAcknowledgedAppend(append: RetainedPendingAppend, receipt: SeatReceiptCoordinate): void {
    const verified = verifiedAppends.get(append.operation_id);
    if (verified && (!appendMatches(verified, append)
      || verified.event_hash !== receipt.event_hash || verified.thread_seq !== receipt.thread_seq)) {
      throw new UnknownDeliveryError("verified seat append differs from its exact acknowledgement");
    }
    acknowledgedAppends.set(append.operation_id, append);
    acknowledgedReceipts.set(append.operation_id, receipt);
    trimSettledAppendMemory();
  }

  function trimSettledAppendMemory(): void {
    // Keep all unobserved ACKs: discarding one would turn an authentic delayed
    // notification into a false foreign append. Bound that backlog and retain
    // a separate rolling window for harmless duplicate hub wakes.
    const unverified = [...acknowledgedAppends.keys()].filter((id) => !verifiedAppends.has(id));
    if (unverified.length > MAX_REMEMBERED_APPENDS) {
      throw new Error("unverified seat append backlog exceeds bound; authenticated rebootstrap required");
    }
    const settled = [...acknowledgedAppends.keys()].filter((id) => verifiedAppends.has(id));
    for (const id of settled.slice(0, Math.max(0, settled.length - MAX_REMEMBERED_APPENDS))) {
      acknowledgedAppends.delete(id);
      acknowledgedReceipts.delete(id);
      verifiedAppends.delete(id);
    }
    if (verifiedAppends.size > MAX_REMEMBERED_APPENDS * 2) {
      throw new Error("verified seat append backlog exceeds bound; authenticated rebootstrap required");
    }
  }

  function attachSeat(): Promise<void> {
    if (replayInvalidated) return Promise.reject(new Error("seat history handoff was invalidated; choose a new authenticated producer"));
    if (attaching) return attaching;
    const pending = performAttach();
    attaching = pending;
    void pending.catch(() => { if (attaching === pending) attaching = null; });
    return pending;
  }

  async function performAttach(): Promise<void> {
    if (closed || replayInvalidated) throw new Error("disposed or invalidated presentation cannot claim a producer");
    const epoch = ++seatEpoch;
    seatReady = false;
    seatAttaching = true;
    seatSyncBlocked = false;
    requireNoProvisionalEvents();
    if (!presentation) {
      if (options.claim) throw new Error("replacement claim requires an exact prepared presentation");
      const prepared = unwrapResult(await retrySeatControl("open", {
        action: "prepare", creation_request_id: creationRequestId, ...options.rootAttachment,
      }));
      if (closed || epoch !== seatEpoch) return;
      requireRootBinding(prepared.root_binding, options.rootAttachment);
      presentation = seatCoordinate(prepared);
      pendingStorageKey = storageKey(options.sessionId, presentation.presentation_id);
    }
    await reconcileRetainedAppend();
    if (closed || epoch !== seatEpoch) return;
    if (replayInvalidated) throw new Error("seat history was invalidated during producer reconciliation");
    if (claimRequest === null) {
      // Explicit replacement choices retain the predecessor the operator saw.
      // A later observation must never silently rebase that intent.
      const predecessor = options.claim
        ? controlPredecessor(options.claim.predecessor)
        : (await observeProducer()).control_predecessor;
      if (closed || epoch !== seatEpoch) return;
      if (replayInvalidated) throw new Error("seat history was invalidated during producer observation");
      claimRequest = freezeControlRequest({
        action: "claim", schema: 2, operation_id: "", ...presentation,
        predecessor, intent_nonce: claimIntentNonce,
        claim: options.claim ? { kind: options.claim.kind, expected_producer_incarnation: options.claim.expected_producer_incarnation } : { kind: "fresh" },
      });
    }
    const operationId = claimRequest.operation_id;
    const opened = unwrapResult(await retrySeatControl("open", claimRequest));
    if (closed || epoch !== seatEpoch) return;
    if (replayInvalidated) throw new Error("seat history was invalidated during producer claim");
    requireSeatCoordinate(opened, presentation);
    if (opened.operation_id !== operationId || opened.state !== "active") throw new Error("seat claim response does not match requested operation");
    seatThreadId = stringField(opened, "thread_id");
    if (!seatThreadId) throw new Error("seat/open returned no durable seat thread");
    const producer = requiredStringField(opened, "producer_incarnation");
    if (!/^[0-9a-f]{64}$/.test(producer)) throw new Error("claim returned invalid producer incarnation");
    const engineFrontier = exactU64Field(opened, "next_engine_seq");
    if (engineFrontier === MAX_U64) throw new Error("producer engine sequence is exhausted");
    seatProducer = producer;
    nextEngineSeq = engineFrontier;
    acknowledgedClaim = Object.freeze({
      operation_id: operationId,
      producer_incarnation: producer,
      next_engine_seq: engineFrontier,
    });
    requireNoProvisionalEvents();
    if (heartbeat !== null) window.clearInterval(heartbeat);
    heartbeat = window.setInterval(() => {
      const touchedPresentation = presentation?.presentation_id;
      const touchedProducer = seatProducer;
      const touchedEpoch = seatEpoch;
      const touchedThreadId = seatThreadId;
      if (!closed && seatReady && !seatTouchInflight && touchedThreadId && touchedProducer && touchedPresentation) {
        seatTouchInflight = true;
        void invokeSeat("touch", {
          presentation_id: touchedPresentation, thread_id: touchedThreadId, producer_incarnation: touchedProducer,
        }).then(
          (response) => handleSeatTouchOutcome(classifySeatTouchResponse(response, touchedPresentation, touchedThreadId), null),
          (error: unknown) => handleSeatTouchOutcome(classifySeatTouchError(error), error),
        ).finally(() => { seatTouchInflight = false; });
      }
      function handleSeatTouchOutcome(outcome: SeatTouchOutcome, error: unknown): void {
        if (closed || touchedEpoch !== seatEpoch || touchedProducer !== seatProducer
          || touchedThreadId !== seatThreadId || touchedPresentation !== presentation?.presentation_id) return;
        if (outcome === "refused") reportSeatFailure(error, false);
        // Retry leaves the producer intact but never treats an unverified
        // acknowledgement as renewal. The next bounded interval retries.
      }
    }, 60_000);
    if (replayInvalidated) throw new Error("seat history was invalidated before replay handoff");
    replayHandoff?.close();
    replayHandoff = null;
    replayAbort?.abort();
    replayAbort = new AbortController();
    // A signed tip can advance while the fixed-anchor proof is in flight.
    // Each retry discards the whole provisional reconstruction, reauthorizes
    // the same session/seat, and starts a fresh proof without another claim.
    for (let attempt = 0; attempt < MAX_COLD_REPLAY_ATTEMPTS; attempt++) {
      try {
        await options.beginSeatReplay(presentation);
        if (closed || epoch !== seatEpoch) throw new Error("seat replay owner changed before history read");
        const handoff = await consumeSeatHistory(presentation, acknowledgedClaim, replayAbort.signal, options.replaySeatEvents, (error) => {
          if (closed || epoch !== seatEpoch) return;
          receiveHistoryFailure(error);
        }, (append) => {
          if (closed || epoch !== seatEpoch) return;
          receiveVerifiedAppend(append);
        }, options.replaySeatTransferProof, options.replaySeatMoveFreezeCancellationProof);
        if (closed || epoch !== seatEpoch || replayInvalidated) {
          handoff.close();
          throw new Error("seat history changed before replay handoff");
        }
        replayHandoff = handoff;
        break;
      } catch (error) {
        options.abortSeatReplay();
        if (!(error instanceof SeatHistoryIncompleteError) || attempt + 1 === MAX_COLD_REPLAY_ATTEMPTS
          || closed || epoch !== seatEpoch || replayInvalidated || replayAbort.signal.aborted) throw error;
      }
    }
    requireNoProvisionalEvents();
    if (replayInvalidated) throw new Error("seat history was invalidated before producer readiness");
    seatAttaching = false;
    seatReady = true;
    attachEvents();
    void syncSeat();
  }

  async function reconcileRetainedAppend(): Promise<void> {
    if (replayInvalidated) throw new Error("seat history was invalidated before append reconciliation");
    if (!pendingStorageKey || !presentation) throw new Error("presentation must be prepared before reconciliation");
    const retained = loadRetainedAppend(pendingStorageKey);
    if (!retained) return;
    if (retained.session_id !== options.sessionId || retained.presentation_id !== presentation.presentation_id
      || retained.thread_id !== presentation.thread_id || retained.chain_root_id !== presentation.chain_root_id) {
      throw new Error("retained seat append belongs to a different browser session");
    }
    const expected = {
      producer: retained.producer_incarnation,
      operationId: retained.operation_id,
      firstEngineSeq: BigInt(retained.first_engine_seq),
      lastEngineSeq: BigInt(retained.last_engine_seq),
      eventCount: retained.event_count,
      payloadDigest: retained.payload_digest,
    };
    const originalIdentity = options.deriveSeatAppendIdentity({
      ...presentation, producer_incarnation: expected.producer,
      first_engine_seq: expected.firstEngineSeq, last_engine_seq: expected.lastEngineSeq,
      event_count: BigInt(expected.eventCount), payload_digest: expected.payloadDigest,
    });
    if (originalIdentity !== expected.operationId) {
      throw new Error("retained append uses an unsupported or conflicting transport identity");
    }
    const retainedRequest = retainedAppendEvents(retained, expected, presentation);
    const retainedDigest = await sha256Hex(encodeSeatPayloadDigest(retainedRequest));
    if (retainedDigest !== expected.payloadDigest) {
      throw new Error("retained append payload differs from its exact commitment");
    }
    const observed = unwrapResult(await invokeSeat("replay", {
      presentation_id: presentation.presentation_id, chain_root_id: presentation.chain_root_id,
      receipt: {
        thread_id: retained.thread_id,
        producer_incarnation: expected.producer, operation_id: expected.operationId,
        first_engine_seq: expected.firstEngineSeq, last_engine_seq: expected.lastEngineSeq,
        event_count: expected.eventCount, payload_digest: expected.payloadDigest,
      },
    }));
    if (Object.keys(observed).length !== 2
      || !Object.hasOwn(observed, "receipt_status")
      || !Object.hasOwn(observed, "receipt")) {
      throw new UnknownDeliveryError("prior seat append receipt lookup has an unsupported response");
    }
    let receipt: SeatReceiptCoordinate;
    if (observed.receipt_status === "found" && isRecord(observed.receipt)) {
      receipt = requireAppendAcknowledgement(observed.receipt, expected, retainedRequest, presentation);
    } else if (observed.receipt_status === "not_found" && observed.receipt === null) {
      // The authenticated history has no receipt yet, but the exact request
      // and its committed staged interval survived this document. Resend only
      // those same bytes; a late original delivery is the daemon's ordinary
      // idempotent retry, not a second operation or a rebased producer claim.
      receipt = await appendWithExactRetry(
        retained.encoded_request, retainedRequest, expected,
        options.sessionId, options.rootAttachment, presentation,
      );
    } else {
      throw new UnknownDeliveryError("prior seat append receipt is not confirmed; replacement claim blocked");
    }
    if (replayInvalidated) throw new Error("seat history was invalidated during append reconciliation");
    const epoch = seatEpoch;
    const current = currentAppend;
    if (seatProducer !== null) {
      if (!current || current.encoded_request !== retained.encoded_request
        || current.operation_id !== retained.operation_id
        || current.payload_digest !== retained.payload_digest
        || seatProducer !== retained.producer_incarnation
        || seatThreadId !== retained.thread_id
        || nextEngineSeq !== expected.firstEngineSeq
        || expected.lastEngineSeq !== expected.firstEngineSeq + BigInt(expected.eventCount - 1)
        || pendingSeatEvents.length < expected.eventCount) {
        throw new Error("retained receipt does not match the current producer's pending interval");
      }
      const prefix = pendingSeatEvents.slice(0, expected.eventCount);
      const wire = wireSeatEvents(prefix, expected.firstEngineSeq);
      const digest = await sha256Hex(encodeSeatPayloadDigest(wire));
      const encoded = encodeJsonBody({
        presentation_id: presentation.presentation_id, thread_id: seatThreadId,
        producer_incarnation: seatProducer, operation_id: expected.operationId,
        first_engine_seq: expected.firstEngineSeq, last_engine_seq: expected.lastEngineSeq,
        event_count: expected.eventCount, payload_digest: digest, events: wire,
      });
      if (closed || replayInvalidated || epoch !== seatEpoch || currentAppend !== current || digest !== expected.payloadDigest
        || encoded !== retained.encoded_request
        || encodeSeatPayloadDigest(wireSeatEvents(pendingSeatEvents.slice(0, expected.eventCount), expected.firstEngineSeq)) !== encodeSeatPayloadDigest(wire)) {
        throw new Error("retained receipt does not prove the exact queued prefix");
      }
      rememberAcknowledgedAppend(retained, receipt);
      clearRetainedAppend(pendingStorageKey, retained.operation_id, retained.producer_incarnation, retained.encoded_request);
      pendingSeatEvents.splice(0, expected.eventCount);
      pendingSeatBytes -= prefix.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
      nextEngineSeq = expected.lastEngineSeq + 1n;
      currentAppend = null;
      seatSyncBlocked = false;
      options.replayAcknowledgedSeatEvents({
        presentation_id: presentation.presentation_id, thread_id: retained.thread_id,
        producer_incarnation: retained.producer_incarnation,
      }, prefix, receipt.events);
      return;
    }
    if (replayInvalidated) throw new Error("seat history was invalidated before clearing retained append");
    clearRetainedAppend(pendingStorageKey, retained.operation_id, retained.producer_incarnation, retained.encoded_request);
  }

  function attachEvents(): void {
    if (!options.eventsUrl) return;
    events?.close();
    const source = new EventSource(options.eventsUrl);
    events = source;
    const forward = (event: MessageEvent<string>) => {
      try {
        options.commitEvent({ type: "tick", now_ms: BigInt(Date.now()) });
        options.commitEvent({ type: "daemon_event", payload: decodeJsonBody(event.data) });
      } catch (error) { console.warn("Failed to process RyeOS session event", error); }
    };
    source.addEventListener("message", forward);
    source.addEventListener("ui_intent.applied", forward as EventListener);
    source.addEventListener("thread.hint", ((event: MessageEvent<string>) => {
      try {
        const payload = decodeJsonBody(event.data);
        const kind = isRecord(payload) && typeof payload.kind === "string" ? payload.kind : null;
        if (!kind) return;
        options.commitEvent({ type: "hint_received", kind, payload });
        dirtyHints.add(kind);
        if (hintTimer === null) hintTimer = window.setTimeout(flushHints, 500);
      } catch (error) { console.warn("Failed to process RyeOS lifecycle hint", error); }
    }) as EventListener);
    const requireSnapshot = () => commitTransport("hints", "gap_resnapshot_required");
    source.addEventListener("snapshot_required", requireSnapshot);
    source.addEventListener("open", () => {
      commitTransport("hints", eventsOpened ? "gap_resnapshot_required" : "current");
      eventsOpened = true;
    });
    source.addEventListener("error", () => commitTransport("hints", "reconnecting"));
  }

  function flushHints(): void {
    hintTimer = null;
    const kinds = [...dirtyHints];
    dirtyHints.clear();
    if (kinds.length > 0) options.commitEvent({ type: "hint_flush_batch", kinds });
  }

  function observe(model: RyeOsViewModel): void {
    if (closed || !seatReady) return;
    syncTail(model);
    if (!seatSyncBlocked && (seatAttaching || seatReady)) capturePendingSeatEvents();
    void syncSeat();
  }

  function capturePendingSeatEvents(): void {
    if (seatSyncBlocked) return;
    const all = options.seatEvents();
    if (all.length < seatObserved) {
      seatSyncBlocked = true;
      reportSeatFailure(new Error("RyeOS seat event log moved backwards"), false);
      return;
    }
    while (seatObserved < all.length) {
      const event = all[seatObserved];
      if (!event || !appendPendingSeatEvent(event)) return;
      seatObserved += 1;
    }
  }

  function appendPendingSeatEvent(event: SeatEvent): boolean {
    let encodedBytes: number;
    try {
      encodedBytes = encodedSeatEventBytes(event);
    } catch (error) {
      seatSyncBlocked = true;
      reportSeatFailure(error, false);
      return false;
    }
    if (
      pendingSeatEvents.length >= MAX_PENDING_SEAT_EVENTS
      || pendingSeatBytes + encodedBytes > MAX_PENDING_SEAT_BYTES
    ) {
      seatSyncBlocked = true;
      reportSeatFailure(new Error(
        `unsaved seat history exceeds the pending limit (${MAX_PENDING_SEAT_EVENTS} events / ${MAX_PENDING_SEAT_BYTES} bytes)`,
      ), false);
      return false;
    }
    pendingSeatEvents.push(event);
    pendingSeatBytes += encodedBytes;
    return true;
  }

  function requireNoProvisionalEvents(): void {
    if (options.seatEvents().length !== seatObserved || pendingSeatEvents.length !== 0) {
      throw new Error("provisional seat events before authoritative replay completed are refused");
    }
  }

  function syncTail(model: RyeOsViewModel): void {
    tailThreadId = model.tail_thread_id ?? model.tail_chain_root_id ?? null;
    tailChainRootId = model.tail_chain_root_id ?? null;
    const nextUrl = model.tail_url ?? null;
    if (nextUrl === tailUrl) return;
    tail?.close();
    tail = null;
    tailUrl = nextUrl;
    if (!nextUrl) return;
    const source = new EventSource(nextUrl);
    tail = source;
    let opened = false;
    source.addEventListener("message", (event: MessageEvent<string>) => {
      try {
        const frame = decodeJsonBody(event.data);
        // The URL follows a chain, while the focused route may advance to a
        // successor thread without replacing this EventSource. Never relabel
        // an old/child frame with the new route's thread identity.
        if (source !== tail) return;
        const focused = focusedThreadTailFrame(frame, tailChainRootId, tailThreadId);
        if (!focused || !tailThreadId) return;
        options.commitEvent({
          type: "thread_tail", thread_id: tailThreadId,
          event_type: focused.eventType, payload: focused.payload,
        });
      } catch (error) { console.warn("Failed to process RyeOS thread tail", error); }
    });
    source.addEventListener("open", () => {
      commitTransport("focused_tail", opened ? "gap_resnapshot_required" : "current");
      opened = true;
    });
    source.addEventListener("error", () => commitTransport("focused_tail", "reconnecting"));
  }

  async function syncSeat(): Promise<void> {
    if (seatSyncPromise) return seatSyncPromise;
    const frontier = nextEngineSeq;
    const pending = syncSeatOnce();
    seatSyncPromise = pending;
    try {
      await pending;
    } finally {
      if (seatSyncPromise === pending) seatSyncPromise = null;
    }
    if (!closed && !seatSyncBlocked && nextEngineSeq > frontier && pendingSeatEvents.length > 0) void syncSeat();
  }

  async function syncSeatOnce(): Promise<void> {
    if (closed || !presentation || !pendingStorageKey || !seatThreadId || !seatProducer || seatSyncing || seatSyncBlocked) return;
    capturePendingSeatEvents();
    if (seatSyncBlocked) return;
    if (pendingSeatEvents.length === 0) return;
    let batchEvents = pendingSeatEvents.slice(0, MAX_SEAT_BATCH_EVENTS);
    const firstEngineSeq = nextEngineSeq;
    seatSyncing = true;
    let operationId: string;
    let payloadDigest: string;
    let encodedRequest: string;
    let batch: Array<{ engine_seq: bigint; event_type: string; payload: unknown }> = [];
    let lastEngineSeq = firstEngineSeq;
    const threadId = seatThreadId;
    const producer = seatProducer;
    const epoch = seatEpoch;
    try {
      const remainingEnginePositions = MAX_U64 - firstEngineSeq;
      if (remainingEnginePositions <= 0n) {
        throw new Error("producer engine interval is exhausted");
      }
      if (remainingEnginePositions < BigInt(batchEvents.length)) {
        batchEvents = batchEvents.slice(0, Number(remainingEnginePositions));
      }
      // UUID display width is fixed. Derive only after the exact batch is frozen.
      operationId = "00000000-0000-8000-8000-000000000000";
      while (batchEvents.length > 0) {
        const candidate = wireSeatEvents(batchEvents, firstEngineSeq);
        const candidateLast = firstEngineSeq + BigInt(candidate.length - 1);
        const candidateRequest = encodeJsonBody({
          presentation_id: presentation.presentation_id,
          thread_id: threadId,
          producer_incarnation: producer,
          operation_id: operationId,
          first_engine_seq: firstEngineSeq,
          last_engine_seq: candidateLast,
          event_count: candidate.length,
          payload_digest: "0".repeat(64),
          events: candidate,
        });
        if (encodedByteLength(candidateRequest) <= maxSeatAppendBytes) break;
        batchEvents = batchEvents.slice(0, -1);
      }
      if (batchEvents.length === 0) {
        throw new Error(`one seat event exceeds the ${maxSeatAppendBytes}-byte append route limit`);
      }
      batch = wireSeatEvents(batchEvents, firstEngineSeq);
      const actualLastEngineSeq = firstEngineSeq + BigInt(batch.length - 1);
      lastEngineSeq = actualLastEngineSeq;
      payloadDigest = await sha256Hex(encodeSeatPayloadDigest(batch));
      if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
      operationId = options.deriveSeatAppendIdentity({
        ...presentation, producer_incarnation: producer,
        first_engine_seq: firstEngineSeq, last_engine_seq: lastEngineSeq,
        event_count: BigInt(batch.length), payload_digest: payloadDigest,
      });
      if (!/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(operationId)) {
        throw new Error("shared Rust seat append identity is invalid");
      }
      encodedRequest = encodeJsonBody({
        presentation_id: presentation.presentation_id,
        thread_id: threadId,
        producer_incarnation: producer,
        operation_id: operationId,
        first_engine_seq: firstEngineSeq,
        last_engine_seq: lastEngineSeq,
        event_count: batch.length,
        payload_digest: payloadDigest,
        events: batch,
      });
      if (encodedByteLength(encodedRequest) > maxSeatAppendBytes) {
        throw new Error("seat append exceeded its route limit after final encoding");
      }
      const retained: RetainedPendingAppend = {
        schema_version: "ryeos.ui.seat.pending-append.v3",
        session_id: options.sessionId,
        presentation_id: presentation.presentation_id,
        chain_root_id: presentation.chain_root_id,
        thread_id: threadId,
        producer_incarnation: producer,
        operation_id: operationId,
        first_engine_seq: firstEngineSeq.toString(),
        last_engine_seq: lastEngineSeq.toString(),
        event_count: batch.length,
        payload_digest: payloadDigest,
        encoded_request: encodedRequest,
      };
      retainAppend(pendingStorageKey, retained);
      currentAppend = retained;
    } catch (error) {
      if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
      seatSyncBlocked = true;
      seatSyncing = false;
      reportSeatFailure(error, false);
      return;
    }
    let acknowledged = false;
    try {
      const receipt = await appendWithExactRetry(encodedRequest, batch, {
        producer, operationId, firstEngineSeq, lastEngineSeq,
        eventCount: batch.length, payloadDigest,
      }, options.sessionId, options.rootAttachment, presentation);
      if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
      if (!currentAppend) throw new Error("seat append lost its retained identity before acknowledgement");
      rememberAcknowledgedAppend(currentAppend, receipt);
      clearRetainedAppend(pendingStorageKey, operationId, producer);
      currentAppend = null;
      pendingSeatEvents.splice(0, batch.length);
      pendingSeatBytes -= batchEvents.reduce(
        (total, event) => total + encodedSeatEventBytes(event),
        0,
      );
      nextEngineSeq = lastEngineSeq + 1n;
      acknowledged = true;
      options.replayAcknowledgedSeatEvents({
        presentation_id: presentation.presentation_id, thread_id: threadId,
        producer_incarnation: producer,
      }, batchEvents, receipt.events);
    } catch (error) {
      if (closed || epoch !== seatEpoch || threadId !== seatThreadId) return;
      seatSyncBlocked = true;
      const unknown = error instanceof UnknownDeliveryError;
      reportSeatFailure(error, unknown || isRetryableResponse(error));
      console.warn("RyeOS seat sync stopped with unsaved history", error);
    } finally {
      if (epoch === seatEpoch) seatSyncing = false;
      if (acknowledged && !closed) {
        capturePendingSeatEvents();
      }
    }
  }

  function reportSeatFailure(error: unknown, retryable: boolean): void {
    if (closed) return;
    if (error instanceof HttpResponseError && (error.status === 403 || error.status === 404 || error.status === 409 || error.status === 410)) {
      seatReady = false;
      seatSyncBlocked = true;
    }
    const unknown = error instanceof UnknownDeliveryError;
    options.commitEvent({
      type: "transport_state_changed",
      channel: "session",
      freshness: "reconnecting",
      observed_at_ms: BigInt(Date.now()),
      error: {
        code: unknown ? "seat_append_outcome_unknown" : "seat_append_refused",
        error: `Seat history is not confirmed durable: ${errorMessage(error)}`,
        retryable,
        outcome: unknown ? "unknown" : "refused",
        remediation: "Relaunch or reconcile the UI session before submitting more seat mutations.",
        details: null,
      },
    });
    if (error instanceof HttpResponseError && (error.status === 403 || error.status === 404 || error.status === 409 || error.status === 410)) {
      options.onUnavailable?.({ kind: "writer_refused", error });
    } else if (error instanceof UnknownDeliveryError && seatSyncBlocked) {
      options.onUnavailable?.({ kind: "append_unknown", error });
    }
  }

  function close(): void {
    if (closed) return;
    closed = true;
    seatEpoch += 1;
    replayAbort?.abort();
    replayHandoff?.close();
    options.abortSeatReplay();
    events?.close(); tail?.close();
    if (heartbeat !== null) window.clearInterval(heartbeat);
    if (hintTimer !== null) window.clearTimeout(hintTimer);
    // This is document-adapter teardown, not canonical presentation close.
    // Disappearance/HMR cannot finalize a seat or close a successor producer.
    // Pending append evidence remains for its exact reconciliation owner.
    events = null; tail = null; heartbeat = null; hintTimer = null;
  }

  async function retrySeatControl(operation: string, body: unknown): Promise<unknown> {
    // Snapshot once so every request in this document's retry interval is exact.
    const requestKey = `${operation}:${isRecord(body) ? String(body.action) : ""}`;
    let encoded = controlRequests.get(requestKey);
    if (encoded === undefined) {
      encoded = encodeJsonBody(body);
      controlRequests.set(requestKey, encoded);
    }
    for (let attempt = 1; ; attempt += 1) {
      if (closed) throw new Error("disposed presentation cannot retry seat control");
      try { return await postEncodedJson(`/ui/api/session/seat/${operation}`, encoded); }
      catch (error) {
        if (!isRetryableResponse(error) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw error;
        await new Promise<void>(resolve => globalThis.setTimeout(resolve, Math.min(2_000, 100 * (2 ** (attempt - 1)))));
      }
    }
  }
  function freezeControlRequest(request: UiSeatControlRequest): UiSeatControlRequest {
    const operationId = options.deriveSeatControlIdentity(request);
    if (!/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(operationId)) {
      throw new Error("shared Rust seat control identity is invalid");
    }
    return Object.freeze({ ...request, predecessor: Object.freeze({ ...request.predecessor }), operation_id: operationId });
  }

  function relinquish(): Promise<void> {
    if (relinquishing) return relinquishing;
    const pending = performRelinquish();
    relinquishing = pending;
    void pending.catch(() => { if (relinquishing === pending) relinquishing = null; });
    return pending;
  }

  async function performRelinquish(): Promise<void> {
    if (closed || !presentation || !seatThreadId || !seatProducer || (!seatReady && relinquishRequest === null)) throw new Error("no active presentation producer to relinquish");
    const epoch = seatEpoch;
    const producer = seatProducer;
    if (relinquishRequest === null) {
      capturePendingSeatEvents();
      if (seatSyncing || pendingSeatEvents.length > 0 || !pendingStorageKey || loadRetainedAppend(pendingStorageKey)) {
        throw new Error("seat mutations must settle before producer relinquishment");
      }
      // Fence local mutations BEFORE awaiting the read-only observation.
      seatReady = false;
      seatSyncBlocked = true;
      let observation: UiPresentationProducerObservation;
      try { observation = await observeProducer(); }
      catch (error) {
        // No control request exists yet, so a failed read cannot strand an
        // otherwise acknowledged local writer in a fictitious control retry.
        if (!closed && !replayInvalidated && epoch === seatEpoch && producer === seatProducer) {
          seatReady = true;
          seatSyncBlocked = false;
        }
        throw error;
      }
      if (closed || epoch !== seatEpoch || producer !== seatProducer) return;
      if (replayInvalidated) throw new Error("seat history was invalidated during relinquishment observation");
      if (observation.producer.kind !== "active" || observation.producer.producer_incarnation !== producer) {
        throw new Error("relinquishment no longer names the observed active producer");
      }
      if (options.seatEvents().length !== seatObserved) {
        if (!replayInvalidated) {
          seatReady = true;
          seatSyncBlocked = false;
          capturePendingSeatEvents();
          void syncSeat();
        }
        throw new Error("new local mutations must settle before producer relinquishment");
      }
      relinquishRequest = freezeControlRequest({
        action: "relinquish", schema: 2, operation_id: "", ...presentation,
        producer_incarnation: producer, predecessor: observation.control_predecessor, intent_nonce: crypto.randomUUID(),
      });
    }
    const operationId = relinquishRequest.operation_id;
    seatReady = false;
    seatSyncBlocked = true;
    const response = unwrapResult(await retrySeatControl("close", relinquishRequest));
    if (closed || epoch !== seatEpoch || producer !== seatProducer) return;
    requireSeatCoordinate(response, presentation);
    if (response.operation_id !== operationId || response.producer_incarnation !== producer || response.state !== "relinquished") {
      throw new UnknownDeliveryError("producer relinquishment acknowledgement is invalid");
    }
    close();
  }

  async function reconnect(choice: PresentationReconnectChoice): Promise<SessionRuntime> {
    if (!presentation) throw new Error("reconnect requires an exact prepared presentation");
    if (choice.kind !== "independent" && !/^[0-9a-f]{64}$/.test(choice.expected_producer_incarnation)) {
      throw new Error("reconnect requires an exact predecessor producer incarnation");
    }
    const selectedClaim = choice.kind === "independent" ? undefined : {
      kind: choice.kind, expected_producer_incarnation: choice.expected_producer_incarnation,
      predecessor: controlPredecessor(choice.predecessor),
    };
    if (selectedClaim?.predecessor.kind === "initialization") throw new Error("replacement requires an observed control predecessor");
    // Never discard queued local work, even for an independent duplicate. An
    // unresolved persisted interval is reconciled read-only before any new
    // control operation; this function itself neither claims nor disposes.
    if (seatSyncing) throw new Error("seat append must settle before reconnect");
    await reconcileRetainedAppend();
    // A blocked transport deliberately skips capture. The Rust log can still
    // have a new provisional suffix that has never entered pendingSeatEvents.
    if (options.seatEvents().length !== seatObserved) {
      throw new Error("uncaptured seat mutations must settle before reconnect");
    }
    if (!closed && !seatSyncBlocked) capturePendingSeatEvents();
    if (pendingSeatEvents.length > 0 || currentAppend !== null
      || (pendingStorageKey !== null && loadRetainedAppend(pendingStorageKey))) {
      throw new Error("queued seat mutations must settle before reconnect");
    }
    const { presentation: _previousPresentation, claim: _previousClaim, ...sharedOptions } = options;
    return createSessionRuntime({
      ...sharedOptions,
      ...(selectedClaim === undefined ? {} : { presentation: { ...presentation }, claim: selectedClaim }),
    });
  }

  async function observeProducer(): Promise<UiPresentationProducerObservation> {
      if (closed || !presentation) throw new Error("producer observation requires a retained presentation");
      const coordinate = { ...presentation };
      const observation = unwrapResult(await invokeSeat("open", { action: "observe", ...coordinate }));
      if (closed || !presentation || presentation.presentation_id !== coordinate.presentation_id) {
        throw new Error("producer observation belongs to a disposed document");
      }
      requireSeatCoordinate(observation, coordinate);
      requireRootBinding(observation.root_binding, options.rootAttachment);
      const revision = exactIntegerField(observation, "revision");
      const control_predecessor = controlPredecessor(observation.control_predecessor);
      const producer = observation.producer;
      if (!isRecord(producer)) {
        throw new Error("producer observation state is invalid");
      }
      if (producer.kind === "unclaimed") {
        if (Object.keys(producer).some((key) => key !== "kind") || control_predecessor.kind !== "initialization") {
          throw new Error("unclaimed producer observation contains unexpected fields");
        }
        return { ...coordinate, revision, root_binding: { ...options.rootAttachment }, control_predecessor, producer: { kind: "unclaimed" } };
      }
      if (producer.kind !== "active" && producer.kind !== "relinquished" && producer.kind !== "closed") {
        throw new Error("producer observation state is invalid");
      }
      if (Object.keys(producer).some((key) => key !== "kind" && key !== "producer_incarnation")
        || control_predecessor.kind !== "control"
        || typeof producer.producer_incarnation !== "string"
        || !/^[0-9a-f]{64}$/.test(producer.producer_incarnation)) {
        throw new Error("producer observation incarnation is invalid");
      }
      return { ...coordinate, revision, root_binding: { ...options.rootAttachment }, control_predecessor, producer: {
        kind: producer.kind, producer_incarnation: producer.producer_incarnation,
      } };
  }

  return { attachSeat, observe, relinquish, reconnect, close, observeProducer,
    async settle() {
      if (!closed) throw new Error("session settlement requires document disposal");
      // No new tasks start after close. Rejections from cancelled old work do
      // not revive it; this join only fences successor document attachment.
      await Promise.allSettled([attaching, reproofing, seatSyncPromise, relinquishing]);
    },
    currentReplayCompletion: () => !closed && seatReady && !seatSyncBlocked && !replayInvalidated && replayHandoff
      ? { ...replayHandoff.completion } : null,
    preparedPresentation: () => presentation ? { ...presentation } : null,
    async reconcilePendingAppend() {
      if (seatSyncing) throw new Error("seat append is still in flight");
      await reconcileRetainedAppend();
      if (!closed && !seatSyncBlocked) { capturePendingSeatEvents(); void syncSeat(); }
    },
    canAcceptMutations: () => !closed && seatReady && !seatSyncBlocked && engineBootstrapped,
    currentProducer: () => !closed && seatReady && !seatSyncBlocked && presentation && seatThreadId && seatProducer
      ? { presentation_id: presentation.presentation_id, thread_id: seatThreadId, producer_incarnation: seatProducer } : null,
    currentEngineFrontier: () => !closed && seatReady && !seatSyncBlocked ? nextEngineSeq : null,
    currentPresentation: () => !closed && seatReady && presentation ? { ...presentation } : null,
    completeEngineBootstrap() {
      if (closed || !seatReady || engineBootstrapped) throw new Error("engine bootstrap requires one acknowledged presentation producer");
      engineBootstrapped = true;
      seatObserved = options.seatEvents().length;
    },
  };
}

async function invokeSeat(operation: string, body: unknown): Promise<unknown> {
  return postJson(`/ui/api/session/seat/${operation}`, body);
}

type SeatTouchOutcome = "renewed" | "retry" | "refused";

export function classifySeatTouchResponse(
  value: unknown, presentationId: string, threadId: string,
): SeatTouchOutcome {
  try {
    const acknowledgement = unwrapResult(value);
    return Object.keys(acknowledgement).length === 3
      && acknowledgement.presentation_id === presentationId
      && acknowledgement.thread_id === threadId
      && acknowledgement.touched === true ? "renewed" : "retry";
  } catch {
    return "retry";
  }
}

export function classifySeatTouchError(error: unknown): SeatTouchOutcome {
  return error instanceof HttpResponseError && [403, 404, 409, 410].includes(error.status)
    ? "refused" : "retry";
}

function storageKey(sessionId: string, presentationId: string): string {
  return `ryeos.ui.seat.pending.v2:${encodeURIComponent(sessionId)}:${encodeURIComponent(presentationId)}`;
}

function seatCoordinate(value: Record<string, unknown>): PresentationSeatCoordinate {
  return { presentation_id: requiredStringField(value, "presentation_id"),
    thread_id: requiredStringField(value, "thread_id"), chain_root_id: requiredStringField(value, "chain_root_id") };
}

function requireSeatCoordinate(value: Record<string, unknown>, expected: PresentationSeatCoordinate): void {
  const actual = seatCoordinate(value);
  if (actual.presentation_id !== expected.presentation_id || actual.thread_id !== expected.thread_id
    || actual.chain_root_id !== expected.chain_root_id) throw new Error("seat response changed the retained presentation coordinate");
}

function requireRootBinding(value: unknown, expected: BindingAttachmentCoordinate): void {
  if (!isRecord(value) || value.binding_attachment_id !== expected.binding_attachment_id
    || exactIntegerField(value, "binding_generation") !== expected.binding_generation
    || value.binding_digest !== expected.binding_digest) throw new Error("prepared seat root binding differs from admitted attachment");
}

async function appendWithExactRetry(
  encodedRequest: string,
  stagedEvents: WireSeatEvent[],
  expected: ExpectedAppendAcknowledgement,
  sessionId: string,
  rootAttachment: BindingAttachmentCoordinate,
  presentation: PresentationSeatCoordinate,
): Promise<SeatReceiptCoordinate> {
  const url = "/ui/api/session/seat/append";
  let authenticationReconciled = false;
  for (let attempt = 1; ; attempt += 1) {
    try {
      let response: Record<string, unknown>;
      try {
        response = unwrapResult(await postEncodedJson(url, encodedRequest));
      } catch (error) {
        if (error instanceof UnknownDeliveryError || error instanceof HttpResponseError) throw error;
        throw new UnknownDeliveryError(`seat append acknowledgement is unreadable: ${errorMessage(error)}`);
      }
      try {
        return requireAppendAcknowledgement(response, expected, stagedEvents, presentation);
      } catch (error) {
        // A successful HTTP response does not prove which append outcome was
        // committed when its acknowledgement cannot be validated. Preserve
        // the exact retained request and reconcile; never label this a
        // definitive refusal because a required field was malformed.
        if (error instanceof UnknownDeliveryError) throw error;
        throw new UnknownDeliveryError(`seat append acknowledgement is invalid: ${errorMessage(error)}`);
      }
    } catch (error) {
      if (isAuthenticationResponse(error) && !authenticationReconciled) {
        await requireCurrentSession(sessionId, rootAttachment, presentation);
        authenticationReconciled = true;
        continue;
      }
      // HTTP 408, 429 and 5xx do not prove the daemon rejected this exact
      // append before commit. Preserve its identity through exact retries.
      const outcomeError = error instanceof HttpResponseError
        && (error.status === 408 || error.status === 429 || error.status >= 500)
        ? new UnknownDeliveryError(`seat append response leaves delivery unknown: ${errorMessage(error)}`)
        : error;
      if (!isRetryableResponse(outcomeError) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw outcomeError;
      await new Promise<void>((resolve) => {
        globalThis.setTimeout(resolve, Math.min(2_000, 100 * (2 ** (attempt - 1))));
      });
    }
  }
}

function isAuthenticationResponse(error: unknown): boolean {
  return error instanceof HttpResponseError && (error.status === 401 || error.status === 403);
}

async function requireCurrentSession(
  expectedSessionId: string,
  expectedAttachment: BindingAttachmentCoordinate,
  expectedPresentation: PresentationSeatCoordinate,
): Promise<void> {
  const current = unwrapResult(await getJson("/ui/api/session/current"));
  if (requiredStringField(current, "session_id") !== expectedSessionId) {
    throw new Error("browser session changed while reconciling a seat append");
  }
  const attachments = current.binding_attachments;
  if (!Array.isArray(attachments)) {
    throw new Error("browser session attachments changed while reconciling a seat append");
  }
  const matches = attachments.filter((value) => isRecord(value)
    && value.binding_attachment_id === expectedAttachment.binding_attachment_id
    && exactIntegerField(value, "binding_generation") === BigInt(expectedAttachment.binding_generation)
    && value.binding_digest === expectedAttachment.binding_digest);
  if (matches.length !== 1) {
    throw new Error("browser root attachment changed while reconciling a seat append");
  }
  requireAttachmentSubject(matches[0]);
  const memberships = current.presentations;
  const matching = Array.isArray(memberships) ? memberships.filter((value) => isRecord(value)
    && value.presentation_id === expectedPresentation.presentation_id
    && value.thread_id === expectedPresentation.thread_id
    && value.chain_root_id === expectedPresentation.chain_root_id) : [];
  if (matching.length !== 1 || !isRecord(matching[0])) {
    throw new Error("browser presentation membership changed while reconciling a seat append");
  }
  requireRootBinding(matching[0].root_binding, expectedAttachment);
}

export interface BindingAttachmentCoordinate {
  readonly binding_attachment_id: string;
  readonly binding_generation: bigint;
  readonly binding_digest: string;
}

function isRetryableResponse(error: unknown): boolean {
  return error instanceof UnknownDeliveryError
    || (error instanceof HttpResponseError && error.status >= 500);
}

async function sha256Hex(encoded: string): Promise<string> {
  const bytes = new TextEncoder().encode(encoded);
  const digest = await crypto.subtle.digest("SHA-256", bytes);
  return [...new Uint8Array(digest)].map((byte) => byte.toString(16).padStart(2, "0")).join("");
}

interface ExpectedAppendAcknowledgement {
  readonly producer: string;
  readonly operationId: string;
  readonly firstEngineSeq: bigint;
  readonly lastEngineSeq: bigint;
  readonly eventCount: number;
  readonly payloadDigest: string;
}

interface SeatReceiptCoordinate {
  readonly event_hash: string;
  readonly thread_seq: bigint;
  readonly events: unknown[];
}

interface WireSeatEvent {
  readonly engine_seq: bigint;
  readonly event_type: string;
  readonly payload: unknown;
}

interface RetainedPendingAppend {
  readonly schema_version: "ryeos.ui.seat.pending-append.v3";
  readonly session_id: string;
  readonly presentation_id: string;
  readonly chain_root_id: string;
  readonly thread_id: string;
  readonly producer_incarnation: string;
  readonly operation_id: string;
  readonly first_engine_seq: string;
  readonly last_engine_seq: string;
  readonly event_count: number;
  readonly payload_digest: string;
  readonly encoded_request: string;
}

function retainAppend(key: string, retained: RetainedPendingAppend): void {
  localStorage.setItem(key, JSON.stringify(retained));
}

function clearRetainedAppend(key: string, operationId: string, producer: string, expectedEncodedRequest?: string): void {
  const retained = loadRetainedAppend(key);
  if (retained && retained.operation_id === operationId && retained.producer_incarnation === producer
    && (expectedEncodedRequest === undefined || retained.encoded_request === expectedEncodedRequest)) {
    localStorage.removeItem(key);
  } else if (retained) {
    throw new Error("pending seat append changed during acknowledgement reconciliation");
  }
}

function loadRetainedAppend(key: string): RetainedPendingAppend | null {
  const encoded = localStorage.getItem(key);
  if (encoded === null) return null;
  let value: unknown;
  try {
    value = JSON.parse(encoded);
  } catch (error) {
    throw new Error(`retained seat append is malformed: ${errorMessage(error)}`);
  }
  if (!isRecord(value)
    || value.schema_version !== "ryeos.ui.seat.pending-append.v3"
    || typeof value.session_id !== "string"
    || typeof value.presentation_id !== "string"
    || typeof value.chain_root_id !== "string"
    || typeof value.thread_id !== "string"
    || typeof value.producer_incarnation !== "string"
    || typeof value.operation_id !== "string"
    || typeof value.first_engine_seq !== "string"
    || !/^(0|[1-9][0-9]*)$/.test(value.first_engine_seq)
    || typeof value.last_engine_seq !== "string"
    || !/^(0|[1-9][0-9]*)$/.test(value.last_engine_seq)
    || typeof value.event_count !== "number"
    || !Number.isSafeInteger(value.event_count)
    || value.event_count <= 0
    || typeof value.payload_digest !== "string"
    || typeof value.encoded_request !== "string"
  ) {
    throw new Error("retained seat append has an invalid schema");
  }
  return value as unknown as RetainedPendingAppend;
}

function requireAppendAcknowledgement(
  response: Record<string, unknown>,
  expected: ExpectedAppendAcknowledgement,
  stagedEvents: WireSeatEvent[],
  coordinate: PresentationSeatCoordinate,
): SeatReceiptCoordinate {
  const acknowledgementFields = [
    "producer_incarnation", "operation_id", "first_engine_seq", "last_engine_seq", "event_count",
    "payload_digest", "appended", "chain_seq", "receipt_thread_seq", "receipt_event_hash",
    "events_authority", "events",
  ];
  if (Object.keys(response).some((key) => !acknowledgementFields.includes(key))
    ||
    requiredStringField(response, "producer_incarnation") !== expected.producer
    || requiredStringField(response, "operation_id") !== expected.operationId
    || exactIntegerField(response, "first_engine_seq") !== expected.firstEngineSeq
    || exactIntegerField(response, "last_engine_seq") !== expected.lastEngineSeq
    || exactIntegerField(response, "event_count") !== BigInt(expected.eventCount)
    || exactIntegerField(response, "appended") !== BigInt(expected.eventCount)
    || requiredStringField(response, "payload_digest") !== expected.payloadDigest
  ) {
    throw new UnknownDeliveryError("seat append acknowledgement does not match the submitted operation");
  }
  if (response.events_authority !== "authoritative" || !Array.isArray(response.events)
    || response.events.length !== expected.eventCount || stagedEvents.length !== expected.eventCount) {
    throw new UnknownDeliveryError("seat append acknowledgement omitted its authoritative event interval");
  }
  const receiptChainSeq = exactIntegerField(response, "chain_seq");
  const receiptThreadSeq = exactIntegerField(response, "receipt_thread_seq");
  const receiptHash = requiredStringField(response, "receipt_event_hash");
  if (!/^[0-9a-f]{64}$/.test(receiptHash)) {
    throw new UnknownDeliveryError("seat append acknowledgement receipt hash is invalid");
  }
  const firstChainSeq = receiptChainSeq - BigInt(expected.eventCount);
  if (firstChainSeq < 0n) {
    throw new UnknownDeliveryError("seat append acknowledgement event interval underflows");
  }
  let previous: Record<string, unknown> | null = null;
  const hashes = new Set<string>();
  for (let index = 0; index < stagedEvents.length; index += 1) {
    const acknowledged = response.events[index];
    const staged = stagedEvents[index];
    if (staged === undefined || !isRecord(acknowledged)
      || Object.keys(acknowledged).some((key) => ![
        "event_hash", "chain_root_id", "chain_seq", "thread_id", "thread_seq", "event_type",
        "durability", "ts", "prev_chain_event_hash", "prev_thread_event_hash", "payload",
      ].includes(key))
      || typeof acknowledged.event_hash !== "string"
      || !/^[0-9a-f]{64}$/.test(acknowledged.event_hash)
      || hashes.has(acknowledged.event_hash)
      || acknowledged.chain_root_id !== coordinate.chain_root_id
      || acknowledged.thread_id !== coordinate.thread_id
      || exactIntegerField(acknowledged, "chain_seq") !== firstChainSeq + BigInt(index)
      || acknowledged.event_type !== staged.event_type
      || acknowledged.durability !== "durable"
      || typeof acknowledged.ts !== "string" || acknowledged.ts.length === 0
      || !canonicalOptionalHash(acknowledged.prev_chain_event_hash)
      || !canonicalOptionalHash(acknowledged.prev_thread_event_hash)
      || !isRecord(acknowledged.payload)
      || exactIntegerField(acknowledged.payload, "seq") !== staged.engine_seq
      || encodeSeatPayloadDigest(acknowledged.payload.payload) !== encodeSeatPayloadDigest(staged.payload)) {
      throw new UnknownDeliveryError("seat append acknowledgement changed its authoritative event interval");
    }
    hashes.add(acknowledged.event_hash);
    const threadSeq = exactIntegerField(acknowledged, "thread_seq");
    if (previous !== null && (
      threadSeq !== exactIntegerField(previous, "thread_seq") + 1n
      || acknowledged.prev_chain_event_hash !== previous.event_hash
      || acknowledged.prev_thread_event_hash !== previous.event_hash
    )) {
      throw new UnknownDeliveryError("seat append acknowledgement event links are not contiguous");
    }
    previous = acknowledged;
  }
  if (previous === null || receiptThreadSeq !== exactIntegerField(previous, "thread_seq") + 1n) {
    throw new UnknownDeliveryError("seat append acknowledgement receipt is not adjacent to its event interval");
  }
  return { event_hash: receiptHash, thread_seq: receiptThreadSeq, events: response.events };
}

function retainedAppendEvents(
  retained: RetainedPendingAppend,
  expected: ExpectedAppendAcknowledgement,
  coordinate: PresentationSeatCoordinate,
): WireSeatEvent[] {
  const request = decodeJsonBody(retained.encoded_request);
  if (!isRecord(request)
    || Object.keys(request).some((key) => ![
      "presentation_id", "thread_id", "producer_incarnation", "operation_id", "first_engine_seq",
      "last_engine_seq", "event_count", "payload_digest", "events",
    ].includes(key))
    || request.presentation_id !== coordinate.presentation_id
    || request.thread_id !== coordinate.thread_id
    || request.producer_incarnation !== expected.producer
    || request.operation_id !== expected.operationId
    || exactIntegerField(request, "first_engine_seq") !== expected.firstEngineSeq
    || exactIntegerField(request, "last_engine_seq") !== expected.lastEngineSeq
    || exactIntegerField(request, "event_count") !== BigInt(expected.eventCount)
    || request.payload_digest !== expected.payloadDigest
    || !Array.isArray(request.events)
    || request.events.length !== expected.eventCount) {
    throw new Error("retained append request differs from its exact staged interval");
  }
  return request.events.map((event, index) => {
    if (!isRecord(event)
      || exactIntegerField(event, "engine_seq") !== expected.firstEngineSeq + BigInt(index)
      || typeof event.event_type !== "string" || event.event_type.length === 0
      || !("payload" in event)) {
      throw new Error("retained append contains an invalid staged event interval");
    }
    return {
      engine_seq: exactIntegerField(event, "engine_seq"),
      event_type: event.event_type,
      payload: event.payload,
    };
  });
}

function canonicalOptionalHash(value: unknown): boolean {
  return value === null || (typeof value === "string" && /^[0-9a-f]{64}$/.test(value));
}

function unwrapResult(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) throw new Error("seat service returned a non-object response");
  const first = isRecord(value.result) ? value.result : value;
  return isRecord(first.result) ? first.result : first;
}

function stringField(value: Record<string, unknown>, name: string): string | null {
  return typeof value[name] === "string" ? value[name] : null;
}

function requiredStringField(value: Record<string, unknown>, name: string): string {
  const field = stringField(value, name);
  if (field === null || field.length === 0) throw new Error(`seat service returned no ${name}`);
  return field;
}

function exactU64Field(value: Record<string, unknown>, name: string): bigint {
  const integer = exactIntegerField(value, name);
  if (integer > MAX_U64) throw new Error(`seat service returned an out-of-range u64 for ${name}`);
  return integer;
}

function exactIntegerField(value: Record<string, unknown>, name: string): bigint {
  const field = value[name];
  if (field === null || field === undefined) throw new Error(`seat service returned no ${name}`);
  return parseExactInteger(field, name);
}

function parseExactInteger(value: unknown, name: string): bigint {
  if (typeof value === "bigint" && value >= 0n) return value;
  if (typeof value === "number" && Number.isSafeInteger(value) && value >= 0) return BigInt(value);
  if (typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value)) return BigInt(value);
  throw new Error(`seat service returned an invalid exact integer for ${name}`);
}

function eventType(event: SeatEvent): string {
  return event.event_type;
}

function eventPayload(event: SeatEvent): unknown {
  return event.payload;
}

function wireSeatEvents(events: SeatEvent[], firstEngineSeq: bigint): WireSeatEvent[] {
  return events.map((event, offset) => {
    const engineSeq = firstEngineSeq + BigInt(offset);
    if (engineSeq < 0n || engineSeq >= MAX_U64) {
      throw new Error("producer engine interval is exhausted");
    }
    const payload = eventPayload(event);
    if (event.producer_engine_seq !== undefined && event.producer_engine_seq !== null
      && parseExactInteger(event.producer_engine_seq, "producer_engine_seq") !== engineSeq) {
      throw new Error("captured event cannot be rebased to another producer interval");
    }
    if (eventType(event) === "seat.presentation_operation.prepared"
      && (!isRecord(payload) || (payload.schema !== 2 && payload.schema !== 2n)
        || exactIntegerField(payload, "prepared_engine_seq") !== engineSeq)) {
      throw new Error("prepared operation differs from its reserved producer position");
    }
    return { engine_seq: engineSeq, event_type: eventType(event), payload };
  });
}

function encodedSeatEventBytes(event: SeatEvent): number {
  return encodedByteLength(encodeJsonBody({
    engine_seq: event.seq,
    event_type: eventType(event),
    payload: eventPayload(event),
  }));
}

function encodedByteLength(encoded: string): number {
  return new TextEncoder().encode(encoded).byteLength;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}

function controlPredecessor(value: unknown): UiSeatControlPredecessor {
  if (!isRecord(value) || (value.kind !== "initialization" && value.kind !== "control")
    || Object.keys(value).some((key) => key !== "kind" && key !== "event_hash")
    || typeof value.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(value.event_hash)) {
    throw new Error("producer observation omitted its exact control predecessor");
  }
  return { kind: value.kind, event_hash: value.event_hash };
}
