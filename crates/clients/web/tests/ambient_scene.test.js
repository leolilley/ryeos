import assert from "node:assert/strict";
import test from "node:test";

import {
  ambientPlatform,
  seededAmbientRandom,
} from "../pkg/ryeos_ambient_scene.js";

test("ambient random input is deterministic for a fixed seed", () => {
  const first = seededAmbientRandom(0x5259454f);
  const second = seededAmbientRandom(0x5259454f);
  assert.deepEqual(
    [first(), first(), first(), first()],
    [second(), second(), second(), second()],
  );
});

test("ambient platform uses the complete injected graphics boundary", () => {
  const target = {};
  const callbacks = [];
  const platform = ambientPlatform({
    random: () => 0.25,
    now: () => 1234,
    requestFrame: (callback) => { callbacks.push(callback); return 7; },
    cancelFrame: (frame) => callbacks.push(frame),
    eventTarget: target,
    devicePixelRatio: () => 1.5,
    viewport: () => ({ width: 1440, height: 900 }),
    reducedMotion: () => true,
  });

  assert.equal(platform.random(), 0.25);
  assert.equal(platform.now(), 1234);
  assert.equal(platform.requestFrame(() => {}), 7);
  platform.cancelFrame(7);
  assert.equal(platform.eventTarget, target);
  assert.equal(platform.devicePixelRatio(), 1.5);
  assert.deepEqual(platform.viewport(), { width: 1440, height: 900 });
  assert.equal(platform.reducedMotion(), true);
  assert.equal(callbacks.length, 2);
});
