import assert from "node:assert/strict";
import { webcrypto } from "node:crypto";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const transportMock = `
export class UnknownDeliveryError extends Error { outcome = "unknown"; }
export class HttpResponseError extends Error {
  constructor(status, message) { super(message); this.status = status; }
}
function encode(value, canonical) {
  if (value === null) return "null";
  if (typeof value === "bigint") return value.toString(10);
  if (typeof value === "number" || typeof value === "boolean") return String(value);
  if (typeof value === "string") return JSON.stringify(value);
  if (Array.isArray(value)) return "[" + value.map((item) => encode(item, canonical)).join(",") + "]";
  const keys = Object.keys(value); if (canonical) keys.sort();
  return "{" + keys.map((key) => JSON.stringify(key) + ":" + encode(value[key], canonical)).join(",") + "}";
}
export const encodeJsonBody = (value) => encode(value, false);
export const encodeCanonicalJsonBody = (value) => encode(value, true);
export const encodeSeatPayloadDigest = (value) => encode(value, true);
export const decodeJsonBody = (value) => JSON.parse(value);
export const errorMessage = (error) => error instanceof Error ? error.message : String(error);
export const getJson = (url) => globalThis.__seatGetJson(url);
export const postJson = (url, body) => globalThis.__seatPostJson(url, body);
export async function postEncodedJson(url, body) {
  globalThis.__seatEncodedBodies.push(body);
  if (globalThis.__seatHttpStatusOnce) {
    const status = globalThis.__seatHttpStatusOnce;
    globalThis.__seatHttpStatusOnce = 0;
    throw new HttpResponseError(status, "simulated authentication response");
  }
  if (globalThis.__seatUnknownOnce) {
    globalThis.__seatUnknownOnce = false;
    throw new UnknownDeliveryError("simulated lost response");
  }
  if (globalThis.__seatUnknownAlways) throw new UnknownDeliveryError("simulated exhausted response");
  return globalThis.__seatPostEncodedJson(url, body);
}
`;

async function loadSessionRuntime() {
  const transportUrl = `data:text/javascript;base64,${Buffer.from(transportMock).toString("base64")}`;
  const sourceUrl = new URL("../browser/runtime/session.ts", import.meta.url);
  const source = (await readFile(sourceUrl, "utf8"))
    .replace('from "./transport"', `from ${JSON.stringify(transportUrl)}`);
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  const moduleUrl = `data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`;
  return import(moduleUrl);
}

function installDurableStorage() {
  const values = new Map();
  globalThis.localStorage = {
    getItem: (key) => values.get(key) ?? null,
    setItem: (key, value) => { values.set(key, String(value)); },
    removeItem: (key) => { values.delete(key); },
  };
  return values;
}

function appendAcknowledgement(encoded) {
  const request = JSON.parse(encoded);
  return {
    producer_incarnation: request.producer_incarnation,
    operation_id: request.operation_id,
    first_engine_seq: String(request.first_engine_seq),
    last_engine_seq: String(request.last_engine_seq),
    event_count: request.event_count,
    payload_digest: request.payload_digest,
    appended: request.event_count,
    chain_seq: "4",
  };
}

