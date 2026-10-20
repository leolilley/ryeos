import assert from "node:assert/strict";
import { readFile } from "node:fs/promises";
import test from "node:test";

const read = (path) => readFile(new URL(path, import.meta.url), "utf8");

test("prepared-document handoff stays a compact token-styled platform frame", async () => {
  const styles = await read("../browser/styles/components.css");
  assert.match(styles, /\.ryeos-document-handoff\s*\{[^}]*width: min\(440px, calc\(100vw - 24px\)\)/);
  assert.match(styles, /\.ryeos-document-handoff\s*\{[^}]*border-top: 2px solid var\(--accent-bright\)/);
  assert.match(styles, /\.ryeos-document-handoff \[hidden\]\s*\{ display: none; \}/);
  assert.match(styles, /\.ryeos-document-handoff :is\(a, button\):focus-visible/);
});

test("opening declarations use one signed effect and explicit placement without an alias", async () => {
  for (const path of ["surfaces/list", "view-sets/library", "projects/list"]) {
    const view = await read(`../../../../bundles/ryeos-ui/.ai/views/ryeos/${path}.yaml`);
    assert.match(view, /presentation_operation: surface_opening/);
    assert.match(view, /placement:\s*\{\s*kind: new_surface\s*\}/);
    assert.doesNotMatch(view, /presentation_operation: open_surface/);
  }
});

test("collection import declares captured placement rather than a renderer purpose", async () => {
  const catalogue = await read("../../../../bundles/ryeos-ui/.ai/views/ryeos/surfaces/list.yaml");
  const library = await read("../../../../bundles/ryeos-ui/.ai/views/ryeos/view-sets/library.yaml");
  assert.match(catalogue, /id: import-tabs[\s\S]*?placement: "\{record\.opening_placement\}"/);
  assert.match(library, /id: import-reusable-surface-tabs[\s\S]*?placement: "\{record\.opening_placement\}"/);
  assert.match(catalogue, /id: open-composed-surface[\s\S]*?selection: "\{record\.opening_selection\}"/);
  for (const view of [catalogue, library]) {
    assert.doesNotMatch(view, /presentation_operation: import_surface_tabs/);
  }
});

test("particular Surface duplicate is an explicit action through the existing signed resume owner", async () => {
  const library = await read("../../../../bundles/ryeos-ui/.ai/views/ryeos/view-sets/library.yaml");
  const action = library.split("  - id: duplicate-particular-surface\n")[1]?.split("  - id: ")[0];
  assert.ok(action, "independent-copy action is declared");
  assert.match(action, /presentation_operation: resume_particular_surface/);
  assert.match(action, /ref: service:ui\/ryeos-ui\/surfaces\/resume/);
  for (const field of ["id", "expected_revision", "view_set_expected_revision", "creation_request_id"]) {
    assert.ok(action.includes(`${field}: "{record.${field}}"`), field);
  }
  assert.doesNotMatch(action, /kind: composed|surface_opening|plane: ui/);
  assert.match(library, /collection: particular_surfaces[\s\S]*?activate: duplicate-particular-surface/);
});

test("ordinary table consumes Core hierarchy only in its first column", async () => {
  const view = await read("../browser/views/ViewRenderer.svelte");
  assert.match(view, /index === 0 && row\.hierarchy/);
  assert.match(view, /<TableHierarchy hierarchy=\{row\.hierarchy\}/);
  const hierarchy = await read("../browser/views/TableHierarchy.svelte");
  for (const field of ["ancestor_continues", "is_last", "has_children", "collapsed"]) {
    assert.ok(hierarchy.includes("hierarchy." + field), field);
  }
  assert.doesNotMatch(hierarchy, /dispatch|fetch\(|onMount/);
});

test("generic thread history activates execution inspection without conversation retargeting", async () => {
  for (const path of ["threads/history", "threads/list", "node/threads/history"]) {
    const view = await read(`../../../../bundles/ryeos-ui/.ai/views/ryeos/${path}.yaml`);
    const action = view.split("  - id: watch\n")[1]?.split("  - id: ")[0];
    assert.ok(action, path);
    assert.match(action, /facet: selection/);
    assert.match(action, /thread_id: "\{record\.thread_id\}"/);
    assert.match(action, /open_view: view:ryeos\/threads\/detail/);
    assert.match(action, /drill: true/);
    assert.doesNotMatch(action, /input\.route|thread\/transcript|chain_root:/);
  }
});

test("tree branch disclosure addresses exact mount and row independently of activation", async () => {
  const view = await read("../browser/views/ViewRenderer.svelte");
  assert.match(view, /type: "set_view_tree_item_collapsed",\s*instance_key: instanceKey,\s*item_id: itemId/);
  assert.match(view, /onToggle=\{\(\) => foldTree\(row.id, row.hierarchy!\.collapsed\)/);
  const hierarchy = await read("../browser/views/TableHierarchy.svelte");
  assert.match(hierarchy, /aria-expanded=\{!hierarchy.collapsed\}/);
  assert.match(hierarchy, /onclick=\{onToggle\}/);
  assert.match(hierarchy, /\{#if hierarchy.has_children\}/);
  assert.doesNotMatch(hierarchy, /\$state|set_tree_row_collapsed/);
});

test("unavailable rows use native disabled behavior", async () => {
  const view = await read("../browser/views/ViewRenderer.svelte");
  assert.match(view, /\sdisabled=\{row\.kind === "unavailable"\}/);
});

test("read-only detail rows use ordinary definition-list rendering", async () => {
  const view = await read("../browser/views/ViewRenderer.svelte");
  assert.match(view, /row\.kind === "detail"[\s\S]*?<RowDetails details=\{row\.detail/);
});

test("session details remain an ordinary Rust projection in each shared presentation", async () => {
  const view = await read("../../../../bundles/ryeos-ui/.ai/views/ryeos/ui/session-details.yaml");
  assert.match(view, /local_projection: session_details/);
  assert.doesNotMatch(view, /sources:|affordances:/);
  for (const name of ["presentation", "terminal-presentation", "atlas-presentation"]) {
    const config = await read(`../../../../bundles/ryeos-ui/.ai/config/ryeos/ui/${name}.yaml`);
    assert.match(config, /id: session, title: Session details, view: "view:ryeos\/ui\/session-details"/);
  }
  const projection = await read("../../base/src/ui/local_projection.rs");
  assert.match(projection, /fn session_details\(/);
  assert.match(projection, /binding_attachment_for_instance\(instance\)/);
  assert.match(projection, /user_principal_id/);
  const status = await read("../../../daemon/ryeos-app/src/state.rs");
  assert.match(status, /node_id: self\.identity\.fingerprint\(\)\.to_owned\(\)/);
});
