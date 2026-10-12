import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

// Source-shape checks only: no generated-contract or compiled Svelte claim.
const source = await readFile(new URL("../browser/views/FieldView.svelte", import.meta.url), "utf8");

test("field disclosure renders projected source evidence without inferring solver outcomes", () => {
  const disclosure = source.slice(source.indexOf('<details class="field-source-status"'), source.indexOf('<div class="field-accessibility"'));
  for (const projected of ["source.phase", "source.source_ref", "source.subject_fingerprint", "source.truncated", "source.error", "source.revision", "source.evidence", "field.provenance"]) {
    assert.ok(disclosure.includes(projected), projected);
  }
  assert.match(source, /field\.sources\.length \|\| field\.subjects\.length \|\| field\.metrics\.length \|\| field\.provenance\.length/);
  assert.match(disclosure, /Projected entity\/relation source refs/);
  assert.match(disclosure, /Subject fingerprint <code>\{source\.subject_fingerprint\}<\/code>/);
  assert.doesNotMatch(disclosure, /<summary aria-label=/);
  assert.match(disclosure, /not the complete retained field/);
  assert.doesNotMatch(disclosure, /fetch\(|postJson\(|evaluation_accepted|completed|passed|solved/i);
});

test("bounded source evidence list discloses omitted refs apart from document truncation", () => {
  const disclosure = source.slice(source.indexOf('<details class="field-source-status"'), source.indexOf('<div class="field-accessibility"'));
  assert.match(disclosure, /source\.evidence_omitted > 0/);
  assert.match(disclosure, /further entity evidence references omitted from this display/);
  assert.match(disclosure, /source\.truncated.*Truncated response/s);
});

test("field disclosure shows exact projected subjects without deriving source status", () => {
  const disclosure = source.slice(source.indexOf('<details class="field-source-status"'), source.indexOf('<div class="field-accessibility"'));
  assert.match(source, /field\.sources\.length \|\| field\.subjects\.length \|\| field\.metrics\.length \|\| field\.provenance\.length/);
  assert.match(disclosure, /aria-label="Projected source subjects"/);
  assert.match(disclosure, /\{#each field\.subjects as subject, index/);
  for (const projected of ["subject.source", "subject.kind", "subject.id", "subject.definition_ref", "subject.effective_definition_digest"]) {
    assert.ok(disclosure.includes(projected), projected);
  }
  assert.doesNotMatch(disclosure, /subject\.source\s*===?\s*source\.name|fetch\(|postJson\(|evaluation_accepted|solved/i);
});

test("field disclosure shows retained scalar metrics without inferring outcomes", () => {
  const disclosure = source.slice(source.indexOf('<details class="field-source-status"'), source.indexOf('<div class="field-accessibility"'));
  assert.match(source, /field\.subjects\.length \|\| field\.metrics\.length \|\| field\.provenance\.length/);
  assert.match(disclosure, /aria-label="Projected field metrics"/);
  assert.match(disclosure, /\{#each field\.metrics as metric, index \(`\$\{metric\.id\}:\$\{index\}`\)\}/);
  for (const projected of ["metric.label", "JSON.stringify(metric.value)", "metric.unit"]) {
    assert.ok(disclosure.includes(projected), projected);
  }
  assert.doesNotMatch(disclosure, /metric\.id\.split|metric\.id\.startsWith|fetch\(|postJson\(|evaluation_accepted|solved/i);
});

test("preview omission uses retained projected warnings, not inferred missing payloads", () => {
  assert.match(source, /\{#each field\.warnings as warning\}<small class="field-warning">\{warning\}<\/small>/);
  assert.match(source, /fieldPreviewGroups\(field\)/);
  assert.match(source, /previewGroups\.selectedPreviews/);
  assert.match(source, /previewGroups\.compared/);
  assert.match(source, /\{#if preview\.grid\}<GridPreview/);
  assert.doesNotMatch(source, /missingPreviewCount|previewIds\.size > 0 && previews\.length === 0/);
});

test("artifact coordinates are rendered as evidence without a browser fetch action", () => {
  const coordinate = source.slice(source.indexOf("{#if selected.artifact_ref}"), source.indexOf("{#each selectedRelations"));
  for (const field of ["thread_id", "artifact_id", "content_hash"]) {
    assert.ok(coordinate.includes(`selected.artifact_ref.${field}`), field);
  }
  assert.match(coordinate, /Retained artifact coordinate/);
  assert.doesNotMatch(coordinate, /fetch\(|postJson\(|onclick|href|dispatch\(/);
});

test("replay, search, and expansion limits disclose Rust-projected evidence boundaries", () => {
  const disclosure = source.slice(source.indexOf('<details class="field-source-status"'), source.indexOf('<div class="field-accessibility"'));
  for (const projected of [
    "field.replay.outside_cut",
    "field.search.truncated",
    "expansion.phase",
    "expansion.applied_depth",
    "expansion.entity_count",
    "expansion.truncated",
  ]) {
    assert.ok(disclosure.includes(projected), projected);
  }
  assert.match(disclosure, /not included in the selected braid cut/);
  assert.match(disclosure, /not the complete matching field/);
  assert.doesNotMatch(disclosure, /\bARC\b|solver|evaluation_accepted|fetch\(|postJson\(/i);
});

test("focused tree item Enter activates only that item, not the selected parent item", () => {
  const treeItem = source.slice(source.indexOf('role="treeitem"'), source.indexOf("{/each}\n  </div>", source.indexOf('role="treeitem"')));
  assert.match(treeItem, /if \(event\.key === "Enter"\) \{\s*event\.preventDefault\(\);\s*event\.stopPropagation\(\);\s*activateEntity\(item\.id\);/);
  assert.match(source, /event\.key === "Enter" && field\.selected/);
});