test("reattach deduplicates startup baseline but preserves mutation during delayed open", async () => {
  globalThis.crypto ??= webcrypto;
  globalThis.window = globalThis;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { sendBeacon: () => true },
  });
  globalThis.__seatEncodedBodies = [];
  globalThis.__seatUnknownOnce = true;
  globalThis.__seatUnknownAlways = false;
  globalThis.__seatHttpStatusOnce = 0;
  globalThis.__seatGetJson = async () => ({ session_id: "session-test" });
  installDurableStorage();

  let resolveOpen;
  const open = new Promise((resolve) => { resolveOpen = resolve; });
  globalThis.__seatPostJson = async (url) => {
    if (url.endsWith("/open")) return open;
    if (url.endsWith("/replay")) {
      return {
        events: [{
          event_type: "seat.facet",
          payload: { seq: 0, payload: { key: "durable", value: true } },
        }],
        next_cursor: null,
      };
    }
    return {};
  };
  globalThis.__seatPostEncodedJson = async (_url, encoded) => appendAcknowledgement(encoded);

  const localEvents = [{
    seq: 0n,
    event_type: "seat.facet",
    payload: { key: "startup", value: "baseline" },
  }];
  const { createSessionRuntime } = await loadSessionRuntime();
  const runtime = createSessionRuntime({
    sessionId: "session-test",
    eventsUrl: null,
    seatEvents: () => localEvents,
    commitEvent: () => {},
    replaySeatEvents: (events) => {
      for (const event of events) {
        localEvents.push({
          seq: BigInt(event.payload.seq),
          event_type: event.event_type,
          payload: event.payload.payload,
        });
      }
    },
  });

  const attaching = runtime.attachSeat();
  localEvents.push({
    seq: 1n,
    event_type: "seat.facet",
    payload: { key: "local", value: `during-open-${"x".repeat(40_000)}` },
  });
  localEvents.push({
    seq: 2n,
    event_type: "seat.facet",
    payload: { key: "local-2", value: `during-open-2-${"y".repeat(40_000)}` },
  });
  resolveOpen({
    thread_id: "T-seat",
    chain_root_id: "T-seat",
    reattached: true,
    producer_incarnation: "a".repeat(64),
    next_engine_seq: "1",
  });
  await attaching;
  assert.deepEqual(
    localEvents.slice(-3).map((event) => [event.seq, event.payload.key]),
    [[0n, "durable"], [1n, "local"], [2n, "local-2"]],
    "durable replay is followed by pending mutations rebased to authoritative sequence",
  );
  for (let attempt = 0; attempt < 80 && globalThis.__seatEncodedBodies.length < 3; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }

  assert.equal(globalThis.__seatEncodedBodies.length, 3, "large events are split after one exact retry");
  assert.equal(globalThis.__seatEncodedBodies[0], globalThis.__seatEncodedBodies[1]);
  const appended = JSON.parse(globalThis.__seatEncodedBodies[1]);
  assert.equal(appended.first_engine_seq, 1);
  assert.equal(appended.last_engine_seq, 1);
  assert.equal(appended.events.length, 1);
  assert.equal(appended.events[0].payload.key, "local");
  assert.match(appended.events[0].payload.value, /^during-open-/);
  assert.equal(appended.events[0].engine_seq, 1);
  const second = JSON.parse(globalThis.__seatEncodedBodies[2]);
  assert.equal(second.first_engine_seq, 2);
  assert.equal(second.last_engine_seq, 2);
  assert.equal(second.events.length, 1);
  assert.equal(second.events[0].payload.key, "local-2");
  for (const body of globalThis.__seatEncodedBodies) {
    assert.ok(Buffer.byteLength(body, "utf8") <= 65_536, "append body stays within signed route bound");
  }
  runtime.close();
});

test("pending seat history blocks visibly at its count bound without dropping into append", async () => {
  globalThis.crypto ??= webcrypto;
  globalThis.window = globalThis;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { sendBeacon: () => true },
  });
  globalThis.__seatEncodedBodies = [];
  globalThis.__seatUnknownOnce = false;
  globalThis.__seatUnknownAlways = false;
  globalThis.__seatHttpStatusOnce = 0;
  globalThis.__seatGetJson = async () => ({ session_id: "session-overflow" });
  installDurableStorage();

  let resolveOpen;
  const open = new Promise((resolve) => { resolveOpen = resolve; });
  globalThis.__seatPostJson = async (url) => url.endsWith("/open") ? open : {};
  globalThis.__seatPostEncodedJson = async () => {
    throw new Error("overflowed pending history must not be appended");
  };

  const localEvents = [];
  const committed = [];
  const { createSessionRuntime } = await loadSessionRuntime();
  const runtime = createSessionRuntime({
    sessionId: "session-overflow",
    eventsUrl: null,
    seatEvents: () => localEvents,
    commitEvent: (event) => committed.push(event),
    replaySeatEvents: () => {},
  });
  const attaching = runtime.attachSeat();
  for (let seq = 0; seq < 513; seq += 1) {
    localEvents.push({
      seq: BigInt(seq),
      event_type: "seat.facet",
      payload: { key: `pending-${seq}`, value: seq },
    });
  }
  resolveOpen({
    thread_id: "T-overflow",
    chain_root_id: "T-overflow",
    reattached: false,
    producer_incarnation: "b".repeat(64),
    next_engine_seq: "0",
  });
  await attaching;
  await new Promise((resolve) => setTimeout(resolve, 20));

  assert.equal(globalThis.__seatEncodedBodies.length, 0);
  const failure = committed.find((event) => event.type === "transport_state_changed" && event.error);
  assert.ok(failure, "overflow produces an explicit unsaved-state transport failure");
  assert.match(failure.error.error, /pending limit/);
  runtime.close();
});

