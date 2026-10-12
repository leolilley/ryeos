import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const mock = `
export class HttpResponseError extends Error { constructor(status,message) { super(message); this.status=status; } }
export const encodeJsonBody = JSON.stringify;
export const decodeJsonBody = text => JSON.parse(text, (_key,value) => typeof value === 'number' && Number.isInteger(value) ? BigInt(value) : value);
`;
const source = (await readFile(new URL("../browser/runtime/seat-history-stream.ts", import.meta.url), "utf8"))
  .replace('from "./transport"', `from ${JSON.stringify(`data:text/javascript;base64,${Buffer.from(mock).toString("base64")}`)}`);
const compiled = ts.transpileModule(source, { compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 } }).outputText;
const { consumeSeatHistory: consumeSeatHistoryImpl, SeatHistoryIncompleteError } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const coordinate = { presentation_id: "P", chain_root_id: "R", thread_id: "T" };
const hash = (digit) => digit.repeat(64);
const claim = { operation_id: "11111111-1111-8111-8111-111111111111", producer_incarnation: hash("b"), next_engine_seq: 0n };
const consumeSeatHistory = (coordinate, signal, stage, onInvalidated, onVerifiedAppend, stageTransferProof = () => {}, stageMoveFreezeCancellationProof = () => {}) =>
  consumeSeatHistoryImpl(coordinate, claim, signal, stage, onInvalidated, onVerifiedAppend, stageTransferProof, stageMoveFreezeCancellationProof);
const record = { event_hash: hash("a"), thread_id: "T", chain_root_id: "R", thread_seq: 2,
  prev_thread_event_hash: hash("0"), chain_seq: 3, event_type: "ui_seat.presentation_initialized.v1", payload: {} };
const control = { event_hash: hash("b"), thread_id: "T", chain_root_id: "R", thread_seq: 3,
  prev_thread_event_hash: hash("a"), chain_seq: 4, event_type: "ui_seat.producer.v3", payload: {
    schema_version: "ryeos.ui.seat.producer.v3", operation_id: claim.operation_id,
    producer_incarnation: claim.producer_incarnation, next_engine_seq: 0, state: "active",
    request: { action: "claim", operation_id: claim.operation_id, presentation_id: "P", chain_root_id: "R", thread_id: "T" },
  } };
const frame = (event, data) => `event: ${event}\ndata: ${JSON.stringify(data)}\n\n`;
const begin = frame("ui_seat.replay.begin.v1", { schema: 1, ...coordinate });
const batch = frame("ui_seat.replay.events.v1", { schema: 1, events: [record, control] });
const complete = frame("ui_seat.replay.complete.v1", { schema: 1, tip_hash: hash("b"), thread_seq: 3, event_count: 2 });
const invalidated = frame("ui_seat.replay.invalidated.v1", { schema: 1 });
const verified = (overrides = {}) => frame("ui_seat.replay.append_verified.v1", {
  schema: 1, operation_id: "22222222-2222-8222-8222-222222222222",
  producer_incarnation: hash("b"), first_engine_seq: 0, last_engine_seq: 0,
  event_count: 1, payload_digest: hash("c"), event_hash: hash("d"), thread_seq: 4,
  ...overrides,
});

function stream(chunks, keepOpen = false) {
  const encoder = new TextEncoder();
  let controller;
  const body = new ReadableStream({
    start(owner) {
      controller = owner;
      for (const chunk of chunks) owner.enqueue(encoder.encode(chunk));
      if (!keepOpen) owner.close();
    },
  });
  globalThis.fetch = async (_url, init) => {
    assert.equal(init.method, "POST");
    assert.deepEqual(JSON.parse(init.body), { presentation_id: "P", chain_root_id: "R" });
    return new Response(body, { headers: { "Content-Type": "text/event-stream" } });
  };
  return controller;
}

test("negative cancellation proof stages separately without advancing canonical count", async () => {
  const proof = { schema: 1, cancellation: { operation_id: "test-only-cancellation" } };
  stream([begin + batch + frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof }) + complete], true);
  const seen = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    () => {}, () => {}, () => {}, () => { throw new Error("not a transfer phase proof"); },
    (value) => seen.push(value));
  assert.equal(seen.length, 1);
  assert.equal(seen[0].schema, 1n);
  assert.equal(seen[0].cancellation.operation_id, proof.cancellation.operation_id);
  assert.equal(handoff.completion.event_count, 2n);
  handoff.close();
});

