import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

async function load(name) {
  const source = await readFile(new URL(`../browser/runtime/${name}.ts`, import.meta.url), "utf8");
  const compiled = ts.transpileModule(source, {
    compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
  }).outputText;
  return import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);
}

const { createDocumentLifetime } = await load("document-lifetime");
const { createCommitRuntime } = await load("commit");

test("commit runtime applies a readiness reducer event before enqueue returns", () => {
  let installed = false;
  const runtime = createCommitRuntime({
    reduce() { installed = true; return { effects: [] }; },
    applyEffectResult() { throw new Error("unexpected effect result"); },
    startEffect() { throw new Error("unexpected effect"); },
    failedEffectResult() { throw new Error("unexpected effect failure"); },
    render() {},
  });
  runtime.enqueueEvent({ type: "presentation_producer_ready" });
  assert.equal(installed, true, "boot verifies reducer readiness immediately after enqueue");
  runtime.close();
});

test("disposal before boot prevents starting a document", async () => {
  let starts = 0;
  const lifetime = createDocumentLifetime(async () => {
    starts += 1;
    return { async close() {} };
  });
  await lifetime.dispose();
  await lifetime.ready;
  assert.equal(starts, 0);
});

test("disposal during asynchronous boot closes a late runtime exactly once", async () => {
  let resolveBoot;
  let signal;
  let closes = 0;
  const lifetime = createDocumentLifetime((nextSignal) => {
    signal = nextSignal;
    return new Promise((resolve) => { resolveBoot = resolve; });
  });
  await Promise.resolve();
  await lifetime.dispose();
  assert.equal(signal.aborted, true);
  resolveBoot({ async close() { closes += 1; } });
  await lifetime.ready;
  await lifetime.dispose();
  assert.equal(closes, 1);
});

test("prompt disposal has a separate late-cleanup barrier for successor attachment", async () => {
  let resolveBoot;
  let finishClose;
  let successorStarts = 0;
  const predecessor = createDocumentLifetime(() => new Promise((resolve) => { resolveBoot = resolve; }));
  await Promise.resolve();
  await predecessor.dispose();
  const successor = createDocumentLifetime(async () => {
    await predecessor.settled;
    successorStarts += 1;
    return { async close() {} };
  });
  await Promise.resolve();
  assert.equal(successorStarts, 0, "prompt disposal is not a teardown barrier");
  resolveBoot({ close: () => new Promise((resolve) => { finishClose = resolve; }) });
  await Promise.resolve();
  await Promise.resolve();
  assert.equal(successorStarts, 0, "late runtime cleanup must finish first");
  finishClose();
  await predecessor.settled;
  await successor.ready;
  assert.equal(successorStarts, 1);
  await successor.dispose();
  await successor.settled;
});

test("a synchronous close throw rejects settlement instead of stranding it", async () => {
  const lifetime = createDocumentLifetime(async () => ({
    close() { throw new Error("close failed"); },
  }));
  await lifetime.ready;
  const prompt = lifetime.dispose();
  await assert.rejects(prompt, /close failed/);
  await assert.rejects(lifetime.settled, /close failed/);
});

test("a mounted document has one idempotent disposal promise", async () => {
  let closes = 0;
  const lifetime = createDocumentLifetime(async () => ({ async close() { closes += 1; } }));
  await lifetime.ready;
  const first = lifetime.dispose();
  assert.equal(lifetime.dispose(), first);
  await first;
  assert.equal(closes, 1);
});

test("closed commit adapter fences late results without cancelling execution", async () => {
  let deliver;
  let applied = 0;
  let reduced = 0;
  let started = 0;
  let rendered = 0;
  const runtime = createCommitRuntime({
    reduce() { reduced += 1; return { effects: [] }; },
    applyEffectResult() { applied += 1; return { effects: [{ id: 2 }] }; },
    startEffect() { started += 1; return new Promise((resolve) => { deliver = resolve; }); },
    failedEffectResult() { throw new Error("unexpected failure"); },
    render() { rendered += 1; },
  });
  runtime.enqueueEnvelope({ effects: [{ id: 1 }] });
  const accepted = runtime.idle();
  runtime.close();
  await accepted;
  runtime.enqueueEvent({});
  runtime.commitMutation(() => { throw new Error("disposed mutation ran"); });
  deliver({});
  await Promise.resolve();
  await runtime.idle();
  assert.equal(started, 1, "the already accepted execution is not cancelled or restarted");
  assert.equal(applied, 0);
  assert.equal(reduced, 0);
  assert.equal(rendered, 0);
});

test("late failed effects do not reconstruct results for a disposed core", async () => {
  let rejectEffect;
  const runtime = createCommitRuntime({
    reduce() { throw new Error("unexpected mutation"); },
    applyEffectResult() { throw new Error("disposed result applied"); },
    startEffect() { return new Promise((_resolve, reject) => { rejectEffect = reject; }); },
    failedEffectResult() { throw new Error("disposed result reconstructed"); },
    render() { throw new Error("disposed render ran"); },
  });
  runtime.enqueueEnvelope({ effects: [{ id: 1 }] });
  runtime.close();
  rejectEffect(new Error("late transport failure"));
  await Promise.resolve();
  await runtime.idle();
});
