import type {
  RyeOsEvent,
  RyeOsTransportChannel,
  RyeOsTransportFreshness,
  RyeOsViewModel,
  SeatEvent,
} from "../generated";
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
const MAX_SEAT_APPEND_ATTEMPTS = 5;
const MAX_SEAT_APPEND_BODY_BYTES = 65_536;
const MAX_PENDING_SEAT_EVENTS = 512;
const MAX_PENDING_SEAT_BYTES = 512 * 1_024;

interface SessionRuntimeOptions {
  readonly sessionId: string;
  readonly eventsUrl: string | null;
  readonly surfaceAttachment: BindingAttachmentCoordinate;
  readonly seatEvents: () => SeatEvent[];
  readonly commitEvent: (event: RyeOsEvent) => void;
  readonly replaySeatEvents: (events: unknown[]) => void;
}

export interface SessionRuntime {
  attachSeat(): Promise<void>;
  observe(model: RyeOsViewModel): void;
  close(): void;
}

export function createSessionRuntime(options: SessionRuntimeOptions): SessionRuntime {
  const pendingStorageKey = `ryeos.ui.seat.pending.v1:${options.sessionId}`;
  let seatThreadId: string | null = null;
  let seatProducer: string | null = null;
  let nextEngineSeq = 0n;
  let seatObserved = 0;
  const pendingSeatEvents: SeatEvent[] = [];
  let pendingSeatBytes = 0;
  let seatAttaching = false;
  let seatReady = false;
  let seatSyncing = false;
  let seatSyncBlocked = false;
  let closed = false;
  let seatEpoch = 0;
  let heartbeat: number | null = null;
  let events: EventSource | null = null;
  let eventsOpened = false;
  let tail: EventSource | null = null;
  let tailUrl: string | null = null;
  let tailThreadId: string | null = null;
  let hintTimer: number | null = null;
  const dirtyHints = new Set<string>();

  function commitTransport(channel: RyeOsTransportChannel, freshness: RyeOsTransportFreshness): void {
    options.commitEvent({
      type: "transport_state_changed", channel, freshness,
      observed_at_ms: BigInt(Date.now()), error: null,
    });
  }

  async function attachSeat(): Promise<void> {
    const epoch = ++seatEpoch;
    seatAttaching = true;
    seatSyncBlocked = false;
    const startupBaseline = options.seatEvents().slice();
    seatObserved = startupBaseline.length;
    await reconcileRetainedAppend();
    if (closed || epoch !== seatEpoch) return;
    const opened = unwrapResult(await invokeSeat("open", options.surfaceAttachment));
    if (closed || epoch !== seatEpoch) return;
    seatThreadId = stringField(opened, "thread_id");
    if (!seatThreadId) throw new Error("seat/open returned no durable seat thread");
    seatProducer = requiredStringField(opened, "producer_incarnation");
    nextEngineSeq = exactIntegerField(opened, "next_engine_seq");
    capturePendingSeatEvents();
    if (heartbeat !== null) window.clearInterval(heartbeat);
    heartbeat = window.setInterval(() => {
      if (seatThreadId) void invokeSeat("touch", { thread_id: seatThreadId });
    }, 60_000);
    if (opened.reattached === true) {
      let cursor: bigint | null = null;
      do {
        const replay = unwrapResult(await invokeSeat("replay", {
          chain_root_id: seatThreadId,
          after_chain_seq: cursor,
          limit: 500,
        }));
        if (closed || epoch !== seatEpoch) return;
        capturePendingSeatEvents();
        const replayEvents = Array.isArray(replay.events) ? replay.events : [];
        if (replayEvents.length > 0) options.replaySeatEvents(replayEvents);
        seatObserved = options.seatEvents().length;
        cursor = optionalExactIntegerField(replay, "next_cursor");
      } while (cursor !== null);
      if (nextEngineSeq === 0n) {
        prependPendingSeatEvents(startupBaseline);
      } else if (pendingSeatEvents.length > 0) {
        // Replay is older authority and may have replaced provisional local
        // sequence slots. Re-apply pending mutations at the producer's exact
        // durable interval so the fold remains authority-ordered while the
        // identical events await acknowledgement.
        const rebased = wireSeatEvents(pendingSeatEvents, nextEngineSeq).map((event) => ({
          event_type: event.event_type,
          payload: { seq: event.engine_seq, payload: event.payload },
        }));
        options.replaySeatEvents(rebased);
        seatObserved = options.seatEvents().length;
      }
    } else {
      // A new seat has no durable history. Initial engine output is therefore
      // part of the first producer batch and must precede mutations that
      // arrived while open was in flight.
      prependPendingSeatEvents(startupBaseline);
    }
    capturePendingSeatEvents();
    seatAttaching = false;
    seatReady = true;
    void syncSeat();
  }

  async function reconcileRetainedAppend(): Promise<void> {
    const retained = loadRetainedAppend(pendingStorageKey);
    if (!retained) return;
    if (retained.session_id !== options.sessionId) {
      throw new Error("retained seat append belongs to a different browser session");
    }
    await appendWithExactRetry(retained.encoded_request, {
      producer: retained.producer_incarnation,
      operationId: retained.operation_id,
      firstEngineSeq: BigInt(retained.first_engine_seq),
      lastEngineSeq: BigInt(retained.last_engine_seq),
      eventCount: retained.event_count,
      payloadDigest: retained.payload_digest,
    }, options.sessionId, options.surfaceAttachment);
    clearRetainedAppend(pendingStorageKey);
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

  function prependPendingSeatEvents(events: SeatEvent[]): void {
    let encodedBytes: number;
    try {
      encodedBytes = events.reduce((total, event) => total + encodedSeatEventBytes(event), 0);
    } catch (error) {
      seatSyncBlocked = true;
      reportSeatFailure(error, false);
      return;
    }
    if (
      pendingSeatEvents.length + events.length > MAX_PENDING_SEAT_EVENTS
      || pendingSeatBytes + encodedBytes > MAX_PENDING_SEAT_BYTES
    ) {
      seatSyncBlocked = true;
      reportSeatFailure(new Error(
        `unsaved seat history exceeds the pending limit (${MAX_PENDING_SEAT_EVENTS} events / ${MAX_PENDING_SEAT_BYTES} bytes)`,
      ), false);
      return;
    }
    pendingSeatEvents.unshift(...events);
    pendingSeatBytes += encodedBytes;
  }

  function syncTail(model: RyeOsViewModel): void {
    tailThreadId = model.tail_thread_id ?? model.tail_chain_root_id ?? null;
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
        if (!isRecord(frame) || typeof frame.event_type !== "string" || !tailThreadId) return;
        options.commitEvent({
          type: "thread_tail", thread_id: tailThreadId,
          event_type: frame.event_type, payload: frame.payload ?? null,
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
    if (closed || !seatThreadId || !seatProducer || seatSyncing || seatSyncBlocked) return;
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
      operationId = crypto.randomUUID();
      while (batchEvents.length > 0) {
        const candidate = wireSeatEvents(batchEvents, firstEngineSeq);
        const candidateLast = firstEngineSeq + BigInt(candidate.length - 1);
        const candidateRequest = encodeJsonBody({
          thread_id: threadId,
          producer_incarnation: producer,
          operation_id: operationId,
          first_engine_seq: firstEngineSeq,
          last_engine_seq: candidateLast,
          event_count: candidate.length,
          payload_digest: "0".repeat(64),
          events: candidate,
        });
        if (encodedByteLength(candidateRequest) <= MAX_SEAT_APPEND_BODY_BYTES) break;
        batchEvents = batchEvents.slice(0, -1);
      }
      if (batchEvents.length === 0) {
        throw new Error(`one seat event exceeds the ${MAX_SEAT_APPEND_BODY_BYTES}-byte append route limit`);
      }
      batch = wireSeatEvents(batchEvents, firstEngineSeq);
      const actualLastEngineSeq = firstEngineSeq + BigInt(batch.length - 1);
      lastEngineSeq = actualLastEngineSeq;
      payloadDigest = await sha256Hex(encodeSeatPayloadDigest(batch));
      encodedRequest = encodeJsonBody({
        thread_id: threadId,
        producer_incarnation: producer,
        operation_id: operationId,
        first_engine_seq: firstEngineSeq,
        last_engine_seq: lastEngineSeq,
        event_count: batch.length,
        payload_digest: payloadDigest,
        events: batch,
      });
      if (encodedByteLength(encodedRequest) > MAX_SEAT_APPEND_BODY_BYTES) {
        throw new Error("seat append exceeded its route limit after final encoding");
      }
      retainAppend(pendingStorageKey, {
        schema_version: "ryeos.ui.seat.pending-append.v1",
        session_id: options.sessionId,
        thread_id: threadId,
        producer_incarnation: producer,
        operation_id: operationId,
        first_engine_seq: firstEngineSeq.toString(),
        last_engine_seq: lastEngineSeq.toString(),
        event_count: batch.length,
        payload_digest: payloadDigest,
        encoded_request: encodedRequest,
      });
    } catch (error) {
      seatSyncBlocked = true;
      seatSyncing = false;
      reportSeatFailure(error, false);
      return;
    }
    let acknowledged = false;
    try {
      await appendWithExactRetry(encodedRequest, {
        producer, operationId, firstEngineSeq, lastEngineSeq,
        eventCount: batch.length, payloadDigest,
      }, options.sessionId, options.surfaceAttachment);
      if (closed || epoch !== seatEpoch || threadId !== seatThreadId || producer !== seatProducer) return;
      clearRetainedAppend(pendingStorageKey);
      pendingSeatEvents.splice(0, batch.length);
      pendingSeatBytes -= batchEvents.reduce(
        (total, event) => total + encodedSeatEventBytes(event),
        0,
      );
      nextEngineSeq = lastEngineSeq + 1n;
      acknowledged = true;
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
        if (pendingSeatEvents.length > 0) void syncSeat();
      }
    }
  }

  function reportSeatFailure(error: unknown, retryable: boolean): void {
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
  }

  function close(): void {
    if (closed) return;
    closed = true;
    seatEpoch += 1;
    events?.close(); tail?.close();
    if (heartbeat !== null) window.clearInterval(heartbeat);
    if (hintTimer !== null) window.clearTimeout(hintTimer);
    if (seatThreadId && !seatSyncing && pendingSeatEvents.length === 0
      && !hasRetainedAppend(pendingStorageKey)) {
      const body = JSON.stringify({ thread_id: seatThreadId });
      navigator.sendBeacon?.("/ui/api/session/seat/close", new Blob([body], { type: "application/json" }));
    }
    events = null; tail = null; heartbeat = null; hintTimer = null;
  }

  attachEvents();
  return { attachSeat, observe, close };
}

async function invokeSeat(operation: string, body: unknown): Promise<unknown> {
  return postJson(`/ui/api/session/seat/${operation}`, body);
}

async function appendWithExactRetry(
  encodedRequest: string,
  expected: ExpectedAppendAcknowledgement,
  sessionId: string,
  surfaceAttachment: BindingAttachmentCoordinate,
): Promise<void> {
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
      requireAppendAcknowledgement(response, expected);
      return;
    } catch (error) {
      if (isAuthenticationResponse(error) && !authenticationReconciled) {
        await requireCurrentSession(sessionId, surfaceAttachment);
        authenticationReconciled = true;
        continue;
      }
      if (!isRetryableResponse(error) || attempt >= MAX_SEAT_APPEND_ATTEMPTS) throw error;
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
    throw new Error("browser surface attachment changed while reconciling a seat append");
  }
}

