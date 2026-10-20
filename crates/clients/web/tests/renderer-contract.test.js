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

test("bounded row evidence is rendered through the shared detail primitive", async () => {
  const details = await readFile(new URL("browser/components/RowDetails.svelte", root), "utf8");
  const renderer = await readFile(new URL("browser/views/ViewRenderer.svelte", root), "utf8");
  const field = await readFile(new URL("browser/views/FieldView.svelte", root), "utf8");

  assert.match(details, /<dl class="row-details"/);
  assert.doesNotMatch(details, /innerHTML|{@html/);
  assert.match(renderer, /type: "toggle_view_item_expansion"/);
  assert.match(renderer, /model\.entry_details\?\.\[index\]/);
  assert.match(renderer, /details=\{row\.detail \?\? \[\]\}/);
  assert.match(renderer, /class="table-record-line" role="row"/);
  assert.match(renderer, /class="table-action-cell" role="cell"/);
  assert.doesNotMatch(renderer, /<button[^>]*role="row"/);
  assert.match(field, /details=\{selected\.detail\}/);
});

test("unavailable rows stay discoverable with explicit disabled semantics", async () => {
  const renderer = await readFile(new URL("browser/views/ViewRenderer.svelte", root), "utf8");
  const projection = await readFile(new URL("../base/src/ui/local_projection.rs", root), "utf8");
  const reducer = await readFile(new URL("../base/src/ui/reducer/mod.rs", root), "utf8");

  assert.match(renderer, /data-kind=\{row\.kind \?\? undefined\}/);
  assert.match(renderer, /aria-disabled=\{row\.kind === "unavailable"\}/);
  assert.match(renderer, /<button[^>]*\sdisabled=\{row\.kind === "unavailable"\}[^>]*onclick=\{\(\) => select\(row\.id\)\}/);
  // Enabled rows submit the exact mount and semantic row, never a cached intent.
  assert.match(renderer, /type: "choose_view_item",\s*instance_key: instanceKey,\s*item_id: itemId,\s*activate: true/);
  assert.match(projection, /intent: record\.intent\.filter\(\|_\| enabled\)/);
  assert.match(reducer, /view_pointer_item\(self, &instance_key, &item_id\)/);
  assert.match(reducer, /if activate && let Some\(intent\) = intent/);
});

test("empty tables describe an observed page rather than asserting absence", async () => {
  const renderer = await readFile(new URL("browser/views/ViewRenderer.svelte", root), "utf8");
  const tile = await readFile(new URL("browser/layout/TileFrame.svelte", root), "utf8");
  assert.match(renderer, /model\.total_rows === 0/);
  assert.match(renderer, /No rows in this page\./);
  assert.match(tile, /model\.supplement\?\.frame_label \|\| model\.group_label \|\| model\.title/);
});

test("table coverage is rendered independently of whether rows are present", async () => {
  const renderer = await readFile(new URL("browser/views/ViewRenderer.svelte", root), "utf8");
  assert.match(renderer, /class="table-coverage"/);
  assert.match(renderer, /tableCoverage\.state/);
  assert.match(renderer, /tableCoverage\.reason/);
});

test("shell keeps semantic context in Rust while narrow navigation stays reachable", async () => {
  const shell = await readFile(new URL("browser/app/RyeOs.svelte", root), "utf8");
  const system = await readFile(new URL("browser/app/SystemBar.svelte", root), "utf8");
  const navigation = await readFile(new URL("browser/app/Navigation.svelte", root), "utf8");
  const status = await readFile(new URL("browser/app/StatusBar.svelte", root), "utf8");
  const tile = await readFile(new URL("browser/layout/TileFrame.svelte", root), "utf8");

  assert.match(shell, /navigationOpen/);
  assert.match(shell, /navigationOpener/);
  assert.match(shell, /panel\?\.contains\(active\)/);
  assert.match(system, /aria-controls="ryeos-navigation"/);
  assert.doesNotMatch(system, /session\.project_path/);
  assert.match(navigation, /querySelector<HTMLElement>\("button:not\(\[disabled\]\)"\)/);
  assert.match(navigation, /event\.key === "Escape"/);
  assert.match(navigation, /event\.key !== "Tab"/);
  assert.match(status, /model\.segments/);
  assert.doesNotMatch(status, /project_path|thread_count|input_tokens/);
  assert.match(tile, /data-keyboard-focus=\{model\.focused/);
  const dock = await readFile(new URL("browser/layout/DockSlot.svelte", root), "utf8");
  assert.match(dock, /data-keyboard-focus=\{model\.focused/);
});

test("canonical mount identities bound renderer-local DOM lifetimes", async () => {
  const viewSet = await readFile(new URL("browser/layout/ViewSet.svelte", root), "utf8");
  const layout = await readFile(new URL("browser/layout/LayoutNode.svelte", root), "utf8");

  assert.match(viewSet, /\{#key model\?\.view_set_id\}[\s\S]*<LayoutNode/);
  assert.match(layout, /\{#key model\.instance_key\}[\s\S]*<TileFrame/);
  for (const edge of ["top", "left", "right", "bottom"]) {
    assert.match(viewSet, new RegExp(`\\{#key docks\\.${edge}\\.instance_key\\}<DockSlot`));
  }

  // Dock keys remain outside the center's view-set key. Switching Surfaces
  // remounts the center while an unchanged shared dock keeps its DOM lifetime.
  const centerKey = viewSet.indexOf("{#key model?.view_set_id}");
  assert.ok(centerKey > viewSet.indexOf("{#key docks.top.instance_key}"));
  assert.ok(viewSet.indexOf("{#key docks.bottom.instance_key}") > viewSet.indexOf("{/key}", centerKey));
});
