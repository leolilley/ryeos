import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../browser/runtime/dialog-focus.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const { dialogFocusables, trapDialogTab, restoreDialogFocus } = await import(
  `data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`,
);

function fixture(count) {
  const document = { body: {}, activeElement: null };
  const controls = Array.from({ length: count }, () => ({
    hidden: false,
    layout: true,
    closest() { return this.hidden ? {} : null; },
    getClientRects() { return this.layout ? [{}] : []; },
    focus() { document.activeElement = this; },
  }));
  const panel = {
    ownerDocument: document,
    querySelectorAll() { return controls; },
    contains(element) { return element === this || controls.includes(element); },
    focus() { document.activeElement = this; },
  };
  return { document, panel, controls };
}

test("modal Tab wraps only through visible, non-inert controls", () => {
  const { document, panel, controls } = fixture(4);
  controls[1].hidden = true;
  controls[2].layout = false;
  assert.deepEqual(dialogFocusables(panel), [controls[0], controls[3]]);
  let prevented = 0;
  const event = { key: "Tab", isComposing: false, shiftKey: false, preventDefault() { prevented++; } };
  document.activeElement = controls[3];
  trapDialogTab(panel, event);
  assert.equal(document.activeElement, controls[0]);
  event.shiftKey = true;
  trapDialogTab(panel, event);
  assert.equal(document.activeElement, controls[3]);
  document.activeElement = {};
  trapDialogTab(panel, event);
  assert.equal(document.activeElement, controls[3]);
  assert.equal(prevented, 3);
});

test("an empty dialog traps focus on its frame, not the underlying Surface", () => {
  const { document, panel } = fixture(0);
  trapDialogTab(panel, { key: "Tab", preventDefault() {} });
  assert.equal(document.activeElement, panel);
});

test("dismissal restores its opener but preserves a View action's destination focus", () => {
  const { document, panel, controls } = fixture(1);
  const opener = { isConnected: true, focus() { document.activeElement = this; } };
  document.activeElement = controls[0];
  restoreDialogFocus(panel, opener);
  assert.equal(document.activeElement, opener);
  const destination = {};
  document.activeElement = destination;
  restoreDialogFocus(panel, opener);
  assert.equal(document.activeElement, destination);
  opener.isConnected = false;
  document.activeElement = document.body;
  restoreDialogFocus(panel, opener);
  assert.equal(document.activeElement, document.body);
});

test("composition and non-Tab keys are not interpreted by the dialog frame", () => {
  const { panel } = fixture(1);
  const preventDefault = () => { throw new Error("frame consumed a View key"); };
  trapDialogTab(panel, { key: "Enter", preventDefault });
  trapDialogTab(panel, { key: "Tab", isComposing: true, preventDefault });
});