test("cancellation proof rejects invalid envelopes and staging failure", async () => {
  for (const wire of [
    frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof: {} }),
    begin + frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof: {}, extra: true }),
    begin + frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof: null }),
  ]) {
    stream([wire]);
    await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
      () => {}, () => {}, () => {}, () => {}, () => {}), /invalid shape or order/);
  }
  stream([begin + batch + frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof: {} }) + complete]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    () => {}, () => {}, () => {}, () => {}, () => { throw new Error("negative proof refused"); }), /negative proof refused/);
});

test("post-completion cancellation proof invalidates without staging", async () => {
  const controller = stream([begin, batch, complete], true);
  const seen = [], invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    () => {}, (error) => invalidations.push(error.message), () => {}, () => {}, (proof) => seen.push(proof));
  controller.enqueue(new TextEncoder().encode(frame("ui_seat.replay.move_freeze_cancellation_proof.v1", { schema: 1, proof: {} })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(seen, []);
  assert.match(invalidations[0], /unsupported post-completion/);
  handoff.close();
});

test("bounded provisional transfer proof stages before complete without changing event count", async () => {
  const proof = { schema: 1, transfer: { operation_id: "test-only-operation" }, phases: [] };
  stream([begin + batch + frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof }) + complete], true);
  const order = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    (events) => order.push(["events", events.length]), () => {}, () => {},
    (data) => order.push(["proof", data.transfer.operation_id]));
  assert.deepEqual(order, [["events", 2], ["proof", "test-only-operation"]]);
  assert.equal(handoff.completion.event_count, 2n);
  handoff.close();
});

test("transfer proof refuses before begin, malformed envelope and shared staging failure", async () => {
  const proof = { schema: 1 };
  for (const chunks of [
    [frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof })],
    [begin + frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof, extra: true })],
    [begin + frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof: null })],
  ]) {
    stream(chunks);
    await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal, () => {}, () => {}, () => {}), /invalid shape or order/);
  }
  stream([begin + batch + frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof }) + complete]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal, () => {}, () => {}, () => {},
    () => { throw new Error("shared proof refused"); }), /shared proof refused/);
});

test("post-completion transfer proof invalidates without staging", async () => {
  const controller = stream([begin, batch, complete], true);
  const proofs = [], invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), () => {}, (proof) => proofs.push(proof));
  controller.enqueue(new TextEncoder().encode(frame("ui_seat.replay.transfer_proof.v1", { schema: 1, proof: {} })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(proofs, []);
  assert.match(invalidations[0], /unsupported post-completion/);
  handoff.close();
});

test("complete promotes an exact streamed prefix and later invalidation fences it", async () => {
  const controller = stream([begin, batch, complete], true);
  const staged = [], invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => staged.push(...records), (error) => invalidations.push(error.message));
  assert.equal(handoff.completion.event_count, 2n);
  assert.equal(staged.length, 2);
  controller.enqueue(new TextEncoder().encode(invalidated));
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(invalidations[0], /invalidated/);
  handoff.close();
});

test("completion and a verified append in one network chunk retain their order", async () => {
  stream([begin + batch + complete + verified()], true);
  const order = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    () => order.push("stage"), () => order.push("invalidated"),
    (append) => order.push(`verified:${append.thread_seq}`));
  assert.deepEqual(order, ["stage", "verified:4"]);
  assert.equal(handoff.completion.thread_seq, 3n);
  handoff.close();
});

test("a verified append interval split across three bounded frames reaches one completion fence", async () => {
  const events = [record, control];
  for (let threadSeq = 4; threadSeq <= 130; threadSeq++) {
    const previous = events.at(-1);
    events.push({ event_hash: threadSeq.toString(16).padStart(64, "0"),
      thread_id: "T", chain_root_id: "R", thread_seq: threadSeq,
      prev_thread_event_hash: previous.event_hash, chain_seq: threadSeq + 1,
      event_type: "ui_seat.composition.v1", payload: {} });
  }
  const frames = [begin];
  for (let start = 0; start < events.length; start += 64) {
    frames.push(frame("ui_seat.replay.events.v1", { schema: 1, events: events.slice(start, start + 64) }));
  }
  frames.push(frame("ui_seat.replay.complete.v1", { schema: 1,
    tip_hash: events.at(-1).event_hash, thread_seq: 130, event_count: events.length }));
  stream(frames, true);
  const staged = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => staged.push(...records), () => {});
  assert.equal(handoff.completion.event_count, 129n);
  assert.equal(staged.length, 129);
  handoff.close();
});

test("EOF before complete never promotes partial replay", async () => {
  stream([begin, batch]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal, () => {}, () => {}),
    (error) => error instanceof SeatHistoryIncompleteError && /ended/.test(error.message));
});

