import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../browser/runtime/layout-preferences.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const { createScopedLayoutPreferencePersistence } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`
);

test("scope changes preserve cached restore candidates without replacing admitted structure", () => {
  const timers = new Map();
  let nextTimer = 0;
  globalThis.window = {
    setTimeout(callback) { const id = ++nextTimer; timers.set(id, callback); return id; },
    clearTimeout(id) { timers.delete(id); },
  };
  const storage = new Map([["surface-b", "b-saved"]]);
  const writes = [];
  let key;
  let current;
  const persistence = createScopedLayoutPreferencePersistence({
    readKey: () => key,
    readCurrent: () => current,
    storage: {
      getItem(key) { return storage.get(key) ?? null; },
      setItem(key, value) { writes.push([key, value]); storage.set(key, value); },
    },
    reportError(error) { throw error; },
  });
  assert.equal(persistence.observeAcceptedState(), false);
  key = "surface-a";
  current = "a-initial";
  persistence.observeAcceptedState();
  current = "a-edited";
  assert.equal(persistence.observeAcceptedState(), true);
  key = "surface-b";
  current = "b-initial";
  persistence.observeAcceptedState();
  assert.deepEqual(writes, [["surface-a", "a-edited"]]);
  assert.equal(current, "b-initial", "cached structure never replaces the admitted composition");
  assert.equal(storage.get("surface-b"), "b-saved", "old candidate remains until a deliberate accepted edit");
  persistence.flush();
  for (const callback of [...timers.values()]) callback();
  assert.equal(timers.size, 0, "bootstrap cannot queue a rewrite of cached structure");
  assert.equal(storage.get("surface-b"), "b-saved", "flush/timers cannot destroy a restore candidate");
  assert.deepEqual(writes, [["surface-a", "a-edited"]]);
  current = "b-edited";
  persistence.observeAcceptedState();
  key = undefined;
  persistence.observeAcceptedState();
  assert.deepEqual(writes, [["surface-a", "a-edited"], ["surface-b", "b-edited"]]);
  assert.equal(timers.size, 0);
  key = "surface-a";
  current = "a-new-seed";
  persistence.observeAcceptedState();
  assert.equal(current, "a-new-seed");
  assert.equal(storage.get("surface-a"), "a-edited");
  persistence.flush();
  assert.equal(storage.get("surface-a"), "a-edited", "a remount baseline is not a new accepted edit");
  delete globalThis.window;
});
