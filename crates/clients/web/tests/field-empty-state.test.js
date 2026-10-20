import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { fieldEmptyState } from "../browser/views/field_empty_state.js";

const field = (...sources) => ({ entities: [], sources });
const source = (phase, overrides = {}) => ({ phase, error: null, truncated: false, ...overrides });

test("zero-entity source state never equates failure or absence with a successful empty run", () => {
  assert.equal(fieldEmptyState(field(source("error"))).tone, "error");
  assert.equal(fieldEmptyState(field(source("ready"), source("ready", { error: "unavailable" }))).tone, "error");
  assert.equal(fieldEmptyState(field(source("unresolved"))).tone, "pending");
  assert.equal(fieldEmptyState(field(source("loading"))).tone, "pending");
  assert.equal(fieldEmptyState(field(source("refreshing"))).tone, "pending");
  assert.equal(fieldEmptyState(field(source("ready", { truncated: true }))).tone, "bounded");
  assert.equal(fieldEmptyState(field(source("ready"))).title, "No entities in this projection");
  assert.match(fieldEmptyState(field(source("ready"))).detail, /does not establish/);
  assert.equal(fieldEmptyState(field()).tone, "unknown");
  assert.equal(fieldEmptyState({ entities: [{ id: "one" }], sources: [source("error")] }), null);
});

test("Field stage mounts the source-derived zero-entity status without another fetch", async () => {
  const view = await readFile(new URL("../browser/views/FieldView.svelte", import.meta.url), "utf8");
  assert.match(view, /const emptyState = \$derived\(fieldEmptyState\(field\)\)/);
  assert.match(view, /\{#if emptyState\}[\s\S]*role="status"[\s\S]*\{emptyState\.title\}/);
  assert.doesNotMatch(view, /fetch\(/);
});
