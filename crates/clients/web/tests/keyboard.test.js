import assert from "node:assert/strict";
import test from "node:test";

import {
  hasModifiers,
  isNativeActivationTarget,
  isTypingTarget,
  ryeosKeyEvent,
  ryeosKeyName,
} from "../pkg/ryeos_keyboard.js";

test("normalizes named and printable keys", () => {
  assert.equal(ryeosKeyName("ArrowUp"), "arrow_up");
  assert.equal(ryeosKeyName("Escape"), "escape");
  assert.deepEqual(ryeosKeyName("x"), { char: "x" });
  assert.equal(ryeosKeyName("F5"), null);
  assert.equal(ryeosKeyName("Dead"), null);
});

test("preserves browser modifier state", () => {
  const key = ryeosKeyEvent({
    key: "k",
    ctrlKey: true,
    altKey: false,
    shiftKey: true,
    metaKey: false,
  });
  assert.deepEqual(key, {
    key: { char: "k" },
    modifiers: { ctrl: true, alt: false, shift: true, meta: false },
  });
  assert.equal(hasModifiers(key), true);
});

test("defers typing and native activation targets to the browser", () => {
  const target = (selector) => ({ closest: (query) => query.includes(selector) ? {} : null });
  assert.equal(isTypingTarget(target("textarea")), true);
  assert.equal(isTypingTarget(target("button")), false);
  assert.equal(isNativeActivationTarget(target("button")), true);
  assert.equal(isNativeActivationTarget(target("textarea")), false);
});
