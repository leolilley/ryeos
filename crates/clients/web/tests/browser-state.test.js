import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import { runInNewContext } from "node:vm";
import test from "node:test";
import ts from "typescript";

const source = await readFile(new URL("../browser/runtime/browser-state.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.CommonJS, target: ts.ScriptTarget.ES2022 },
}).outputText;

function fixture() {
  const document = { activeElement: null, body: {} };
  class Element {
    constructor(key, inDialog = false) { this.dataset = { focusKey: key }; this.inDialog = inDialog; }
    closest() { return this.inDialog ? {} : null; }
    focus() { document.activeElement = this; }
  }
  class Input extends Element {
    selectionStart = 0;
    selectionEnd = 0;
    setSelectionRange(start, end) { this.selectionStart = start; this.selectionEnd = end; }
  }
  const elements = new Map();
  const root = {
    dialogOpen: false,
    contains(element) { return [...elements.values()].includes(element); },
    querySelectorAll() { return []; },
    querySelector(selector) {
      if (selector === '[role="dialog"][aria-modal="true"]') return this.dialogOpen ? {} : null;
      return elements.get(selector.slice('[data-focus-key="'.length, -2)) ?? null;
    },
  };
  const exports = {};
  runInNewContext(compiled, {
    exports, document, HTMLElement: Element, HTMLInputElement: Input,
    HTMLTextAreaElement: Input, CSS: { escape: (key) => key },
  });
  const add = (key, dialog = false) => {
    const element = new Input(key, dialog);
    elements.set(key, element);
    return element;
  };
  return { ...exports, root, document, elements, add };
}

test("ordinary dialog input and row updates retain opener across close and reopen", () => {
  const f = fixture();
  const opener = f.add("view:seat:test:surface:a:item:open");
  const nextOpener = f.add("view:seat:test:surface:b:item:open");
  for (const expected of [opener, nextOpener]) {
    expected.focus();
    const beforeOpen = f.captureBrowserPresentation(f.root);
    f.root.dialogOpen = true;
    const query = f.add("input:seat:test:dialog:launch:query", true);
    query.focus();
    f.restoreBrowserPresentation(f.root, beforeOpen);
    assert.equal(f.document.activeElement, query);
    f.restoreBrowserPresentation(f.root, f.captureBrowserPresentation(f.root));
    const row = f.add("view:seat:test:dialog:launch:item:choice", true);
    row.focus();
    f.restoreBrowserPresentation(f.root, f.captureBrowserPresentation(f.root));
    const beforeClose = f.captureBrowserPresentation(f.root);
    f.root.dialogOpen = false;
    f.elements.delete(query.dataset.focusKey);
    f.elements.delete(row.dataset.focusKey);
    f.document.activeElement = f.document.body;
    f.restoreBrowserPresentation(f.root, beforeClose);
    assert.equal(f.document.activeElement, expected);
  }
});

test("ordinary input focus and selection restoration remain keyed to the exact mount", () => {
  const f = fixture();
  const input = f.add("input:seat:test:surface:a:tile:1:message");
  input.focus();
  input.setSelectionRange(2, 5);
  const captured = f.captureBrowserPresentation(f.root);
  input.setSelectionRange(0, 0);
  f.document.activeElement = f.document.body;
  f.restoreBrowserPresentation(f.root, captured);
  assert.equal(f.document.activeElement, input);
  assert.equal(input.selectionStart, 2);
  assert.equal(input.selectionEnd, 5);
});
