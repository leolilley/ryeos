import assert from "node:assert/strict";
import { execFileSync } from "node:child_process";
import { createRequire } from "node:module";
import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import path from "node:path";
import process from "node:process";
import { fileURLToPath } from "node:url";

const require = createRequire(import.meta.url);
const browserTestDirectory = path.dirname(fileURLToPath(import.meta.url));
const webCrateDirectory = path.resolve(browserTestDirectory, "../..");
const assetDirectory = path.join(webCrateDirectory, "pkg");

function loadPlaywright() {
  const requested = process.env.RYEOS_PLAYWRIGHT_PACKAGE || "playwright";
  const attempts = [requested];
  if (!path.isAbsolute(requested)) {
    try {
      const globalRoot = execFileSync("npm", ["root", "-g"], {
        encoding: "utf8",
        stdio: ["ignore", "pipe", "ignore"],
      }).trim();
      if (globalRoot) {
        attempts.push(path.join(globalRoot, requested));
        // Some distributions expose only @playwright/test globally while its
        // runtime package remains an exact nested dependency.
        if (requested === "playwright") {
          attempts.push(path.join(globalRoot, "@playwright/test/node_modules/playwright"));
        }
      }
    } catch {
      // The final error below describes the supported explicit override.
    }
  }

  let lastError;
  for (const candidate of attempts) {
    try {
      return require(candidate);
    } catch (error) {
      lastError = error;
    }
  }
  throw new Error(
    `real-browser checks require an existing Playwright installation; `
      + `set RYEOS_PLAYWRIGHT_PACKAGE to its package name or absolute path (${lastError?.message || "not found"})`,
  );
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

    return {
      mainTag: main.tagName,
      selectedRows: root.querySelectorAll(".ryeos-table-row.selected").length,
      events,
    };
  });
}

async function qualifyNativeFocus(page) {
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
    const enabled = [...root.querySelectorAll("button:not([disabled])")];
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
    check(document.activeElement === input, "Tab from the final control must wrap to the first control");

    input.focus();
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

  const component = await renderComponentFixture(page);
  const focus = await qualifyNativeFocus(page);
  const binding = await qualifyBindingWire(page);
  assert.deepEqual(pageErrors, [], `browser emitted page errors: ${pageErrors.join("; ")}`);
  assert.equal(component.mainTag, "MAIN");
  assert.equal(component.selectedRows, 1);
  assert.equal(focus.enabledChoices, 2);
  assert.equal(binding.binding_digest, "sha256:binding");
  console.log(JSON.stringify({ browser: browserName, component, focus, binding }));
} catch (error) {
  console.error(error instanceof Error ? error.stack : error);
  process.exitCode = 1;
} finally {
  await browser?.close();
  await closeServer(server);
}
