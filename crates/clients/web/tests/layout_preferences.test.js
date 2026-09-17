import assert from "node:assert/strict";
import test from "node:test";

import { createLayoutPreferencePersistence } from "../pkg/ryeos_layout_preferences.js";

function harness({ current = "A", persisted = null, failWrites = false } = {}) {
  const scheduled = new Map();
  const cancelled = [];
  const writes = [];
  const errors = [];
  let nextTimer = 1;
  const persistence = createLayoutPreferencePersistence({
    key: "layout-key",
    persisted,
    readCurrent: () => current,
    storage: {
      setItem(key, value) {
        if (failWrites) throw new Error("storage unavailable");
        writes.push([key, value]);
      },
    },
    reportError: (error) => errors.push(error),
    schedule(callback) {
      const timer = nextTimer++;
      scheduled.set(timer, callback);
      return timer;
    },
    cancel(timer) {
      cancelled.push(timer);
      scheduled.delete(timer);
    },
  });
  return {
    persistence,
    scheduled,
    cancelled,
    writes,
    errors,
    setCurrent(value) { current = value; },
    setFailWrites(value) { failWrites = value; },
    runLastTimer() {
      const entry = [...scheduled.entries()].at(-1);
      assert.ok(entry, "expected a scheduled persistence write");
      scheduled.delete(entry[0]);
      entry[1]();
    },
  };
}

test("unchanged accepted state performs no browser write", () => {
  const h = harness();
  assert.equal(h.persistence.observeAcceptedState(), false);
  assert.equal(h.scheduled.size, 0);
  assert.deepEqual(h.writes, []);
});

test("scheduled writes retain the exact observed opaque value", () => {
  const h = harness();
  h.setCurrent("B");
  assert.equal(h.persistence.observeAcceptedState(), true);
  h.setCurrent("C");
  h.runLastTimer();
  assert.deepEqual(h.writes, [["layout-key", "B"]]);
});

test("a newer arrangement replaces the pending write", () => {
  const h = harness();
  h.setCurrent("B");
  h.persistence.observeAcceptedState();
  h.setCurrent("C");
  h.persistence.observeAcceptedState();
  assert.deepEqual(h.cancelled, [1]);
  h.runLastTimer();
  assert.deepEqual(h.writes, [["layout-key", "C"]]);
});

test("returning to the persisted arrangement cancels pending work", () => {
  const h = harness({ current: "A", persisted: "A" });
  h.setCurrent("B");
  h.persistence.observeAcceptedState();
  h.setCurrent("A");
  assert.equal(h.persistence.observeAcceptedState(), false);
  assert.equal(h.scheduled.size, 0);
  assert.deepEqual(h.writes, []);
});

test("pagehide flush writes the captured pending value and retries failures", () => {
  const h = harness({ failWrites: true });
  h.setCurrent("B");
  h.persistence.observeAcceptedState();
  assert.equal(h.persistence.flush(), false);
  assert.equal(h.errors.length, 1);
  h.setCurrent("C");
  h.setFailWrites(false);
  assert.equal(h.persistence.flush(), true);
  assert.deepEqual(h.writes, [["layout-key", "B"]]);
});
