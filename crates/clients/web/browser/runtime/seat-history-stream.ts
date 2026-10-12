import { decodeJsonBody, encodeJsonBody, HttpResponseError } from "./transport";

// The state reader admits up to 8 MiB of locator/CAS records per batch;
// JSON framing can add overhead, but one frame is still bounded.
const MAX_FRAME_BYTES = 16 * 1024 * 1024;
const CHUNK_SLICE_CHARS = 64 * 1024;
const MAX_BATCH = 64;
const U64_MAX = 18_446_744_073_709_551_615n;

/** A provisional proof ended without a completion fence. No staged state is authoritative. */
export class SeatHistoryIncompleteError extends Error {}

/** A completed cut needs replacement; this wake carries no phase authority. */
export class SeatHistoryReproofRequiredError extends Error {}

export interface SeatHistoryCoordinate {
  readonly presentation_id: string;
  readonly chain_root_id: string;
  readonly thread_id: string;
}

export interface SeatHistoryClaim {
  readonly operation_id: string;
  readonly producer_incarnation: string;
  readonly next_engine_seq: bigint;
}

export interface SeatHistoryCompletion {
  readonly tip_hash: string;
  readonly thread_seq: bigint;
  readonly event_count: bigint;
}

export interface SeatHistoryHandoff {
  readonly completion: SeatHistoryCompletion;
  close(): void;
}

export interface SeatVerifiedAppend {
  readonly operation_id: string;
  readonly producer_incarnation: string;
  readonly first_engine_seq: bigint;
  readonly last_engine_seq: bigint;
  readonly event_count: bigint;
  readonly payload_digest: string;
  readonly event_hash: string;
  readonly thread_seq: bigint;
}

type Frame = { readonly event: string; readonly data: Record<string, unknown> };
type SeatHistoryControlTip = {
  readonly schema_version: unknown;
  readonly operation_id: unknown;
  readonly producer_incarnation: unknown;
  readonly state: unknown;
  readonly next_engine_seq: bigint;
  readonly request_action: unknown;
  readonly request_operation_id: unknown;
  readonly request_presentation_id: unknown;
  readonly request_chain_root_id: unknown;
  readonly request_thread_id: unknown;
};

function controlTip(payload: Record<string, unknown>): SeatHistoryControlTip {
  if (!record(payload.request)) throw new Error("seat history producer control has no request");
  // Copy only primitive testimony before exposing the provisional batch to
  // staging. A callback cannot change which control the completion compares.
  return {
    schema_version: payload.schema_version,
    operation_id: payload.operation_id,
    producer_incarnation: payload.producer_incarnation,
    state: payload.state,
    next_engine_seq: exactU64(payload.next_engine_seq),
    request_action: payload.request.action,
    request_operation_id: payload.request.operation_id,
    request_presentation_id: payload.request.presentation_id,
    request_chain_root_id: payload.request.chain_root_id,
    request_thread_id: payload.request.thread_id,
  };
}