for (const status of [401, 403]) {
  test(`seat append reconciles the current session once after HTTP ${status}`, async () => {
    globalThis.crypto ??= webcrypto;
    globalThis.window = globalThis;
    Object.defineProperty(globalThis, "navigator", {
      configurable: true,
      value: { sendBeacon: () => true },
    });
    globalThis.__seatEncodedBodies = [];
    globalThis.__seatUnknownOnce = false;
    globalThis.__seatUnknownAlways = false;
    globalThis.__seatHttpStatusOnce = status;
    const retained = installDurableStorage();
    let currentCalls = 0;
    globalThis.__seatGetJson = async () => {
      currentCalls += 1;
      return { session_id: `session-${status}` };
    };
    globalThis.__seatPostJson = async (url) => {
      if (url.endsWith("/open")) {
        return {
          thread_id: `T-${status}`,
          reattached: false,
          producer_incarnation: "c".repeat(64),
          next_engine_seq: "0",
        };
      }
      return {};
    };
    globalThis.__seatPostEncodedJson = async (_url, encoded) => appendAcknowledgement(encoded);

    const localEvents = [{
      seq: 0n,
      event_type: "seat.facet",
      payload: { key: "auth", value: status },
    }];
    const { createSessionRuntime } = await loadSessionRuntime();
    const runtime = createSessionRuntime({
      sessionId: `session-${status}`,
      eventsUrl: null,
      seatEvents: () => localEvents,
      commitEvent: () => {},
      replaySeatEvents: () => {},
    });
    await runtime.attachSeat();
    for (let attempt = 0; attempt < 40 && globalThis.__seatEncodedBodies.length < 2; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 10));
    }
    assert.equal(currentCalls, 1);
    assert.equal(globalThis.__seatEncodedBodies.length, 2);
    assert.equal(globalThis.__seatEncodedBodies[0], globalThis.__seatEncodedBodies[1]);
    assert.equal(retained.size, 0, "confirmed authentication retry clears recovery state");
    runtime.close();
  });
}