interface BindingAttachmentCoordinate {
  readonly binding_attachment_id: string;
  readonly binding_generation: number;
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

interface RetainedPendingAppend {
  readonly schema_version: "ryeos.ui.seat.pending-append.v1";
  readonly session_id: string;
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

function clearRetainedAppend(key: string): void {
  localStorage.removeItem(key);
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
    || value.schema_version !== "ryeos.ui.seat.pending-append.v1"
    || typeof value.session_id !== "string"
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

function hasRetainedAppend(key: string): boolean {
  try {
    return loadRetainedAppend(key) !== null;
  } catch {
    // A malformed or inaccessible recovery record must keep close from
    // settling the seat; retirement would destroy the only reconciliation path.
    return true;
  }
}

function requireAppendAcknowledgement(
  response: Record<string, unknown>,
  expected: ExpectedAppendAcknowledgement,
): void {
  if (
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

function optionalExactIntegerField(value: Record<string, unknown>, name: string): bigint | null {
  const field = value[name];
  if (field === null || field === undefined) return null;
  return parseExactInteger(field, name);
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

function wireSeatEvents(events: SeatEvent[], firstEngineSeq: bigint): Array<{
  engine_seq: bigint;
  event_type: string;
  payload: unknown;
}> {
  return events.map((event, offset) => ({
    engine_seq: firstEngineSeq + BigInt(offset),
    event_type: eventType(event),
    payload: eventPayload(event),
  }));
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
