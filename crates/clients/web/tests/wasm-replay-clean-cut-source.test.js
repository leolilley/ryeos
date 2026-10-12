import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

test("mutable replay export and loader requirement are absent", async () => {
  const wasm = await readFile(new URL("../src/wasm.rs", import.meta.url), "utf8");
  const loader = await readFile(new URL("../browser/runtime/wasm-api.ts", import.meta.url), "utf8");
  assert.doesNotMatch(wasm, /pub fn ryeos_replay_seat_events\b/);
  assert.doesNotMatch(loader, /\bryeos_replay_seat_events\b/);
  assert.match(wasm, /pub fn ryeos_replay_acknowledged_seat_events\b/);
  assert.match(wasm, /pub fn ryeos_stage_seat_replay_events\b/);
});

test("session replacement retains the live adapter document support declaration", async () => {
  const reducer = await readFile(new URL("../../base/src/ui/reducer/effect_results.rs", import.meta.url), "utf8");
  const replacement = reducer.slice(reducer.indexOf("let mut candidate = RyeOsCore::new("),
    reducer.indexOf("candidate.next_effect_id = next_effect_id;"));
  assert.match(replacement,
    /candidate\.supports_independent_documents = self\.supports_independent_documents;/);
});
