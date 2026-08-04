import assert from "node:assert/strict";
import test from "node:test";

import {
  fieldAccessibilityModel,
  fieldPreferenceModel,
  mountFieldAccessibility,
} from "../pkg/ryeos_field_accessibility.js";
import { comparableGrids } from "../pkg/ryeos_grid_canvas.js";
import {
  StaleFieldLayoutError,
  hitTest,
  hitTestGroup,
  layoutField,
  layoutFieldChunked,
  settleLayout,
} from "../pkg/ryeos_field_layout.js";

function entity(id, overrides = {}) {
  return {
    id,
    label: id,
    kind: "step",
    group_id: "work",
    layer_ids: ["live"],
    lane: null,
    rank: null,
    order: null,
    selected: false,
    traits: { shape: "rect", fill: "solid" },
    accessibility_label: `${id} step`,
    ...overrides,
  };
}

function vm() {
  return {
    id: "field:test",
    title: "Test",
    structural_revision: "one",
    groups: [{ id: "work", label: "Work", collapsed: false }],
    layers: [{ id: "live", label: "Live", visible: true }],
    entities: [entity("a", { selected: true }), entity("b"), entity("c")],
    relations: [
      { id: "ab", kind: "flows_to", source_id: "a", target_id: "b" },
      { id: "ba", kind: "flows_to", source_id: "b", target_id: "a" },
      { id: "bc", kind: "flows_to", source_id: "b", target_id: "c" },
    ],
    traversal: ["a", "b", "c"],
    selected: "a",
  };
}

test("compound layout is deterministic, condenses cycles, and retains prior coordinates", () => {
  const first = layoutField(vm());
  const second = layoutField(vm());
  assert.deepEqual(
    [...first.nodes.values()].map(({ id, targetX, targetY, componentSize }) => ({ id, targetX, targetY, componentSize })),
    [...second.nodes.values()].map(({ id, targetX, targetY, componentSize }) => ({ id, targetX, targetY, componentSize })),
  );
  assert.equal(first.nodes.get("a").componentSize, 2);
  assert.equal(first.nodes.get("b").componentSize, 2);
  assert.ok(first.nodes.get("c").targetX > first.nodes.get("b").targetX);

  const prior = new Map([["a", { x: 17, y: 23 }]]);
  const retained = layoutField(vm(), prior);
  assert.equal(retained.nodes.get("a").x, 17);
  assert.equal(retained.nodes.get("a").y, 23);
  settleLayout(retained, 1);
  assert.equal(retained.nodes.get("a").x, retained.nodes.get("a").targetX);
  assert.equal(hitTest(retained, retained.nodes.get("a").x, retained.nodes.get("a").y)?.id, "a");
  const group = retained.groups[0];
  assert.equal(hitTestGroup(retained, group.x + 5, group.y + 5)?.id, "work");
});

test("accessibility traversal has one ordered semantic model and relation summaries", () => {
  const model = fieldAccessibilityModel(vm());
  assert.deepEqual(model.map((item) => item.id), ["a", "b", "c"]);
  assert.equal(model.filter((item) => item.selected).length, 1);
  assert.match(model[0].neighbors, /flows_to to b/);
  assert.equal(model[0].position, 1);
  assert.equal(model[0].size, 3);
});

test("accessibility keeps collapsed entities reachable and exposes preference hooks", () => {
  const field = vm();
  field.groups[0].collapsed = true;
  const model = fieldAccessibilityModel(field);
  assert.deepEqual(model.map((item) => item.id), ["a", "b", "c"]);
  assert.ok(model.every((item) => item.expanded === false));
  assert.deepEqual(
    fieldPreferenceModel((query) => ({ matches: query.includes("reduced-motion") })),
    { reducedMotion: true, highContrast: false },
  );
});

test("accessibility DOM uses one tab stop and identical selection/activation events", () => {
  const previousDocument = globalThis.document;
  globalThis.document = { createElement: () => new FakeElement() };
  try {
    const host = new FakeElement();
    const events = [];
    const field = vm();
    field.entities[0].activate_intent = { type: "inspect", item_ref: "item:a" };
    mountFieldAccessibility(host, field, "tile:7", (event) => events.push(event));
    assert.equal(host.tabIndex, 0);
    assert.ok(host.children.every((child) => child.tabIndex === -1));
    assert.equal(host.attributes.get("aria-activedescendant"), host.children[0].id);
    assert.equal(host.children[0].attributes.get("aria-expanded"), "true");
    assert.match(host.children[0].textContent, /Group Work/);

    host.onfocus();
    assert.deepEqual(events.at(-1), {
      type: "set_field_selection",
      instance_key: "tile:7",
      entity_id: "a",
    });
    host.onkeydown(keyEvent("Enter"));
    assert.deepEqual(events.slice(-2), [
      { type: "set_field_selection", instance_key: "tile:7", entity_id: "a" },
      { type: "activate", intent: field.entities[0].activate_intent },
    ]);
    host.onkeydown(keyEvent("ArrowLeft"));
    assert.deepEqual(events.at(-1), {
      type: "set_field_group_collapsed",
      instance_key: "tile:7",
      group_id: "work",
      collapsed: true,
    });
  } finally {
    globalThis.document = previousDocument;
  }
});

test("large layouts yield between phases and reject stale work", async () => {
  const field = vm();
  field.entities = Array.from({ length: 241 }, (_, index) => entity(`entity:${index}`));
  field.relations = field.entities.slice(1).map((item, index) => ({
    id: `relation:${index}`,
    kind: "flows_to",
    source_id: field.entities[index].id,
    target_id: item.id,
  }));
  field.traversal = field.entities.map((item) => item.id);
  let yields = 0;
  const layout = await layoutFieldChunked(field, new Map(), {
    schedule: async () => { yields += 1; },
  });
  assert.equal(layout.nodes.size, 241);
  assert.equal(yields, 2);

  await assert.rejects(
    layoutFieldChunked(field, new Map(), {
      schedule: async () => {},
      isStale: () => true,
    }),
    StaleFieldLayoutError,
  );
});

test("grid comparison requires key, kind, dimensions, and palette meaning", () => {
  const left = {
    comparison_key: "same",
    kind: "indexed_grid",
    grid: { width: 2, height: 2, palette: [{ index: 0, glyph: ".", label: "empty" }] },
  };
  assert.equal(comparableGrids(left, structuredClone(left)), true);
  assert.equal(comparableGrids(left, { ...structuredClone(left), comparison_key: "other" }), false);
  const differentPalette = structuredClone(left);
  differentPalette.grid.palette[0].glyph = "x";
  assert.equal(comparableGrids(left, differentPalette), false);
});

class FakeElement {
  constructor() {
    this.attributes = new Map();
    this.children = [];
    this.dataset = {};
    this.listeners = new Map();
    this.tabIndex = 0;
    this.textContent = "";
    this.id = "";
  }

  replaceChildren(...children) {
    this.children = [...children];
  }

  append(...children) {
    this.children.push(...children);
  }

  setAttribute(name, value) {
    this.attributes.set(name, String(value));
  }

  removeAttribute(name) {
    this.attributes.delete(name);
  }

  addEventListener(name, listener) {
    this.listeners.set(name, listener);
  }
}

function keyEvent(key) {
  return {
    key,
    prevented: false,
    preventDefault() { this.prevented = true; },
  };
}