/** A fresh POST and a fresh proof on every call; no EventSource reconnect cursor. */
export async function consumeSeatHistory(
  coordinate: SeatHistoryCoordinate,
  claim: SeatHistoryClaim,
  signal: AbortSignal,
  stage: (records: Record<string, unknown>[]) => void,
  onInvalidated: (reason: Error) => void,
  onVerifiedAppend: (append: SeatVerifiedAppend) => void,
  stageTransferProof: (proof: unknown) => void,
  stageMoveFreezeCancellationProof: (proof: unknown) => void,
): Promise<SeatHistoryHandoff> {
  // Retain the original admitted expectation before fetch or any provisional
  // staging callback. TypeScript readonly does not freeze caller-owned objects.
  const seat = { ...coordinate };
  const expectedClaim = { ...claim };
  const response = await fetch("/ui/api/session/seat/history", {
    method: "POST",
    credentials: "same-origin",
    headers: { "Content-Type": "application/json", "Accept": "text/event-stream" },
    body: encodeJsonBody({ presentation_id: seat.presentation_id, chain_root_id: seat.chain_root_id }),
    signal,
  });
  if (!response.ok) throw new HttpResponseError(response.status, `seat history refused: ${response.status}`);
  if (!response.body || !response.headers.get("content-type")?.toLowerCase().startsWith("text/event-stream")) {
    throw new Error("seat history did not return an event stream");
  }
  const reader = response.body.getReader();
  const decoder = new TextDecoder("utf-8", { fatal: true });
  let buffer = "";
  let bufferedBytes = 0;
  let began = false;
  let complete: SeatHistoryCompletion | null = null;
  let eventCount = 0n;
  let lastThreadSeq = 0n;
  let lastEventHash: string | null = null;
  let lastControl: SeatHistoryControlTip | null = null;
  const recentVerified = new Map<string, { threadSeq: bigint; eventHash: string }>();
  let settled = false;
  let stopped = false;
  const stop = () => { stopped = true; void reader.cancel().catch(() => {}); };
  return new Promise<SeatHistoryHandoff>((resolve, reject) => {
    const fail = (error: Error) => {
      if (stopped) return;
      // Fence reentrant abort before calling client code. A notification may
      // itself abort this signal or throw; neither may deliver a second notice.
      stop();
      if (settled) onInvalidated(error);
      else reject(error);
    };
    const onAbort = () => fail(new Error("seat history read was aborted"));
    signal.addEventListener("abort", onAbort, { once: true });
    void (async () => {
    try {
    for (;;) {
      signal.throwIfAborted();
      const chunk = await reader.read();
      const decoded = decoder.decode(chunk.value, { stream: !chunk.done });
      // A network read may contain many frames or one oversized complete
      // frame. Slice it so no unparsed accumulation bypasses the frame bound.
      for (let offset = 0; offset < decoded.length; offset += CHUNK_SLICE_CHARS) {
        const scanFrom = Math.max(0, buffer.length - 3);
        const piece = decoded.slice(offset, offset + CHUNK_SLICE_CHARS);
        buffer += piece;
        // Encoding separate UTF-16 pieces can overcount a split surrogate,
        // never undercount it. Recalculate only near the actual limit.
        bufferedBytes += new TextEncoder().encode(piece).length;
        const separator = /\r?\n\r?\n/g;
        separator.lastIndex = scanFrom;
        let boundary: RegExpExecArray | null;
        while ((boundary = separator.exec(buffer)) !== null) {
        if (stopped) return;
        const raw = buffer.slice(0, boundary.index);
        buffer = buffer.slice(boundary.index + boundary[0].length);
        bufferedBytes = new TextEncoder().encode(buffer).length;
        separator.lastIndex = 0;
        if (new TextEncoder().encode(raw).length > MAX_FRAME_BYTES) {
          throw new Error("seat history frame exceeds bound");
        }
        if (!raw || raw.split(/\r?\n/).every((line) => line.startsWith(":"))) continue;
        const frame = parseFrame(raw);
        if (complete) {
          if (frame.event === "ui_seat.replay.append_verified.v1") {
            const append = verifiedAppend(frame.data);
            const duplicate = recentVerified.get(append.operation_id);
            if (duplicate &&
              (duplicate.threadSeq !== append.thread_seq || duplicate.eventHash !== append.event_hash)) {
              throw new Error("seat history verified append reused an operation with different testimony");
            }
            if (append.thread_seq < lastThreadSeq &&
              (duplicate?.threadSeq !== append.thread_seq || duplicate.eventHash !== append.event_hash)) {
              throw new Error("seat history verified append moved backwards");
            }
            if (append.thread_seq === lastThreadSeq && append.event_hash !== lastEventHash) {
              throw new Error("seat history verified append changed the retained tip");
            }
            if (append.thread_seq > lastThreadSeq) {
              lastThreadSeq = append.thread_seq;
              lastEventHash = append.event_hash;
            }
            if (!duplicate) {
              recentVerified.set(append.operation_id, { threadSeq: append.thread_seq, eventHash: append.event_hash });
              if (recentVerified.size > 128) recentVerified.delete(recentVerified.keys().next().value!);
            }
            // Commit the parser's own primitive suffix cursor before exposing
            // this notification to a caller-owned callback.
            onVerifiedAppend(append);
            // close() can be called synchronously from that callback while a
            // fetch chunk still contains more frames. Do not deliver them.
            if (stopped) return;
            continue;
          }
          if (frame.event === "ui_seat.replay.reproof_required.v1" && frame.data.schema === 1n
            && Object.keys(frame.data).length === 1) {
            // Stop first, just like invalidation. A callback may begin a fresh
            // fixed-cut read, but no remaining bytes belong to that candidate.
            fail(new SeatHistoryReproofRequiredError("seat history requires a fresh paired proof"));
            return;
          }
          if (frame.event === "ui_seat.replay.invalidated.v1" && frame.data.schema === 1n
            && Object.keys(frame.data).length === 1) {
            fail(new Error("seat history was invalidated"));
            return;
          }
          throw new Error("seat history emitted unsupported post-completion frame");
        }
        if (frame.event === "ui_seat.replay.begin.v1") {
          if (began || frame.data.schema !== 1n || frame.data.presentation_id !== seat.presentation_id
            || frame.data.chain_root_id !== seat.chain_root_id || frame.data.thread_id !== seat.thread_id) {
            throw new Error("seat history begin does not match the claimed seat");
          }
          began = true;
        } else if (frame.event === "ui_seat.replay.events.v1") {
          if (!began || frame.data.schema !== 1n || !Array.isArray(frame.data.events)
            || frame.data.events.length === 0 || frame.data.events.length > MAX_BATCH) {
            throw new Error("seat history batch has invalid shape or order");
          }
          const records: Record<string, unknown>[] = [];
          for (const entry of frame.data.events) {
            if (!record(entry) || entry.thread_id !== seat.thread_id
              || entry.chain_root_id !== seat.chain_root_id
              || typeof entry.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(entry.event_hash)) {
              throw new Error("seat history event has invalid retained identity");
            }
            const sequence = exactU64(entry.thread_seq);
            // The server proves the complete signed interval. Its provisional
            // stream omits non-fold events (notably thread_created), so the
            // client checks monotone emitted order, not false adjacency.
            if (sequence <= lastThreadSeq) throw new Error("seat history event order moved backwards");
            if (lastEventHash !== null && sequence === lastThreadSeq + 1n
              && entry.prev_thread_event_hash !== lastEventHash) {
              throw new Error("seat history predecessor is discontinuous");
            }
            lastThreadSeq = sequence;
            lastEventHash = entry.event_hash;
            if (entry.event_type === "ui_seat.producer.v3") {
              if (!record(entry.payload)) throw new Error("seat history producer control has no payload");
              lastControl = controlTip(entry.payload);
            }
            eventCount++;
            records.push(entry);
          }
          stage(records);
          // Staging is caller-owned and can synchronously abort the document.
          // A decoded chunk may still contain more provisional frames; none
          // may reach a disposed candidate after the abort fence.
          if (stopped) return;
        } else if (frame.event === "ui_seat.replay.transfer_proof.v1") {
          if (!began || frame.data.schema !== 1n || Object.keys(frame.data).length !== 2
            || !record(frame.data.proof)) {
            throw new Error("seat history transfer proof has invalid shape or order");
          }
          // The frame byte budget was checked before JSON decoding. Shared
          // Rust validates its one-operation closed proof; transport does not
          // construct another lifecycle or alter the canonical event cursor.
          stageTransferProof(frame.data.proof);
          if (stopped) return;
        } else if (frame.event === "ui_seat.replay.move_freeze_cancellation_proof.v1") {
          if (!began || frame.data.schema !== 1n || Object.keys(frame.data).length !== 2
            || !record(frame.data.proof)) {
            throw new Error("seat history move freeze cancellation proof has invalid shape or order");
          }
          stageMoveFreezeCancellationProof(frame.data.proof);
          if (stopped) return;
        } else if (frame.event === "ui_seat.replay.complete.v1") {
          if (!began || frame.data.schema !== 1n || typeof frame.data.tip_hash !== "string"
            || !/^[0-9a-f]{64}$/.test(frame.data.tip_hash)) {
            throw new Error("seat history completion is invalid");
          }
          const count = exactU64(frame.data.event_count);
          const tipSequence = exactU64(frame.data.thread_seq);
          if (count !== eventCount || tipSequence !== lastThreadSeq
            || (lastEventHash !== null && frame.data.tip_hash !== lastEventHash)) {
            throw new Error("seat history completion does not match staged events");
          }
          if (!lastControl || lastControl.schema_version !== "ryeos.ui.seat.producer.v3"
            || lastControl.operation_id !== expectedClaim.operation_id
            || lastControl.producer_incarnation !== expectedClaim.producer_incarnation
            || lastControl.state !== "active"
            || lastControl.next_engine_seq !== expectedClaim.next_engine_seq
            || lastControl.request_action !== "claim"
            || lastControl.request_operation_id !== expectedClaim.operation_id
            || lastControl.request_presentation_id !== seat.presentation_id
            || lastControl.request_chain_root_id !== seat.chain_root_id
            || lastControl.request_thread_id !== seat.thread_id) {
            throw new Error("seat history completion does not retain the acknowledged producer claim");
          }
          complete = { tip_hash: frame.data.tip_hash, thread_seq: tipSequence, event_count: count };
          settled = true;
          resolve({ completion: complete, close: stop });
        } else if (frame.event === "ui_seat.replay.invalidated.v1" && began
          && frame.data.schema === 1n && Object.keys(frame.data).length === 1) {
          fail(new SeatHistoryIncompleteError("seat history was invalidated before completion"));
          return;
        } else {
          throw new Error(`seat history has unknown frame ${frame.event}`);
        }
        }
        if (bufferedBytes > MAX_FRAME_BYTES) {
          bufferedBytes = new TextEncoder().encode(buffer).length;
          if (bufferedBytes > MAX_FRAME_BYTES) throw new Error("seat history frame exceeds bound");
        }
      }
      if (chunk.done) break;
    }
    if (!stopped) fail(settled
      ? new Error("seat history live handoff ended")
      : new SeatHistoryIncompleteError(buffer.trim()
        ? "seat history ended with an incomplete frame" : "seat history stream ended before completion"));
    } catch (error) {
      if (!stopped) fail(error instanceof Error ? error : new Error(String(error)));
      else if (!settled) reject(error);
    } finally {
      signal.removeEventListener("abort", onAbort);
      if (!stopped) stop();
    }
    })();
  });
}