test("exhausted unknown delivery survives close and reconciles before restart open", async () => {
  globalThis.crypto ??= webcrypto;
  globalThis.window = globalThis;
  let beaconCalls = 0;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { sendBeacon: () => { beaconCalls += 1; return true; } },
  });
  globalThis.__seatEncodedBodies = [];
  globalThis.__seatUnknownOnce = false;
  globalThis.__seatUnknownAlways = true;
  globalThis.__seatHttpStatusOnce = 0;
  globalThis.__seatGetJson = async () => ({ session_id: "session-restart" });
  const retained = installDurableStorage();
  globalThis.__seatPostEncodedJson = async (_url, encoded) => appendAcknowledgement(encoded);
  globalThis.__seatPostJson = async (url) => {
    if (url.endsWith("/open")) {
      return {
        thread_id: "T-restart",
        reattached: false,
        producer_incarnation: "d".repeat(64),
        next_engine_seq: "0",
      };
    }
    return {};
  };

  const { createSessionRuntime } = await loadSessionRuntime();
  const firstEvents = [{
    seq: 0n,
    event_type: "seat.facet",
    payload: { key: "restart", value: "pending" },
  }];
  const first = createSessionRuntime({
    sessionId: "session-restart",
    eventsUrl: null,
    seatEvents: () => firstEvents,
    commitEvent: () => {},
    replaySeatEvents: () => {},
  });
  await first.attachSeat();
  for (let attempt = 0; attempt < 250 && globalThis.__seatEncodedBodies.length < 5; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.equal(globalThis.__seatEncodedBodies.length, 5, "unknown delivery uses a bounded retry budget");
  assert.equal(new Set(globalThis.__seatEncodedBodies).size, 1, "every retry uses identical bytes");
  assert.equal(retained.size, 1, "exhaustion retains the exact operation for restart");
  first.close();
  assert.equal(beaconCalls, 0, "close cannot settle a seat with unresolved history");

  globalThis.__seatUnknownAlways = false;
  let openedAfterReconciliation = false;
  globalThis.__seatPostJson = async (url) => {
    if (url.endsWith("/open")) {
      openedAfterReconciliation = true;
      return {
        thread_id: "T-restart",
        reattached: true,
        producer_incarnation: "e".repeat(64),
        next_engine_seq: "1",
      };
    }
    if (url.endsWith("/replay")) {
      return {
        events: [{
          event_type: "seat.facet",
          payload: { seq: 0, payload: { key: "restart", value: "pending" } },
        }],
        next_cursor: null,
      };
    }
    return {};
  };
  const replayed = [];
  const second = createSessionRuntime({
    sessionId: "session-restart",
    eventsUrl: null,
    seatEvents: () => replayed,
    commitEvent: () => {},
    replaySeatEvents: (events) => { replayed.push(...events); },
  });
  await second.attachSeat();
  assert.equal(openedAfterReconciliation, true);
  assert.equal(globalThis.__seatEncodedBodies.length, 6);
  assert.equal(globalThis.__seatEncodedBodies[5], globalThis.__seatEncodedBodies[0]);
  assert.equal(retained.size, 0, "restart clears recovery only after exact acknowledgement");
  second.close();
  assert.equal(beaconCalls, 1, "settled restarted seat may close normally");
});

test("close racing an in-flight append preserves recovery and does not retire the seat", async () => {
  globalThis.crypto ??= webcrypto;
  globalThis.window = globalThis;
  let beaconCalls = 0;
  Object.defineProperty(globalThis, "navigator", {
    configurable: true,
    value: { sendBeacon: () => { beaconCalls += 1; return true; } },
  });
  globalThis.__seatEncodedBodies = [];
  globalThis.__seatUnknownOnce = false;
  globalThis.__seatUnknownAlways = false;
  globalThis.__seatHttpStatusOnce = 0;
  globalThis.__seatGetJson = async () => ({ session_id: "session-close-race" });
  const retained = installDurableStorage();
  globalThis.__seatPostJson = async (url) => url.endsWith("/open") ? {
    thread_id: "T-close-race",
    reattached: false,
    producer_incarnation: "f".repeat(64),
    next_engine_seq: "0",
  } : {};
  let resolveAppend;
  globalThis.__seatPostEncodedJson = (_url, encoded) => new Promise((resolve) => {
    resolveAppend = () => resolve(appendAcknowledgement(encoded));
  });

  const events = [{
    seq: 0n,
    event_type: "seat.facet",
    payload: { key: "close", value: "racing" },
  }];
  const { createSessionRuntime } = await loadSessionRuntime();
  const runtime = createSessionRuntime({
    sessionId: "session-close-race",
    eventsUrl: null,
    seatEvents: () => events,
    commitEvent: () => {},
    replaySeatEvents: () => {},
  });
  await runtime.attachSeat();
  for (let attempt = 0; attempt < 40 && globalThis.__seatEncodedBodies.length < 1; attempt += 1) {
    await new Promise((resolve) => setTimeout(resolve, 10));
  }
  assert.equal(retained.size, 1);
  runtime.close();
  assert.equal(beaconCalls, 0);
  resolveAppend();
  await new Promise((resolve) => setTimeout(resolve, 20));
  assert.equal(retained.size, 1, "stale in-flight success remains restart-reconcileable");
});
