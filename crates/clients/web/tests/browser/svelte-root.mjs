import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const require = createRequire(import.meta.url);
const stage = path.resolve(process.env.RYEOS_UI_SVELTE_STAGE || "../../../target/ui-browser-stage");
const server = createServer(async (request, response) => {
  if (request.url === "/") {
    response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    response.end('<!doctype html><html><body><div id="root"></div></body></html>');
    return;
  }
  const file = request.url === "/ui/assets/ryeos_ui.js" ? "ryeos_ui.js"
    : request.url === "/ui/assets/ryeos_ui.css" ? "ryeos_ui.css" : null;
  if (!file) { response.writeHead(404).end(); return; }
  response.writeHead(200, { "content-type": file.endsWith(".css") ? "text/css" : "application/javascript" });
  response.end(await readFile(path.join(stage, file)));
});

await new Promise((resolve, reject) => {
  server.once("error", reject);
  server.listen(0, "127.0.0.1", resolve);
});

let browser;
try {
  const address = server.address();
  assert.ok(address && typeof address === "object");
  browser = await require("playwright").chromium.launch({ headless: true });
  const page = await browser.newPage();
  await page.goto(`http://127.0.0.1:${address.port}`);
  const result = await page.evaluate(async () => {
    const { mountRyeOsRenderer, captureBrowserPresentation, restoreBrowserPresentation } = await import("/ui/assets/ryeos_ui.js");
    const fixture = (generation) => ({
      schema_version: "test", generation, effects: [], scene_model: { objects: [] },
      view_model: {
        schema_version: "test", generation, now_ms: 0n,
        session: { session_id: "test", project_path: null, surface_ref: "surface:test", ambient: {}, user_principal_id: null, posture: "observe" },
        navigation: { items: [] },
        chrome: { title: "RyeOS", subtitle: "", health_label: "ready", health_tone: "good" },
        presentation: {
          schema_version: "test", theme: { id: "gruvbox", tone: "neutral" },
          chrome: {
            title: "RyeOS", version_label: "test", border: "thin",
            top_bar: { visible: true, tabs: [], focused_title: "", layout_symbol: "" },
            status_bar: { visible: true, segments: [], key_hint: "", energy: 0, attention: null },
          },
          metrics: {}, frame: {}, motion: [],
        },
        workspace: {
          layout_guard: "test", split_min_ratio: .1, split_max_ratio: .9,
          root: null, focused_tile: "", center_is_empty: true, tile_count: 0n,
          docks: { top: null, bottom: null, left: null, right: null }, lens_trail: [], lens_label: null,
        },
        overlays: [], notices: [], transport: { freshness: "live", last_observed_at_ms: null, channels: [] },
      },
    });
    const root = document.getElementById("root");
    const dispatched = [];
    const renderer = mountRyeOsRenderer(root, fixture(1n), (event) => dispatched.push(event));
    await new Promise((resolve) => requestAnimationFrame(resolve));
    const first = root.firstElementChild;
    renderer.replaceEnvelope(fixture(2n));
    await new Promise((resolve) => requestAnimationFrame(resolve));
    const observed = { firstGeneration: "1", secondGeneration: first?.getAttribute("data-generation"), reused: root.firstElementChild === first };
    const returnButton = document.createElement("button");
    returnButton.dataset.focusKey = "test:return";
    returnButton.textContent = "return";
    first?.append(returnButton);
    returnButton.focus();
    const beforeModal = captureBrowserPresentation(root);
    const modal = fixture(3n);
    modal.view_model.notices = [{ id: "n1", message: "Exact refusal", tone: "warn" }];
    modal.view_model.overlays = [{
      id: "views", title: "Views", widget: "palette", columns: [], query: "", selected: 0n, hint: "Choose a view",
      items: [
        { id: "overlay-one", category: "a", primary: "One", secondary: "", meta: "", enabled: true, intent: null, secondary_intent: null, depth: 0, header: false, expanded: false },
        { id: "overlay-two", category: "a", primary: "Two", secondary: "", meta: "", enabled: true, intent: null, secondary_intent: null, depth: 0, header: false, expanded: false },
        { id: "overlay-disabled", category: "a", primary: "Blocked", secondary: "", meta: "", enabled: false, disabled_reason: "Exact authority required", intent: null, secondary_intent: null, depth: 0, header: false, expanded: false },
      ],
    }];
    renderer.replaceEnvelope(modal);
    await new Promise((resolve) => requestAnimationFrame(resolve));
    restoreBrowserPresentation(root, beforeModal);
    const item = root.querySelectorAll(".overlay-item > button")[1];
    item?.dispatchEvent(new PointerEvent("pointerenter", { bubbles: true }));
    item?.click();
    const notice = root.querySelector(".notice span")?.textContent;
    const queryFocused = document.activeElement?.classList.contains("overlay-query") ?? false;
    root.querySelector(".notice button")?.click();
    const overlay = {
      dialog: root.querySelector('[role="dialog"][aria-modal="true"]') !== null,
      notice,
      queryFocused,
      navigationAbsent: root.querySelector(".navigation") === null,
      disabledReason: root.querySelector(".overlay-disabled-reason")?.textContent,
      disabledDescribed: root.querySelector('button[aria-describedby="overlay-reason-overlay-disabled"]') !== null,
      events: dispatched.map((event) => ({ type: event.type, itemId: "item_id" in event ? event.item_id : null })),
    };
    const beforeClose = captureBrowserPresentation(root);
    renderer.replaceEnvelope(fixture(4n));
    await new Promise((resolve) => requestAnimationFrame(resolve));
    restoreBrowserPresentation(root, beforeClose);
    overlay.returnFocused = document.activeElement === returnButton;
    await renderer.destroy();
    return { ...observed, overlay, children: root.childElementCount };
  });
  assert.deepEqual(result, {
    firstGeneration: "1", secondGeneration: "2", reused: true,
    overlay: {
      dialog: true, notice: "Exact refusal", queryFocused: true, navigationAbsent: true, disabledReason: "Exact authority required", disabledDescribed: true, returnFocused: true,
      events: [
        { type: "set_overlay_selection", itemId: "overlay-two" },
        { type: "choose_overlay_at", itemId: "overlay-two" },
        { type: "dismiss_notice", itemId: null },
      ],
    },
    children: 0,
  });

  const ordering = await page.evaluate(async () => {
    const { createCommitRuntime } = await import("/ui/assets/ryeos_ui.js");
    const scheduled = [], started = [], rendered = [], completions = new Map();
    const envelope = (generation, effects = []) => ({ generation, effects });
    const runtime = createCommitRuntime({
      reduce: (event) => event.envelope,
      applyEffectResult: (result) => envelope(result.generation),
      startEffect: (effect) => { started.push(effect.id); return new Promise((resolve) => completions.set(effect.id, resolve)); },
      failedEffectResult: (_effect, error) => { throw error; },
      render: (next) => rendered.push(next.generation),
      scheduleRender: (render) => scheduled.push(render),
    });
    runtime.enqueueEvent({ envelope: envelope(1, [{ id: 1 }, { id: 2 }]) });
    runtime.enqueueEvent({ envelope: envelope(2, [{ id: 2 }, { id: 3 }]) });
    await Promise.resolve();
    const startsBeforeRender = [...started];
    scheduled.shift()?.();
    await Promise.resolve();
    completions.get(1)?.({ generation: 3 });
    completions.get(2)?.({ generation: 4 });
    completions.get(3)?.({ generation: 5 });
    await Promise.resolve(); await Promise.resolve();
    scheduled.shift()?.();
    await runtime.idle();
    return { startsBeforeRender, rendered };
  });
  assert.deepEqual(ordering, { startsBeforeRender: [1, 2, 3], rendered: [2, 5] });
  const encoding = await page.evaluate(async () => {
    const { encodeCanonicalJsonBody, encodeJsonBody } = await import("/ui/assets/ryeos_ui.js");
    const nested = Object.create(null);
    nested["__proto__"] = "inert";
    const encoded = encodeJsonBody({ exact: 9007199254740993n, nested, values: [null, true, -0] });
    const canonical = encodeCanonicalJsonBody({
      "😀": "é",
      "𐀀": "supplementary",
      "": "bmp",
      a: "\u0001\n\t\"\\",
    });
    let mapRejected = false;
    try { encodeJsonBody(new Map([["lost", "value"]])); } catch { mapRejected = true; }
    let surrogateRejected = false;
    try { encodeCanonicalJsonBody({ bad: "\ud800" }); } catch { surrogateRejected = true; }
    return { encoded, canonical, mapRejected, surrogateRejected };
  });
  assert.equal(encoding.encoded, '{"exact":9007199254740993,"nested":{"__proto__":"inert"},"values":[null,true,0]}');
  assert.equal(encoding.canonical, '{"a":"\\u0001\\n\\t\\\"\\\\","\\ue000":"bmp","\\ud800\\udc00":"supplementary","\\ud83d\\ude00":"\\u00e9"}');
  assert.equal(encoding.mapRejected, true);
  assert.equal(encoding.surrogateRejected, true);
  console.log(JSON.stringify({ root: result, ordering, encoding }));
} finally {
  await browser?.close();
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}