function verifiedAppend(data: Record<string, unknown>): SeatVerifiedAppend {
  const fields = ["schema", "operation_id", "producer_incarnation", "first_engine_seq",
    "last_engine_seq", "event_count", "payload_digest", "event_hash", "thread_seq"];
  if (Object.keys(data).length !== fields.length || fields.some((field) => !Object.hasOwn(data, field))
    || data.schema !== 1n || typeof data.operation_id !== "string"
    || !/^[0-9a-f]{8}-[0-9a-f]{4}-8[0-9a-f]{3}-[89ab][0-9a-f]{3}-[0-9a-f]{12}$/.test(data.operation_id)
    || typeof data.producer_incarnation !== "string" || !/^[0-9a-f]{64}$/.test(data.producer_incarnation)
    || typeof data.payload_digest !== "string" || !/^[0-9a-f]{64}$/.test(data.payload_digest)
    || typeof data.event_hash !== "string" || !/^[0-9a-f]{64}$/.test(data.event_hash)) {
    throw new Error("seat history verified append has invalid shape");
  }
  const first = exactU64(data.first_engine_seq);
  const last = exactU64(data.last_engine_seq);
  const count = exactU64(data.event_count);
  const threadSeq = exactU64(data.thread_seq);
  if (count === 0n || first > last || last - first + 1n !== count || threadSeq === 0n) {
    throw new Error("seat history verified append has invalid interval");
  }
  return { operation_id: data.operation_id, producer_incarnation: data.producer_incarnation,
    first_engine_seq: first, last_engine_seq: last, event_count: count,
    payload_digest: data.payload_digest, event_hash: data.event_hash, thread_seq: threadSeq };
}

function parseFrame(raw: string): Frame {
  let event: string | null = null;
  let data: string | null = null;
  for (const line of raw.split(/\r?\n/)) {
    if (line.startsWith(":")) continue;
    if (line.startsWith("event: ") && event === null) event = line.slice(7);
    else if (line.startsWith("data: ") && data === null) data = line.slice(6);
    else throw new Error("seat history has duplicate or unsupported SSE field");
  }
  if (!event || data === null) throw new Error("seat history frame omitted event or payload");
  const decoded = decodeJsonBody(data);
  if (!record(decoded)) throw new Error("seat history frame payload is not an object");
  return { event, data: decoded };
}

function record(value: unknown): value is Record<string, unknown> {
  return value !== null && typeof value === "object" && !Array.isArray(value);
}

function exactU64(value: unknown): bigint {
  const number = typeof value === "bigint" ? value
    : typeof value === "number" && Number.isSafeInteger(value) ? BigInt(value)
    : typeof value === "string" && /^(0|[1-9][0-9]*)$/.test(value) ? BigInt(value)
    : null;
  if (number === null || number < 0n || number > U64_MAX) throw new Error("seat history integer is invalid");
  return number;
}
