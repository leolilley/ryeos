import assert from "node:assert/strict";
import os from "node:os";
import test from "node:test";
import { performance } from "node:perf_hooks";

import {
  hitTest,
  layoutField,
  rebindFieldLayout,
} from "../pkg/ryeos_field_layout.js";

const ITERATIONS = 50;
const COLD_LAYOUT_P95_MS = 120;
const DATA_UPDATE_P95_MS = 16;
const HIT_TEST_P95_MS = 2;

function performanceVm() {
  const entities = Array.from({ length: 1_000 }, (_, index) => ({
    id: `entity:${index}`,
    label: `Entity ${index}`,
    kind: "work_unit",
    group_id: "work",
    layer_ids: ["live"],
    lane: `lane:${index % 8}`,
    rank: Math.floor(index / 80),
    order: index,
    selected: index === 0,
    status: "ready",
    traits: { shape: "rect", fill: "solid", stroke: "solid" },
  }));
  const relations = Array.from({ length: 3_000 }, (_, index) => ({
    id: `relation:${index}`,
    kind: "depends_on",
    source_id: `entity:${index % 1_000}`,
    target_id: `entity:${(index + 1 + Math.floor(index / 1_000)) % 1_000}`,
    directed: true,
  }));
  return {
    id: "field:performance",
    structural_revision: "structure:v1",
    data_revision: "data:v1",
    local_revision: "local:v1",
    groups: [{ id: "work", label: "Work", collapsed: false }],
    layers: [{ id: "live", label: "Live", visible: true }],
    entities,
    relations,
    traversal: entities.map((entity) => entity.id),
    selected: "entity:0",
  };
}

function measure(run) {
  const samples = [];
  for (let index = 0; index < ITERATIONS; index += 1) {
    const started = performance.now();
    run(index);
    samples.push(performance.now() - started);
  }
  return samples;
}

function p95(samples) {
  return [...samples].sort((left, right) => left - right)[Math.ceil(samples.length * 0.95) - 1];
}

test("fixed 1000/3000 field meets deterministic renderer performance gates", () => {
  const vm = performanceVm();
  layoutField(vm); // JIT and module warm-up is outside the recorded gate.

  const cold = measure(() => layoutField(vm));
  const layout = layoutField(vm);
  const positions = [...layout.nodes.values()].map(({ id, x, y }) => ({ id, x, y }));
  const updated = structuredClone(vm);
  updated.data_revision = "data:v2";
  updated.local_revision = "local:v2";
  updated.selected = "entity:999";
  updated.entities[0].selected = false;
  updated.entities[999].selected = true;
  updated.entities[500].status = "running";
  const dataUpdates = measure(() => rebindFieldLayout(layout, updated));
  const hit = layout.nodes.get("entity:500");
  const hitTests = measure(() => {
    assert.ok(hitTest(layout, hit.x, hit.y), "a populated field point remains hittable");
  });

  assert.deepEqual(
    [...layout.nodes.values()].map(({ id, x, y }) => ({ id, x, y })),
    positions,
    "data-only updates preserve the existing geometry",
  );
  const measurements = {
    runner: `${process.version} ${process.platform}/${process.arch}; ${os.cpus()[0]?.model || "unknown CPU"}`,
    coldLayoutP95Ms: p95(cold),
    dataUpdateP95Ms: p95(dataUpdates),
    hitTestP95Ms: p95(hitTests),
  };
  assert.ok(
    measurements.coldLayoutP95Ms <= COLD_LAYOUT_P95_MS,
    `cold layout p95 ${measurements.coldLayoutP95Ms.toFixed(2)}ms exceeds ${COLD_LAYOUT_P95_MS}ms on ${measurements.runner}`,
  );
  assert.ok(
    measurements.dataUpdateP95Ms <= DATA_UPDATE_P95_MS,
    `data update p95 ${measurements.dataUpdateP95Ms.toFixed(2)}ms exceeds ${DATA_UPDATE_P95_MS}ms on ${measurements.runner}`,
  );
  assert.ok(
    measurements.hitTestP95Ms <= HIT_TEST_P95_MS,
    `hit test p95 ${measurements.hitTestP95Ms.toFixed(2)}ms exceeds ${HIT_TEST_P95_MS}ms on ${measurements.runner}`,
  );
});
