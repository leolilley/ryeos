import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";
import ts from "typescript";
const source = await readFile(new URL("../browser/runtime/keyboard.ts", import.meta.url), "utf8");
const boot = await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8");
const compiled = ts.transpileModule(source, {
  compilerOptions: { module: ts.ModuleKind.ESNext, target: ts.ScriptTarget.ES2022 },
}).outputText;
const keyboard = await import(`data:text/javascript;base64,${Buffer.from(compiled).toString("base64")}`);

class TestElement {
  constructor(tag = "span", parentElement = null) {
    this.tag = tag;
    this.parentElement = parentElement;
  }
  closest() {
    for (let current = this; current; current = current.parentElement) {
      if (["input", "textarea", "select"].includes(current.tag)) return current;
    }
    return null;
  }
}
class TestHtmlElement extends TestElement {
  constructor(tag = "span", parentElement = null, contentEditable = undefined) {
    super(tag, parentElement);
    this.contentEditable = contentEditable;
  }
  get isContentEditable() {
    if (this.contentEditable !== undefined) {
      return ["", "true", "plaintext-only"].includes(this.contentEditable);
    }
    return this.parentElement?.isContentEditable ?? false;
  }
}
globalThis.Element = TestElement;
globalThis.HTMLElement = TestHtmlElement;

test("only the admitted launcher chord escapes browser typing focus", () => {
  const helper = source.slice(source.indexOf("export function isLauncherChord"), source.indexOf("export function isTypingTarget"));
  assert.match(helper, /!event\.isComposing/);
  assert.match(helper, /event\.key\.toLowerCase\(\) === "k"/);
  assert.match(helper, /!event\.shiftKey && !event\.metaKey/);
  assert.match(helper, /event\.ctrlKey !== event\.altKey/);
  assert.match(boot, /isTypingEvent\(event\) && !isLauncherChord\(event\)/);
});

test("effective editable state owns ordinary typing, including inherited and plaintext editors", () => {
  const editor = new TestHtmlElement("div", null, "");
  const inherited = new TestHtmlElement("span", editor);
  const svgChild = new TestElement("svg", editor);
  const disabled = new TestHtmlElement("span", editor, "false");
  const plaintext = new TestHtmlElement("div", null, "plaintext-only");
  assert.equal(keyboard.isTypingTarget(editor), true);
  assert.equal(keyboard.isTypingTarget(inherited), true);
  assert.equal(keyboard.isTypingTarget(svgChild), true);
  assert.equal(keyboard.isTypingTarget(plaintext), true);
  assert.equal(keyboard.isTypingTarget(disabled), false);
  assert.equal(keyboard.isTypingTarget(new TestHtmlElement("input", disabled)), true);
  assert.equal(keyboard.isTypingTarget(new TestHtmlElement("div")), false);
  assert.equal(keyboard.isTypingTarget(null), false);
  assert.equal(keyboard.isTypingEvent({ target: new TestHtmlElement("div"), composedPath: () => [inherited, editor] }), true);
  assert.equal(keyboard.isTypingEvent({ target: new TestHtmlElement("div"), composedPath: () => [disabled, editor] }), false);
});
