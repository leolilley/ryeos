import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { mkdir, readFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";

const require = createRequire(import.meta.url);
const stage = path.resolve(process.env.RYEOS_UI_SVELTE_STAGE || "../../../target/ui-browser-stage");
const server = createServer(async (request, response) => {
  if (request.url === "/") {
    response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
    response.end('<!doctype html><html><head><link rel="stylesheet" href="/ui/assets/ryeos_ui.css"></head><body><div id="root" style="width:100%;height:100%"></div></body></html>');
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
const pageErrors = [];
function observePageErrors(page) {
  page.on("pageerror", (error) => pageErrors.push(error.message));
}
try {
  const address = server.address();
  assert.ok(address && typeof address === "object");
  const browserName = process.env.RYEOS_PLAYWRIGHT_BROWSER || "chromium";
  const browserType = require("playwright")[browserName];
  assert.ok(browserType, `unknown RYEOS_PLAYWRIGHT_BROWSER: ${browserName}`);
  browser = await browserType.launch({
    headless: true,
    ...(process.env.RYEOS_PLAYWRIGHT_EXECUTABLE_PATH
      ? { executablePath: process.env.RYEOS_PLAYWRIGHT_EXECUTABLE_PATH }
      : {}),
  });
  const page = await browser.newPage();
  observePageErrors(page);
  await page.goto(`http://127.0.0.1:${address.port}`);
  const result = await page.evaluate(async () => {
    const { mountRyeOsRenderer, captureBrowserPresentation, restoreBrowserPresentation, encodeCanonicalJsonBody } = await import("/ui/assets/ryeos_ui.js");
    // Renderer-only VM fixtures, not a fabricated session/admission model.
    // Shape follows Rust's current shared dialog + zero-Surface projection.
    const presentationId = "11111111-1111-4111-8111-111111111111";
    const dialogInstance = `seat:${presentationId}:dialog:views`;
    const binding = { binding_attachment_id: "root", binding_generation: 1n, binding_digest: "a".repeat(64) };
    const bindingScope = [...new Uint8Array(await crypto.subtle.digest("SHA-256",
      new TextEncoder().encode(encodeCanonicalJsonBody(binding))))]
      .map((byte) => byte.toString(16).padStart(2, "0")).join("");
    const fixture = (generation) => ({
      schema_version: "ryeos.ui.envelope.v1", generation, effects: [],
      scene_model: { schema_version: "ryeos.ui.scene.v1", generation, camera: { position: [0, 0, 10], target: [0, 0, 0], fov_degrees: 45 }, objects: [], actions: [], energy: 0, screen_offset: [0, 0], break_amount: 1 },
      view_model: {
        schema_version: "ryeos.ui.vm.v2", generation, now_ms: 0n,
        session: { session_id: "test", project_path: null, root_ref: "config:test/presentation", title: "Test presentation", surface_ordinal: null, surfaces: [], ambient: { show_background: false, opacity: null, mode: "ambient", atlas: null }, user_principal_id: null, posture: "observation_only" },
        navigation: { items: [] },
        chrome: { title: "RyeOS", subtitle: "", health_label: "ready", health_tone: "good" },
        presentation: {
          schema_version: "ryeos.ui.presentation.v1", theme: { id: "gruvbox", tone: "neutral" },
          chrome: {
            title: "RyeOS", version_label: "test", border: "thin",
            top_bar: { visible: true, tabs: [], focused_title: "", layout_symbol: "" },
            status_bar: { visible: true, segments: [], key_hint: "", energy: 0, attention: null },
          },
          metrics: { tile_count: 0n, scene_object_count: 0n, item_count: 0n, thread_count: 0n, project_count: 0n, service_count: 0n, schedule_count: 0n, active_thread_count: 0n, activity_level: 0 }, frame: { corners: { visible: false, tone: "neutral" } }, motion: [],
        },
        view_set: null,
        docks: { top: null, bottom: null, left: null, right: null }, backdrop: null,
        dialogs: [], notices: [], transport: { freshness: "current", last_observed_at_ms: null, channels: [] },
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
    const row = (id, primary, cursor, kind = null, secondary = null) => ({
      id, primary, cursor, kind, secondary, meta: null, glyph: null, intent: null,
      tone: "neutral", selected: false, expandable: false, expanded: false, detail: [],
    });
    modal.view_model.dialogs = [{
      instance_key: dialogInstance, title: "Views",
      view: { type: "rows", title: "Views", columns: [], total_rows: 3n, affordance_hints: [], rows: [
        row("dialog-one", "One", 0n), row("dialog-two", "Two", 1n),
        row("dialog-disabled", "Blocked", 2n, "unavailable", "Exact authority required"),
      ] },
      input: { address: {
        session_id: "test", ...binding,
        owner: { kind: "shared", presentation_id: presentationId },
        buffer: { view_instance_key: dialogInstance, binding_scope: bindingScope, view_ref: "view:test/views", input_id: "query", target_scope: null },
      }, text: "", cursor: 0n, focused: true, route_label: "Find Views", placeholder: "Find a view", hint: "Choose a view", submit_enabled: false, live_filter: true, completion: [] },
    }];
    renderer.replaceEnvelope(modal);
    await new Promise((resolve) => requestAnimationFrame(resolve));
    restoreBrowserPresentation(root, beforeModal);
    const item = root.querySelectorAll(".dialog-content .view-record-line > button")[1];
    root.querySelector(".dialog-content textarea")?.dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowDown", bubbles: true, cancelable: true }));
    item?.click();
    const notice = root.querySelector(".notice span")?.textContent;
    const queryFocused = document.activeElement?.matches(".dialog-content textarea") ?? false;
    root.querySelector(".notice button")?.click();
    const dialog = {
      dialog: root.querySelector('[role="dialog"][aria-modal="true"]') !== null,
      notice,
      queryFocused,
      navigationAbsent: root.querySelector(".navigation") === null,
      disabledReason: root.querySelector('button[data-kind="unavailable"] small')?.textContent,
      disabledBlocked: root.querySelector('button[data-kind="unavailable"][aria-disabled="true"]')?.disabled === true,
      events: dispatched.map((event) => ({ type: event.type, itemId: "item_id" in event ? event.item_id : null, action: event.action?.type ?? null, instance: event.instance_key ?? event.address?.buffer.view_instance_key ?? null })),
    };
    const beforeClose = captureBrowserPresentation(root);
    renderer.replaceEnvelope(fixture(4n));
    await new Promise((resolve) => requestAnimationFrame(resolve));
    restoreBrowserPresentation(root, beforeClose);
    dialog.returnFocused = document.activeElement === returnButton;
    await renderer.destroy();
    return { ...observed, dialog, children: root.childElementCount };
  });
  assert.deepEqual(result, {
    firstGeneration: "1", secondGeneration: "2", reused: true,
    dialog: {
      dialog: true, notice: "Exact refusal", queryFocused: true, navigationAbsent: true, disabledReason: "Exact authority required", disabledBlocked: true, returnFocused: true,
      events: [
        { type: "input_at", itemId: null, action: "focus", instance: "seat:11111111-1111-4111-8111-111111111111:dialog:views" },
        { type: "input_at", itemId: null, action: "navigate_selection", instance: "seat:11111111-1111-4111-8111-111111111111:dialog:views" },
        { type: "choose_view_item", itemId: "dialog-two", action: null, instance: "seat:11111111-1111-4111-8111-111111111111:dialog:views" },
        { type: "dismiss_notice", itemId: null, action: null, instance: null },
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

  const visualFixturePath = process.env.RYEOS_UI_VISUAL_FIXTURE;
  if (visualFixturePath) {
    const encodedFixture = await readFile(path.resolve(visualFixturePath), "utf8");
    const screenshotDir = path.resolve(process.env.RYEOS_UI_SCREENSHOT_DIR || "/tmp/ryeos-ui-production-preview");
    await mkdir(screenshotDir, { recursive: true });
    const captures = [
      ["work", 1600, 1000],
      ["overview", 1600, 1000],
      ["launcher", 1600, 1000],
      ["work", 1024, 768],
      ["work", 390, 844],
    ];
    for (const [name, width, height] of captures) {
      const preview = await browser.newPage({ viewport: { width, height }, reducedMotion: "reduce" });
      observePageErrors(preview);
      await preview.route(/^https?:\/\/(?!127\.0\.0\.1(?::|\/))/, (route) => route.abort());
      await preview.goto(`http://127.0.0.1:${address.port}`);
      const mounted = await preview.evaluate(async ({ encodedFixture, name, source }) => {
        const { decodeJsonBody, mountRyeOsRenderer } = await import("/ui/assets/ryeos_ui.js");
        const fixture = decodeJsonBody(encodedFixture);
        if (!fixture || typeof fixture !== "object" || !(name in fixture)) return { found: false, labeled: false };
        const envelope = fixture[name];
        const labeled = envelope?.view_model?.notices?.some((notice) => /synthetic|sample|visual preview/i.test(notice.message)) ?? false;
        const root = document.getElementById("root");
        root.dataset.visualFixture = "synthetic";
        root.dataset.fixtureSource = source;
        mountRyeOsRenderer(root, envelope, () => {});
        return { found: true, labeled };
      }, { encodedFixture, name, source: path.basename(visualFixturePath) });
      assert.equal(mounted.found, true, `visual fixture is missing the ${name} Rust envelope`);
      assert.equal(mounted.labeled, true, `${name} fixture must visibly identify itself as synthetic/sample data`);
      await preview.evaluate(() => document.fonts.ready);
      await preview.evaluate(() => new Promise((resolve) => requestAnimationFrame(() => requestAnimationFrame(resolve))));
      await preview.screenshot({ path: path.join(screenshotDir, `${name}-${width}x${height}.png`), fullPage: true });
      await preview.close();
    }
  }
  assert.deepEqual(pageErrors, [], "compiled renderer must not emit uncaught browser errors");
  console.log(JSON.stringify({ root: result, ordering, encoding, pageErrors }));
} finally {
  await browser?.close();
  await new Promise((resolve, reject) => server.close((error) => error ? reject(error) : resolve()));
}
