import assert from "node:assert/strict";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFile, mkdir } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const browserTestDirectory = path.dirname(fileURLToPath(import.meta.url));
const webCrateDirectory = path.resolve(browserTestDirectory, "../..");
const assetDirectory = path.join(webCrateDirectory, "pkg");

function loadPlaywright() {
  // Playwright and its browser revision are exact build/test inputs from this
  // crate's lockfile. A global package or host fallback would make accepted
  // screenshots depend on whichever developer machine happened to run them.
  return require("playwright");
}

function contentType(file) {
  if (file.endsWith(".js")) return "application/javascript; charset=utf-8";
  if (file.endsWith(".css")) return "text/css; charset=utf-8";
  if (file.endsWith(".wasm")) return "application/wasm";
  return "application/octet-stream";
}

function staticServer() {
  return createServer(async (request, response) => {
    try {
      if (request.url === "/") {
        response.writeHead(200, { "content-type": "text/html; charset=utf-8" });
        response.end(`<!doctype html>
<html lang="en">
  <head>
    <meta charset="utf-8">
    <meta name="viewport" content="width=device-width, initial-scale=1">
    <link rel="stylesheet" href="/ui/assets/web-shell.css">
    <title>RyeOS browser component qualification</title>
  </head>
  <body><div id="test-root"></div></body>
</html>`);
        return;
      }
      const prefix = "/ui/assets/";
      if (!request.url?.startsWith(prefix)) {
        response.writeHead(404).end();
        return;
      }
      const relative = decodeURIComponent(request.url.slice(prefix.length));
      const file = path.resolve(assetDirectory, relative);
      if (path.dirname(file) !== assetDirectory) {
        response.writeHead(404).end();
        return;
      }
      const bytes = await readFile(file);
      response.writeHead(200, {
        "content-type": contentType(file),
        "cache-control": "no-store",
      });
      response.end(bytes);
    } catch {
      response.writeHead(404).end();
    }
  });
}

async function listen(server) {
  await new Promise((resolve, reject) => {
    server.once("error", reject);
    server.listen(0, "127.0.0.1", resolve);
  });
  const address = server.address();
  assert.ok(address && typeof address === "object");
  return `http://127.0.0.1:${address.port}`;
}

async function closeServer(server) {
  await new Promise((resolve, reject) => {
    server.close((error) => error ? reject(error) : resolve());
  });
}

