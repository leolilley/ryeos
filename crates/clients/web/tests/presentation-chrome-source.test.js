import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const source = await readFile(new URL("../browser/app/SystemBar.svelte", import.meta.url), "utf8");
const boot = await readFile(new URL("../browser/runtime/boot.ts", import.meta.url), "utf8");

test("Surface disclosure dismissal restores stable chrome focus without choosing presentation state", () => {
  const helper = source.slice(source.indexOf("function dismissSurfacePicker"), source.indexOf("</script>"));
  assert.match(helper, /removeAttribute\("open"\)/);
  assert.match(helper, /querySelector<HTMLElement>\("summary"\)\?\.focus\(\)/);
  assert.match(helper, /event\.key !== "Escape"/);
  assert.match(helper, /event\.isComposing/);
  assert.match(helper, /event\.altKey \|\| event\.ctrlKey \|\| event\.metaKey \|\| event\.shiftKey/);
  assert.match(helper, /preventDefault\(\)/);
  assert.match(helper, /stopPropagation\(\)/);
  const pickerKey = helper.slice(helper.indexOf("function handleSurfacePickerKey"));
  assert.doesNotMatch(pickerKey.slice(0, pickerKey.indexOf("event.preventDefault()")), /!event\.currentTarget\.open/);
  assert.match(pickerKey, /const picker = event\.currentTarget\.closest\("details"\)/);
  assert.ok(pickerKey.indexOf("event.stopPropagation()") < pickerKey.indexOf("if (picker?.open) dismissSurfacePicker"));
  assert.doesNotMatch(helper, /dispatch|surface_instance_id|select_intent|close_intent/);
  const choices = source.slice(source.indexOf('<div class="surface-choices"'), source.indexOf('</details>'));
  assert.doesNotMatch(choices.split('>')[0], /onkeydown/);
  const buttons = [...choices.matchAll(/<button\b[^>]*>/g)];
  assert.equal(buttons.length, 5, "all Surface choice and rename buttons are covered");
  for (const [button] of buttons) {
    assert.match(button, /onkeydown=\{handleSurfacePickerKey\}/);
  }
});

test("global keymap never receives an IME composition key from chrome", () => {
  const keydown = boot.slice(boot.indexOf('window.addEventListener("keydown"'), boot.indexOf('window.addEventListener("resize"'));
  assert.ok(keydown.indexOf("if (event.defaultPrevented) return") < keydown.indexOf("if (event.isComposing) return"));
  assert.ok(keydown.indexOf("if (event.isComposing) return") < keydown.indexOf("const key = keyEvent(event)"));
});

test("selection dismisses the popup before dispatching the Rust-owned intent", () => {
  const selection = source.slice(source.indexOf('aria-current={surface.active'), source.indexOf("</button>", source.indexOf('aria-current={surface.active')));
  assert.ok(selection.indexOf("dismissSurfacePicker(picker)") < selection.indexOf('dispatch({ type: "activate", intent: surface.select_intent })'));
  assert.doesNotMatch(selection, /surface_instance_id\s*[:=]|title\s*[:=]|ordinal\s*[:=]/);
});

test("closing a Surface restores focus to chrome that survives the final close", () => {
  const close = source.slice(source.indexOf('class="surface-close"'), source.indexOf('</button>', source.indexOf('class="surface-close"')));
  assert.match(source, /class="system-context" role="group" aria-label="Surface context" tabindex="-1" bind:this=\{contextRoot\}/);
  assert.match(close, /const closingLast = session\.surfaces\.length === 1/);
  assert.match(close, /dismissSurfacePicker\(picker, !closingLast\)/);
  assert.match(close, /if \(closingLast\) contextRoot\?\.focus\(\)/);
  assert.match(close, /dispatch\(\{ type: "activate", intent: surface\.close_intent \}\)/);
  assert.ok(close.indexOf('if (!surface.close_intent) return') < close.indexOf('dismissSurfacePicker(picker, !closingLast)'));
  assert.ok(close.indexOf('contextRoot?.focus()') < close.indexOf('dispatch({ type: "activate", intent: surface.close_intent })'));
  assert.doesNotMatch(close, /surface_instance_id\s*[:=]|title\s*[:=]|ordinal\s*[:=]/);
});
