import assert from "node:assert/strict";
import test from "node:test";
import { fieldPreviewGroups } from "../browser/views/field_preview_groups.js";

const entity = (id) => ({ id, label: id, preview_ids: [`preview:${id}`] });
const preview = (id, grid = { width: 1, height: 1 }) => ({
  id: `preview:${id}`, entity_id: id, grid,
});

test("compared previews retain projected owners independently of selection", () => {
  const field = {
    entities: [entity("left"), entity("right"), entity("third")],
    previews: [preview("left"), preview("right"), preview("third")],
    selected: "third",
    compare: ["left", "right"],
  };
  const grouped = fieldPreviewGroups(field);
  assert.deepEqual(grouped.selectedPreviews.map((item) => item.entity_id), ["third"]);
  assert.deepEqual(grouped.compared.map((entry) => entry.entity.id), ["left", "right"]);
  assert.deepEqual(grouped.compared.map((entry) => entry.previews[0].entity_id), ["left", "right"]);

  field.selected = null;
  const withoutSelection = fieldPreviewGroups(field);
  assert.deepEqual(withoutSelection.selectedPreviews, []);
  assert.deepEqual(withoutSelection.compared.map((entry) => entry.entity.id), ["left", "right"]);
});

test("missing or unrenderable payloads stay with their exact projected entity", () => {
  const field = {
    entities: [entity("left"), entity("right")],
    previews: [preview("left", null), { ...preview("right"), entity_id: "foreign" }],
    selected: null,
    compare: ["left", "right"],
  };
  const grouped = fieldPreviewGroups(field);
  assert.equal(grouped.compared[0].previews[0].grid, null);
  assert.deepEqual(grouped.compared[1].previews, []);
  assert.deepEqual(grouped.selectedPreviews, []);
});