test("pre-completion invalidation requests a fresh proof without promoting staged events", async () => {
  const staged = [];
  stream([begin, batch, invalidated]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    (events) => staged.push(...events), () => {}),
    (error) => error instanceof SeatHistoryIncompleteError && /invalidated/.test(error.message));
  assert.equal(staged.length, 2, "the caller must discard this provisional candidate");
});

test("abort during provisional staging stops later frames from the same chunk", async () => {
  stream([begin + batch + batch + complete], true);
  const abort = new AbortController();
  let staged = 0;
  await assert.rejects(consumeSeatHistory(coordinate, abort.signal, () => {
    staged++;
    abort.abort();
  }, () => {}), /aborted/);
  assert.equal(staged, 1, "a cancelled candidate cannot receive another decoded batch");
});

test("mismatched completion refuses before publication", async () => {
  stream([begin, batch, frame("ui_seat.replay.complete.v1", { schema: 1, tip_hash: hash("c"), thread_seq: 3, event_count: 2 })]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal, () => {}, () => {}), /does not match/);
});

test("a newer producer control at the fixed anchor refuses the earlier claim", async () => {
  const replaced = { ...control, event_hash: hash("c"), thread_seq: 4, chain_seq: 5,
    prev_thread_event_hash: hash("b"), payload: {
      ...control.payload, operation_id: "44444444-4444-8444-8444-444444444444",
      producer_incarnation: hash("d"), request: {
        ...control.payload.request, operation_id: "44444444-4444-8444-8444-444444444444",
      },
    } };
  stream([begin, batch, frame("ui_seat.replay.events.v1", { schema: 1, events: [replaced] }),
    frame("ui_seat.replay.complete.v1", { schema: 1, tip_hash: hash("c"), thread_seq: 4, event_count: 3 })]);
  const staged = [];
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => staged.push(...records), () => {}), /acknowledged producer claim/);
  assert.equal(staged.length, 3, "failed replay stays provisional");
});

test("completion without a producer control cannot publish a claimed writer", async () => {
  stream([begin, frame("ui_seat.replay.events.v1", { schema: 1, events: [record] }),
    frame("ui_seat.replay.complete.v1", { schema: 1, tip_hash: hash("a"), thread_seq: 2, event_count: 1 })]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    () => {}, () => {}), /acknowledged producer claim/);
});

test("the retained producer frontier must equal the acknowledged claim", async () => {
  const advanced = { ...control, payload: { ...control.payload, next_engine_seq: 1 } };
  stream([begin, frame("ui_seat.replay.events.v1", { schema: 1, events: [record, advanced] }), complete]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    () => {}, () => {}), /acknowledged producer claim/);
});

test("provisional staging cannot rewrite the control used at completion", async () => {
  const foreign = { ...control, payload: {
    ...control.payload, operation_id: "55555555-5555-8555-8555-555555555555",
    producer_incarnation: hash("e"), request: {
      ...control.payload.request, operation_id: "55555555-5555-8555-8555-555555555555",
    },
  } };
  stream([begin, frame("ui_seat.replay.events.v1", { schema: 1, events: [record, foreign] }), complete]);
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => {
      records[1].payload.operation_id = claim.operation_id;
      records[1].payload.producer_incarnation = claim.producer_incarnation;
      records[1].payload.request.operation_id = claim.operation_id;
    }, () => {}), /acknowledged producer claim/);
});

test("provisional staging cannot rewrite the acknowledged claim expectation", async () => {
  const foreignOperation = "66666666-6666-8666-8666-666666666666";
  const foreignProducer = hash("f");
  const foreign = { ...control, payload: {
    ...control.payload, operation_id: foreignOperation,
    producer_incarnation: foreignProducer, request: {
      ...control.payload.request, operation_id: foreignOperation,
    },
  } };
  const expected = { ...claim };
  stream([begin, frame("ui_seat.replay.events.v1", { schema: 1, events: [record, foreign] }), complete]);
  await assert.rejects(consumeSeatHistoryImpl(coordinate, expected, new AbortController().signal,
    () => {
      expected.operation_id = foreignOperation;
      expected.producer_incarnation = foreignProducer;
    }, () => {}, () => {}), /acknowledged producer claim/);
});

test("EOF after complete invalidates the live handoff", async () => {
  stream([begin, batch, complete]);
  const invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message));
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(invalidations[0], /ended/);
  handoff.close();
});

test("throwing invalidation notification does not repeat or retain the stream", async () => {
  const controller = stream([begin, batch, complete], true);
  let calls = 0;
  await consumeSeatHistory(coordinate, new AbortController().signal, () => {}, () => {
    calls++;
    throw new Error("observer failed");
  });
  controller.enqueue(new TextEncoder().encode(invalidated));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(calls, 1);
  assert.throws(() => controller.enqueue(new TextEncoder().encode(invalidated)));
});

