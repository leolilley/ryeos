import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import { isShellChromeEscape } from "../browser/runtime/chrome-keyboard.js";

class TestElement {
  constructor(role, parent = null) { this.role = role; this.parent = parent; }
  closest(selector) {
    if (selector !== ".system-bar, .status-bar") return null;
    for (let current = this; current; current = current.parent) {
      if (current.role === "system-bar" || current.role === "status-bar") return current;
    }
    return null;
  }
}
globalThis.Element = TestElement;

const key = (target, overrides = {}) => ({
  key: "Escape", isComposing: false, altKey: false, ctrlKey: false,
  metaKey: false, shiftKey: false, target, composedPath: () => [target],
  ...overrides,
});

test("Escape restored to launcher or other shell chrome cannot close a View", () => {
  const launcher = new TestElement("launcher", new TestElement("status-bar"));
  const picker = new TestElement("summary", new TestElement("system-bar"));
  const view = new TestElement("view");
  assert.equal(isShellChromeEscape(key(launcher)), true);
  assert.equal(isShellChromeEscape(key(picker)), true);
  assert.equal(isShellChromeEscape(key(view)), false);
  const shadowHost = new TestElement("shadow-host", new TestElement("status-bar"));
  const shadowInner = new TestElement("shadow-inner");
  assert.equal(isShellChromeEscape(key(shadowHost, { composedPath: () => [shadowInner, shadowHost] })), true);
  assert.equal(isShellChromeEscape(key(view, { composedPath: () => [shadowInner, view] })), false);
});

test("modified and composing Escape remain on their existing owners", () => {
  const launcher = new TestElement("launcher", new TestElement("status-bar"));
  for (const modifier of ["altKey", "ctrlKey", "metaKey", "shiftKey", "isComposing"]) {
    assert.equal(isShellChromeEscape(key(launcher, { [modifier]: true })), false);
  }
  assert.equal(isShellChromeEscape(key(launcher, { key: "k", ctrlKey: true })), false);
});

test("global boot guards chrome Escape before Rust key translation", async () => {
  const boot = await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8");
  const boundary = boot.slice(boot.indexOf('window.addEventListener("keydown"'), boot.indexOf('window.addEventListener("resize"'));
  assert.ok(boundary.indexOf("isShellChromeEscape(event)") > boundary.indexOf("if (event.isComposing) return"));
  assert.ok(boundary.indexOf("isShellChromeEscape(event)") < boundary.indexOf("const key = keyEvent(event)"));
  assert.match(boundary, /if \(isShellChromeEscape\(event\)\) \{\s*event\.preventDefault\(\);\s*return;/);
});
