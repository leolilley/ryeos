import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../browser/runtime/thread-tail.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const { focusedThreadTailFrame } = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

test("chain tail never relabels a previous or child thread as the focused successor", () => {
  const previous = { event_type: "cognition_out", payload: {
    chain_root_id: "chain-a", thread_id: "thread-a", event_type: "cognition_out", payload: { delta: "old" },
  } };
  assert.deepEqual(focusedThreadTailFrame(previous, "chain-a", "thread-a"),
    { eventType: "cognition_out", payload: previous.payload });
  assert.equal(focusedThreadTailFrame(previous, "chain-a", "thread-b"), null);
  assert.equal(focusedThreadTailFrame(previous, "chain-b", "thread-a"), null);
  assert.equal(focusedThreadTailFrame({ event_type: "cognition_out" }, "chain-a", "thread-a"), null);
  assert.equal(focusedThreadTailFrame({ ...previous, event_type: "token_delta" }, "chain-a", "thread-a"), null);
});
