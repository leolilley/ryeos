import type {
  RyeOsEvent,
  RyeOsTransportChannel,
  RyeOsTransportFreshness,
  RyeOsViewModel,
  SeatEvent,
} from "../generated";
import { postJson } from "./transport";

interface SessionRuntimeOptions {
  readonly eventsUrl: string | null;
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
  let seatThreadId: string | null = null;
  let seatSynced = 0;
  let seatSyncing = false;
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
    const opened = unwrapResult(await invokeSeat("open", {}));
    seatThreadId = stringField(opened, "thread_id");
    if (!seatThreadId) throw new Error("seat/open returned no durable seat thread");
    heartbeat = window.setInterval(() => {
      if (seatThreadId) void invokeSeat("touch", { thread_id: seatThreadId });
    }, 60_000);
    if (opened.reattached === true) {
      const replay = unwrapResult(await invokeSeat("replay", { chain_root_id: seatThreadId }));
      const replayEvents = Array.isArray(replay.events) ? replay.events : [];
      if (replayEvents.length > 0) options.replaySeatEvents(replayEvents);
    }
    seatSynced = options.seatEvents().length;
  }

  function attachEvents(): void {
    if (!options.eventsUrl) return;
    events?.close();
    const source = new EventSource(options.eventsUrl);
    events = source;
    const forward = (event: MessageEvent<string>) => {
      try {
        options.commitEvent({ type: "tick", now_ms: BigInt(Date.now()) });
        options.commitEvent({ type: "daemon_event", payload: JSON.parse(event.data) as unknown });
      } catch (error) { console.warn("Failed to process RyeOS session event", error); }
    };
    source.addEventListener("message", forward);
    source.addEventListener("ui_intent.applied", forward as EventListener);
    source.addEventListener("thread.hint", ((event: MessageEvent<string>) => {
      try {
        const payload: unknown = JSON.parse(event.data);
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
    void syncSeat();
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
        const frame: unknown = JSON.parse(event.data);
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
    if (!seatThreadId || seatSyncing) return;
    const all = options.seatEvents();
    if (all.length <= seatSynced) return;
    const target = all.length;
    const batch = all.slice(seatSynced).map((event) => ({
      event_type: eventType(event),
      payload: { seq: event.seq, payload: eventPayload(event) },
    }));
    seatSyncing = true;
    try {
      await invokeSeat("append", { thread_id: seatThreadId, events: batch });
      seatSynced = target;
    } catch (error) { console.warn("RyeOS seat sync failed", error); }
    finally { seatSyncing = false; if (options.seatEvents().length > seatSynced) void syncSeat(); }
  }

  function close(): void {
    events?.close(); tail?.close();
    if (heartbeat !== null) window.clearInterval(heartbeat);
    if (hintTimer !== null) window.clearTimeout(hintTimer);
    if (seatThreadId) {
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

function unwrapResult(value: unknown): Record<string, unknown> {
  if (!isRecord(value)) throw new Error("seat service returned a non-object response");
  const first = isRecord(value.result) ? value.result : value;
  return isRecord(first.result) ? first.result : first;
}

function stringField(value: Record<string, unknown>, name: string): string | null {
  return typeof value[name] === "string" ? value[name] : null;
}

function eventType(event: SeatEvent): string {
  return event.event_type;
}

function eventPayload(event: SeatEvent): unknown {
  return event.payload;
}

function isRecord(value: unknown): value is Record<string, unknown> {
  return typeof value === "object" && value !== null && !Array.isArray(value);
}