async function renderComponentFixture(page) {
  return page.evaluate(async () => {
    const check = (condition, message) => {
      if (!condition) throw new Error(message);
    };
    const { ryeosWorkspace } = await import("/ui/assets/ryeos_components_workspace.js");
    const root = document.getElementById("test-root");
    const events = [];
    const dispatch = (event) => events.push(event);
    const workspace = ryeosWorkspace({
      center_is_empty: false,
      docks: {},
      root: {
        type: "tile",
        tile_id: "work",
        instance_key: "work:stable",
        title: "Work",
        focused: true,
        input: {
          address: { session_id: "fixture", binding_digest: "fixture-binding", workspace_index: 0, workspace_id: 1,
            buffer: { view_instance_key: "tile:1", view_ref: "view:test/work", input_id: "line" } },
          text: "", focused: false, submit_enabled: true, live_filter: false,
        },
        group_id: "group-one",
        tabs: [
          { tile_id: "work", title: "Work", active: true },
          { tile_id: "evidence", title: "Evidence", active: false },
        ],
        view: {
          type: "table",
          title: "Running work",
          columns: ["Work", "State"],
          rows: [{
            cells: ["<script>not markup</script>", "running"],
            tone: "warn",
            selected: true,
            intent: { type: "inspect", item_ref: "thread:one" },
          }],
        },
      },
    }, null, [], dispatch);
    root.replaceChildren(workspace);

    const main = root.querySelector("main.ryeos-workspace");
    const table = root.querySelector(".ryeos-table");
    const row = root.querySelector("button.ryeos-table-row");
    check(main, "workspace must expose its main landmark");
    check(table, "table semantic VM must render a table component");
    check(row?.textContent.includes("<script>not markup</script>"), "untrusted text must remain visible as text");
    check(!root.querySelector("script"), "untrusted text must not create executable markup");
    check(row?.classList.contains("selected"), "selected row state must be exposed structurally");
    row.click();
    check(events.length === 1 && events[0]?.type === "activate", "row activation must emit one semantic intent");

    const tabs = [...root.querySelectorAll('[role="tab"]')];
    check(tabs.length === 2, "a shared view group must project both tabs");
    check(tabs[0].getAttribute("aria-selected") === "true", "selection comes from the VM");
    check(tabs[0].tabIndex === 0 && tabs[1].tabIndex === -1, "view tabs use roving keyboard focus");
    tabs[1].click();
    check(events.at(-1)?.type === "focus_changed" && events.at(-1)?.target === "evidence", "tab activation must target the exact mounted instance");
    tabs[0].dispatchEvent(new KeyboardEvent("keydown", { key: "ArrowLeft", bubbles: true, cancelable: true }));
    check(events.at(-1)?.target === "evidence", "tab keyboard navigation wraps through shared membership");
    check(document.activeElement === tabs[1], "keyboard focus follows the selected tab before reprojection");

    const composer = root.querySelector("textarea");
    check(composer && table, "a composer must coexist with its view content");
    composer.value = "draft";
    composer.setSelectionRange(5, 5);
    composer.dispatchEvent(new Event("input", { bubbles: true }));
    check(events.at(-1)?.type === "input_at", "browser input must carry an exact origin");
    check(events.at(-1)?.address?.buffer?.view_ref === "view:test/work", "input retains its admitted view coordinate");
    check(events.at(-1)?.action?.text === "draft", "addressed edit preserves typed text");
    root.querySelector(".ryeos-input-submit").click();
    check(events.at(-1)?.action?.type === "submit", "composer submit uses the addressed path");

    const editor = ryeosWorkspace({
      layout_guard: "exact-layout", split_min_ratio: .1, split_max_ratio: .9,
      docks: {}, center_is_empty: false,
      root: { type: "split", axis: "horizontal", ratio: .5,
        first: { type: "tile", tile_id: "1", instance_key: "tile:1", title: "One", tabs: [{tile_id: "1", title: "One", active: true}], view: {type: "text", lines: []} },
        second: { type: "tile", tile_id: "2", instance_key: "tile:2", title: "Two", tabs: [{tile_id: "2", title: "Two", active: true}], view: {type: "text", lines: []} },
      },
    }, null, [], dispatch);
    root.append(editor);
    const divider = editor.querySelector('[role="separator"]');
    check(divider?.tabIndex === 0, "split divider must be keyboard reachable");
    divider.dispatchEvent(new KeyboardEvent("keydown", {key: "ArrowRight", bubbles: true, cancelable: true}));
    check(events.at(-1)?.intent?.type === "resize_split", "divider emits shared resize intent");
    check(events.at(-1)?.intent?.layout_guard === "exact-layout", "resize retains the originating layout guard");
    check(events.at(-1)?.intent?.path.length === 0 && events.at(-1)?.intent?.ratio === .51, "resize addresses the exact split");
    editor.remove();

    return {
      mainTag: main.tagName,
      selectedRows: root.querySelectorAll(".ryeos-table-row.selected").length,
      events,
    };
  });
}

async function qualifyNativeFocus(page) {
  await page.evaluate(async () => {
    const { topStatusLine } = await import("/ui/assets/ryeos_components_home.js");
    const events = [];
    const top = topStatusLine({ presentation: { chrome: { top_bar: {
      visible: true,
      tabs: [
        { workspace_id: 51, number: 1, title: "Overview", active: true },
        { workspace_id: 89, number: 2, title: "Work", active: false },
      ],
    } } } }, { dispatchUi: (event) => events.push(event) });
    document.getElementById("test-root").replaceChildren(top);
    const button = top.querySelector(".ryeos-workspace-tabs button");
    button.click();
    if (events.at(-1)?.intent?.workspace_id !== 51) throw new Error("workspace selection must address identity");
    button.dispatchEvent(new KeyboardEvent("keydown", { key: "F2", bubbles: true }));
    const editor = top.querySelector("input");
    if (!editor || document.activeElement !== editor) throw new Error("workspace rename must focus its editor");
    editor.value = "My work";
    editor.dispatchEvent(new KeyboardEvent("keydown", { key: "Enter", bubbles: true }));
    if (events.at(-1)?.intent?.type !== "rename_workspace" || events.at(-1)?.intent?.title !== "My work") {
      throw new Error("workspace rename must emit shared semantic action");
    }
    top.querySelector(".ryeos-workspace-close").click();
    if (events.at(-1)?.intent?.type !== "close_workspace" || events.at(-1)?.intent?.workspace_id !== 51) {
      throw new Error("workspace close must address exact identity");
    }
  });
  return page.evaluate(async () => {
    const check = (condition, message) => {
      if (!condition) throw new Error(message);
    };
    const { overlayDialog } = await import("/ui/assets/ryeos_components_chrome.js");
    const root = document.getElementById("test-root");
    const calls = [];
    const overlay = overlayDialog({
      open: true,
      title: "Open work",
      selected: 0,
      items: [
        { label: "First", enabled: true },
        { label: "Unavailable", enabled: false },
        { label: "Last", enabled: true },
      ],
    }, {
      chooseOverlay: () => calls.push("choose"),
      closeOverlay: () => calls.push("close"),
    });
    root.replaceChildren(overlay);

    const dialog = root.querySelector('[role="dialog"]');
    const input = root.querySelector('input[role="combobox"]');
    const enabled = [...root.querySelectorAll(".ryeos-command-choice:not([disabled])")];
    const close = root.querySelector('button[aria-label="Close launcher"]');
    const disabled = root.querySelector("button[disabled]");
    check(dialog?.getAttribute("aria-modal") === "true", "overlay must expose a modal dialog");
    check(disabled, "disabled choices must remain non-focusable native controls");
    check(enabled.length === 2, "fixture must have two enabled choices");

    enabled.at(-1).focus();
    overlay.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Tab",
      bubbles: true,
      cancelable: true,
    }));
    check(document.activeElement === close, "Tab from the final control must wrap to the close control");

    close.focus();
    overlay.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Tab",
      shiftKey: true,
      bubbles: true,
      cancelable: true,
    }));
    check(document.activeElement === enabled.at(-1), "Shift+Tab from the first control must wrap to the final control");

    input.dispatchEvent(new KeyboardEvent("keydown", {
      key: "Escape",
      bubbles: true,
      cancelable: true,
    }));
    check(calls.at(-1) === "close", "Escape must request overlay closure");
    return { enabledChoices: enabled.length, closeCalls: calls.length };
  });
}

