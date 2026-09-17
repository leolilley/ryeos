import assert from "node:assert/strict";
import { createHash } from "node:crypto";
import { readFile, readdir } from "node:fs/promises";
import test from "node:test";

const root = new URL("../", import.meta.url);
const packageRoot = new URL("pkg/", root);
const manifestUrl = new URL("../../daemon/ryeos-ui/web-assets.json", root);
const expected = [
  "index.html",
  "ryeos_three.js",
  "ryeos_ui.css",
  "ryeos_ui.js",
  "ryeos_web.js",
  "ryeos_web_bg.wasm",
];

test("packaged browser generation is the exact clean-cut closure", async () => {
  const actual = (await readdir(packageRoot)).sort();
  assert.deepEqual(actual, expected);
  const index = await readFile(new URL("index.html", packageRoot), "utf8");
  assert.match(index, /\/ui\/assets\/ryeos_ui\.css/);
  assert.match(index, /<script type="module" src="\/ui\/assets\/ryeos_ui\.js"><\/script>/);
  assert.doesNotMatch(index, /<script(?![^>]*\bsrc=)[^>]*>/);
  assert.doesNotMatch(index, /bootstrap|web-shell|ryeos_shell/);
});

test("typed asset manifest commits every packaged byte", async () => {
  const manifest = JSON.parse(await readFile(manifestUrl, "utf8"));
  assert.deepEqual(manifest.assets.map((asset) => asset.filename).sort(), expected);
  const routes = new Set(manifest.assets.map((asset) => asset.route));
  for (const asset of manifest.assets) {
    const bytes = await readFile(new URL(asset.filename, packageRoot));
    assert.equal(createHash("sha256").update(bytes).digest("hex"), asset.sha256);
    for (const imported of asset.imports) assert.ok(routes.has(imported));
  }
});

test("compiled entry contains no predecessor renderer vocabulary", async () => {
  const entry = await readFile(new URL("ryeos_ui.js", packageRoot), "utf8");
  for (const forbidden of ["ryeos_shell.js", "ryeos_dom_adapter.js", "bootstrap.js"]) {
    assert.equal(entry.includes(forbidden), false, `compiled entry retains ${forbidden}`);
  }
});

test("scene and field views retain the shared projection boundary", async () => {
  const scene = await readFile(new URL("browser/views/SceneView.svelte", root), "utf8");
  const renderer = await readFile(new URL("browser/views/ViewRenderer.svelte", root), "utf8");
  const field = await readFile(new URL("browser/views/FieldView.svelte", root), "utf8");

  assert.match(renderer, /<SceneView scene=\{model\.scene\} \/>/);
  assert.doesNotMatch(renderer, /SceneView[^\n]*(kind|mode|style)=/);
  assert.match(scene, /interface Props \{ scene: RyeOsSceneModel \}/);
  assert.doesNotMatch(scene, /namespace_atlas|paper_3d|flat_2d|AmbientOptions/);
  assert.doesNotMatch(scene, /set_atlas_|\?\? "project"/);
  assert.match(scene, /scene\.camera\.fov_degrees/);
  assert.match(field, /new FieldCanvasController\(canvas, \{/);
  assert.doesNotMatch(field, /canCompareEntity/);
  assert.doesNotMatch(field, /mountField|replaceChildren|innerHTML/);
});
