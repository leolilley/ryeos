import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fieldAccessibilityModel } from "../browser/visuals/ryeos_field_accessibility.js";

function field(ids) {
  return {
    entities: ids.map((id) => ({ id, label: id })),
    groups: [], relations: [], traversal: ids, selected: ids[0],
  };
}

test("equal retained entities in independent mounts have distinct DOM identities", () => {
  const vm = field(["run:one"]);
  const first = fieldAccessibilityModel(vm, "seat:p:surface:s:tile:1")[0];
  const second = fieldAccessibilityModel(vm, "seat:p:surface:s:tile:2")[0];
  assert.equal(first.id, second.id);
  assert.notEqual(first.domId, second.domId);
  assert.equal(first.domId, fieldAccessibilityModel(vm, "seat:p:surface:s:tile:1")[0].domId);
  assert.equal(first.selected, true);
});

test("punctuation and escape-looking entity or mount IDs cannot collide", () => {
  const vm = field(["run:one", "run_3a_one", "run--one", "run/one", "run💠one"]);
  const first = fieldAccessibilityModel(vm, "mount:one");
  const second = fieldAccessibilityModel(vm, "mount_3a_one");
  const ids = [...first, ...second].map((item) => item.domId);
  assert.equal(new Set(ids).size, ids.length);
  assert.ok(ids.every((id) => /^[a-zA-Z0-9-]+$/.test(id)));
});

test("explicit Activate control uses only the selected entity's admitted activation intent", async () => {
  const view = await readFile(new URL("../browser/views/FieldView.svelte", import.meta.url), "utf8");
  assert.match(view, /\{#if selected\.activate_intent\}<button[^>]*onclick=\{\(\) => activateEntity\(selected\.id\)\}>Activate<\/button>\{\/if\}/);
  assert.match(view, /if \(entity\?\.activate_intent\) dispatch\(\{ type: "activate", intent: entity\.activate_intent \}\)/);
  assert.doesNotMatch(view, /artifact_ref[^\n]*activateEntity/);
});