test("invalidation callback abort cannot reenter notification", async () => {
  const controller = stream([begin, batch, complete], true);
  const signal = new AbortController();
  let calls = 0;
  await consumeSeatHistory(coordinate, signal.signal, () => {}, () => {
    calls++;
    signal.abort();
  });
  controller.enqueue(new TextEncoder().encode(invalidated));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(calls, 1);
  assert.throws(() => controller.enqueue(new TextEncoder().encode(invalidated)));
});

test("verified append keeps handoff live and exact repeated wake is idempotent", async () => {
  const controller = stream([begin, batch, complete], true);
  const seen = [], invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), (append) => seen.push(append));
  const second = verified({ operation_id: "33333333-3333-8333-8333-333333333333",
    first_engine_seq: 1, last_engine_seq: 1, event_hash: hash("e"), thread_seq: 5 });
  controller.enqueue(new TextEncoder().encode(verified() + second + verified()));
  await new Promise((resolve) => setImmediate(resolve));
  assert.equal(seen.length, 3);
  assert.equal(seen[0].thread_seq, 4n);
  assert.deepEqual(invalidations, []);
  handoff.close();
});

test("verified append callback cannot rewrite the live frontier", async () => {
  const controller = stream([begin, batch, complete], true);
  const invalidations = [], seen = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), (append) => {
      seen.push(append.operation_id);
      if (seen.length === 1) {
        append.thread_seq = 999n;
        append.event_hash = hash("f");
        append.operation_id = "99999999-9999-8999-8999-999999999999";
      }
    });
  controller.enqueue(new TextEncoder().encode(verified() + verified({
    operation_id: "33333333-3333-8333-8333-333333333333",
    first_engine_seq: 1, last_engine_seq: 1, event_hash: hash("e"), thread_seq: 5,
  })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(seen, [
    "22222222-2222-8222-8222-222222222222",
    "33333333-3333-8333-8333-333333333333",
  ]);
  assert.deepEqual(invalidations, []);
  handoff.close();
});

test("closing from a verified append callback stops queued frames in that chunk", async () => {
  const controller = stream([begin, batch, complete], true);
  const seen = [], invalidations = [];
  let handoff;
  handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), (append) => {
      seen.push(append.operation_id);
      handoff.close();
    });
  controller.enqueue(new TextEncoder().encode(verified() + verified({
    operation_id: "33333333-3333-8333-8333-333333333333",
    first_engine_seq: 1, last_engine_seq: 1, event_hash: hash("e"), thread_seq: 5,
  })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.deepEqual(seen, ["22222222-2222-8222-8222-222222222222"]);
  assert.deepEqual(invalidations, []);
});

test("malformed verified append invalidates the handoff", async () => {
  const controller = stream([begin, batch, complete], true);
  const invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), () => assert.fail("invalid frame delivered"));
  controller.enqueue(new TextEncoder().encode(verified({ event_count: 2 })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(invalidations[0], /invalid interval/);
  handoff.close();
});

test("reused verified operation with a different later tip invalidates", async () => {
  const controller = stream([begin, batch, complete], true);
  const invalidations = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal, () => {},
    (error) => invalidations.push(error.message), () => {});
  controller.enqueue(new TextEncoder().encode(verified() + verified({
    first_engine_seq: 1, last_engine_seq: 1, event_hash: hash("e"), thread_seq: 5,
  })));
  await new Promise((resolve) => setImmediate(resolve));
  assert.match(invalidations[0], /reused an operation/);
  handoff.close();
});

test("CRLF delimiter split across fetch chunks retains exact framing", async () => {
  const crlf = (begin + batch + complete).replaceAll("\n", "\r\n");
  const chunks = [];
  for (let i = 0; i < crlf.length; i += 1) chunks.push(crlf.slice(i, i + 1));
  stream(chunks, true);
  const staged = [];
  const handoff = await consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => staged.push(...records), () => assert.fail("valid CRLF stream invalidated"));
  assert.equal(staged.length, 2);
  assert.equal(handoff.completion.thread_seq, 3n);
  handoff.close();
});

test("oversized complete frame in one fetch chunk refuses before JSON or completion", async () => {
  const oversized = frame("ui_seat.replay.complete.v1", {
    schema: 1, tip_hash: hash("b"), thread_seq: 3, event_count: 2,
    padding: "x".repeat(17 * 1024 * 1024),
  });
  stream([begin + batch + oversized]);
  const staged = [];
  await assert.rejects(consumeSeatHistory(coordinate, new AbortController().signal,
    (records) => staged.push(...records), () => {}), /frame exceeds bound/);
  assert.equal(staged.length, 2, "only bounded provisional history was staged");
});