async function qualifyBindingWire(page) {
  return page.evaluate(async () => {
    const { runEffect } = await import("/ui/assets/ryeos_effects.js");
    const posted = [];
    const originalFetch = globalThis.fetch;
    globalThis.fetch = async (url, init) => {
      posted.push({ url, body: JSON.parse(init.body) });
      return new Response(JSON.stringify({ result: { rows: [] } }), {
        status: 200,
        headers: { "content-type": "application/json" },
      });
    };
    try {
      const request = {
        binding_digest: "sha256:binding",
        coordinate: { kind: "source", view_ref: "view:test/work", channel: "default" },
        payload: { kind: "source_parameters", params: { limit: 5 } },
      };
      await runEffect({
        id: 7,
        kind: {
          type: "fetch_source",
          tile_id: "tile-1",
          request,
          request_bounds: { max_request_bytes: 4096, max_input_bytes: 128 },
        },
      });
      const body = posted[0]?.body;
      if (posted[0]?.url !== "/ui/api/invocations/dispatch") throw new Error("binding used wrong endpoint");
      if (JSON.stringify(body) !== JSON.stringify(request)) throw new Error("renderer changed binding request");
      for (const forbidden of ["target", "item_ref", "ref_bindings", "capabilities", "read_only"]) {
        if (Object.hasOwn(body, forbidden)) throw new Error(`binding wire leaked ${forbidden}`);
      }
      return body;
    } finally {
      globalThis.fetch = originalFetch;
    }
  });
}

