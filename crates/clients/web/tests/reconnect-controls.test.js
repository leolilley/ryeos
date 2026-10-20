import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../browser/runtime/reconnect-controls.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const controls = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
const incarnation = "a".repeat(64);
const predecessor = { kind: "control", event_hash: "b".repeat(64) };

test("only authenticated producer state supplies replacement choices", () => {
  assert.deepEqual(controls.replacementChoices({ producer: { kind: "unclaimed" } }), []);
  assert.deepEqual(controls.replacementChoices({ producer: { kind: "closed", producer_incarnation: incarnation } }), []);
  assert.deepEqual(controls.replacementChoices({ control_predecessor: predecessor, producer: { kind: "active", producer_incarnation: incarnation } }), [
    { kind: "takeover", expected_producer_incarnation: incarnation, predecessor },
  ]);
  assert.deepEqual(controls.replacementChoices({ control_predecessor: predecessor, producer: { kind: "relinquished", producer_incarnation: incarnation } }), [
    { kind: "resume", expected_producer_incarnation: incarnation, predecessor },
    { kind: "takeover", expected_producer_incarnation: incarnation, predecessor },
  ]);
});

test("receipt reconciliation precedes read-only observation", async () => {
  const order = [];
  const result = { producer: { kind: "unclaimed" } };
  assert.equal(await controls.observeReconnectChoices({
    async reconcilePendingAppend() { order.push("receipt"); },
    async observeProducer() { order.push("observe"); return result; },
  }), result);
  assert.deepEqual(order, ["receipt", "observe"]);
});

test("unknown append outcome prevents producer observation and choices", async () => {
  let observed = false;
  await assert.rejects(controls.observeReconnectChoices({
    async reconcilePendingAppend() { throw new Error("receipt unknown"); },
    async observeProducer() { observed = true; },
  }), /receipt unknown/);
  assert.equal(observed, false);
});

function fakeDocument() {
  const elements = [];
  return { elements, createElement() {
    const listeners = new Map();
    const node = { dataset: {}, append() {}, setAttribute() {}, querySelectorAll() { return []; },
      addEventListener(name, listener) { listeners.set(name, listener); },
      removeEventListener(name, listener) { if (listeners.get(name) === listener) listeners.delete(name); },
      async click() { await listeners.get("click")?.(); },
      listenerCount() { return listeners.size; } };
    elements.push(node);
    return node;
  } };
}

test("visible exact retry returns the same owner without claiming another writer", async () => {
  const previous = globalThis.document;
  const document = fakeDocument();
  globalThis.document = document;
  try {
    const runtime = { preparedPresentation() { return null; } };
    const pending = controls.awaitBootRecovery({ replaceChildren() {} }, runtime, new Error("failure"));
    await document.elements.find((element) => element.textContent === "Retry exact request").click();
    assert.equal(await pending, runtime);
    assert.equal(document.elements.reduce((count, element) => count + element.listenerCount(), 0), 0);
  } finally { globalThis.document = previous; }
});

test("document disposal removes recovery actions without claiming", async () => {
  const previous = globalThis.document;
  const document = fakeDocument();
  globalThis.document = document;
  try {
    const abort = new AbortController();
    const pending = controls.awaitBootRecovery({ replaceChildren() {} }, { preparedPresentation() { return null; } }, "failure", abort.signal);
    abort.abort(new Error("disposed"));
    await assert.rejects(pending, /disposed/);
    assert.equal(document.elements.reduce((count, element) => count + element.listenerCount(), 0), 0);
  } finally { globalThis.document = previous; }
});

test("writer observation alone does not claim; explicit takeover carries exact predecessor", async () => {
  const previous = globalThis.document;
  const document = fakeDocument(); globalThis.document = document;
  try {
    const order = [];
    const replacement = { close() {} };
    const runtime = {
      preparedPresentation() { return { presentation_id: "exact" }; },
      async reconcilePendingAppend() { order.push("receipt"); },
      async observeProducer() { order.push("observe"); return { control_predecessor: predecessor, producer: { kind: "active", producer_incarnation: incarnation } }; },
      async reconnect(choice) { order.push(choice); return replacement; },
    };
    const pending = controls.awaitBootRecovery({ replaceChildren() {} }, runtime, "conflict");
    await document.elements.find(element => element.textContent === "Reconcile and inspect writer").click();
    assert.deepEqual(order, ["receipt", "observe"]);
    await document.elements.find(element => element.textContent === "Take over this presentation").click();
    assert.equal(await pending, replacement);
    assert.deepEqual(order[2], { kind: "takeover", expected_producer_incarnation: incarnation, predecessor });
  } finally { globalThis.document = previous; }
});

test("late replacement is disposed after document abort", async () => {
  const previous = globalThis.document;
  const document = fakeDocument(); globalThis.document = document;
  try {
    let deliver; let closes = 0;
    const abort = new AbortController();
    const runtime = { preparedPresentation() { return {}; }, async reconcilePendingAppend() {},
      async observeProducer() { return { control_predecessor: predecessor, producer: { kind: "active", producer_incarnation: incarnation } }; },
      reconnect() { return new Promise(resolve => { deliver = resolve; }); } };
    const pending = controls.awaitBootRecovery({ replaceChildren() {} }, runtime, "conflict", abort.signal);
    await document.elements.find(element => element.textContent === "Reconcile and inspect writer").click();
    const clicking = document.elements.find(element => element.textContent === "Take over this presentation").click();
    await Promise.resolve();
    assert.equal(typeof deliver, "function", "replacement action reached its in-flight request");
    abort.abort(new Error("disposed"));
    let settled = false;
    void pending.then(() => { settled = true; }, () => { settled = true; });
    await Promise.resolve();
    assert.equal(settled, false, "disposal waits for the clicked replacement decision");
    deliver({ close() { closes += 1; } }); await clicking;
    await assert.rejects(pending, /disposed/);
    assert.equal(closes, 1);
  } finally { globalThis.document = previous; }
});

test("lost live writer offers observation and replacement, never a second exact bootstrap", async () => {
  const previous = globalThis.document;
  const document = fakeDocument(); globalThis.document = document;
  try {
    const abort = new AbortController();
    const runtime = { preparedPresentation() { return {}; } };
    const pending = controls.awaitBootRecovery({ replaceChildren() {} }, runtime, "writer refused", abort.signal, { allowExactRetry: false });
    assert.equal(document.elements.some(element => element.textContent === "Retry exact request"), false);
    assert.equal(document.elements.some(element => element.textContent === "Reconcile and inspect writer"), true);
    assert.equal(document.elements.some(element => element.textContent === "Take over this presentation"), false, "no choice before authenticated observation");
    abort.abort(new Error("done")); await assert.rejects(pending, /done/);
  } finally { globalThis.document = previous; }
});