const server = staticServer();
let browser;
try {
  const origin = await listen(server);
  const playwright = loadPlaywright();
  const browserName = process.env.RYEOS_PLAYWRIGHT_BROWSER || "chromium";
  const browserType = playwright[browserName];
  if (!browserType) throw new Error(`unsupported RYEOS_PLAYWRIGHT_BROWSER: ${browserName}`);
  browser = await browserType.launch({ headless: true });
  const context = await browser.newContext({
    viewport: { width: 390, height: 844 },
    reducedMotion: "reduce",
  });
  const page = await context.newPage();
  const pageErrors = [];
  page.on("pageerror", (error) => pageErrors.push(error.message));
  await page.goto(origin, { waitUntil: "load" });

  await page.setViewportSize({ width:1440, height:900 });
  const component = await renderComponentFixture(page);
  await page.setViewportSize({ width:390, height:844 });
  const focus = await qualifyNativeFocus(page);
  const binding = await qualifyBindingWire(page);
  assert.deepEqual(pageErrors, [], `browser emitted page errors: ${pageErrors.join("; ")}`);
  assert.equal(component.mainTag, "MAIN");
  assert.equal(component.selectedRows, 1);
  assert.equal(focus.enabledChoices, 2);
  assert.equal(binding.binding_digest, "sha256:binding");
  console.log(JSON.stringify({ browser: browserName, component, focus, binding }));
  if (process.env.RYEOS_UI_VISUAL_FIXTURE) {
    // Envelopes come from the real Rust model example, never a handcrafted VM.
    // The data is synthetic; no daemon/seat or effect executor is attached.
    const fixtures = JSON.parse(await readFile(process.env.RYEOS_UI_VISUAL_FIXTURE, "utf8"));
    const output = process.env.RYEOS_UI_SCREENSHOT_DIR || "/tmp/ryeos-ui-production-preview";
    await mkdir(output, { recursive: true });
    await page.route("https://**", (route) => route.abort());
    await page.setViewportSize({ width:1600, height:1000 });
    for (const name of ["work", "overview", "launcher"]) {
      await page.evaluate(async (envelope) => {
        const { renderDom } = await import("/ui/assets/ryeos_dom_adapter.js");
        const { seededAmbientRandom } = await import("/ui/assets/ryeos_ambient_scene.js");
        renderDom(document.getElementById("test-root"), envelope.view_model, envelope.scene_model,
          () => {}, {
            dispatchUi: () => {},
            closeOverlay: () => {},
            ambientPlatformKey: "visual-baseline-v1",
            ambientPlatform: { random: seededAmbientRandom(0x5259454f) },
          });
      }, fixtures[name]);
      if (name === "work") {
        assert.equal(await page.locator(".ryeos-content-heading h1").first().textContent(), "A clearer view of work.");
        assert.equal(await page.locator(".ryeos-tile-chrome + .ryeos-view-tabs").count(), 1, "tabs sit below, not inside, the frame header");
        assert.equal(await page.locator(".ryeos-scene-diagram").count(), 1, "authored scene uses the shared scene projection");
        assert.equal(await page.locator(".ryeos-code-line").count(), 5, "code excerpt renders authored lines, not injected markup");
        assert.equal(await page.locator(".ryeos-statusbar:not(.hidden)").count(), 1, "bottom strip is visible by default");
        assert.equal(await page.locator(".ryeos-tile-footer:not([hidden])").count(), 0, "raw provenance does not consume an empty footer row");
        await page.locator(".ryeos-status-details summary").click();
        assert.equal(await page.locator(".ryeos-status-details").getAttribute("open"), "", "session diagnostics remain accessible");
        await page.locator(".ryeos-status-details summary").click();
      }
      await page.screenshot({ path:path.join(output, `${name}.png`) });
    }
    for (const [width, height] of [[1024,768], [390,844]]) {
      await page.setViewportSize({ width, height });
      await page.evaluate(async (envelope) => {
        const { renderDom } = await import("/ui/assets/ryeos_dom_adapter.js");
        const { seededAmbientRandom } = await import("/ui/assets/ryeos_ambient_scene.js");
        renderDom(document.getElementById("test-root"), envelope.view_model, envelope.scene_model,
          () => {}, {
            ambientPlatformKey: "visual-baseline-v1",
            ambientPlatform: { random: seededAmbientRandom(0x5259454f) },
          });
      }, fixtures.work);
      await page.screenshot({ path:path.join(output, `work-${width}.png`) });
      if (width === 390) {
        assert.equal(await page.locator(".ryeos-region-chooser button").count(), 4, "narrow view keeps centre groups and slot reachable");
        assert.equal(await page.locator(".ryeos-compact-content > .ryeos-tile").count(), 1, "narrow view projects one focused group");
        const geometry = await page.evaluate(() => {
          const tile = document.querySelector(".ryeos-compact-content > .ryeos-tile");
          const body = tile.querySelector(".ryeos-tile-body").getBoundingClientRect();
          const composer = tile.querySelector(".ryeos-input-dock").getBoundingClientRect();
          const bounds = tile.getBoundingClientRect();
          return {bodyHeight:body.height, contentBottom:body.bottom, composerTop:composer.top,
            left:bounds.left, right:bounds.right, animations:tile.getAnimations().length};
        });
        assert.ok(geometry.bodyHeight > 200, "composer must not consume the reading area");
        assert.ok(geometry.contentBottom <= geometry.composerTop + 1, "composer and content must not overlap");
        assert.ok(geometry.left >= 0 && geometry.right <= width, "focused group fits narrow viewport");
        assert.equal(geometry.animations, 0, "reduced motion disables JS geometry animations");
      }
    }
    assert.deepEqual(pageErrors, [], "production renderer preview must not emit page errors");
    console.log(`Production renderer screenshots (synthetic Rust-model fixture): ${output}`);
  }
} catch (error) {
  console.error(error instanceof Error ? error.stack : error);
  process.exitCode = 1;
} finally {
  await browser?.close();
  await closeServer(server);
}
