import { code, el, textEl } from "/ui/assets/studio_components_primitives.js";

export function studioWorkspace(vm, motion, dispatchUi) {
  const main = el("main", "studio-workspace");
  if (!vm?.root) {
    main.append(textEl("p", "No workspace loaded."));
    return main;
  }
  if (vm.is_home) {
    main.classList.add("home-space");
    return main;
  }
  main.append(layoutNode(vm.root, dispatchUi, motion));
  return main;
}

export function tileIdsForNode(node, ids = []) {
  if (!node) return ids;
  if (node.type === "split") {
    tileIdsForNode(node.first, ids);
    tileIdsForNode(node.second, ids);
  } else if (node.tile_id) {
    ids.push(node.tile_id);
  }
  return ids;
}

function layoutNode(node, dispatchUi, motion = []) {
  if (node.type === "split") {
    const wrap = el("div", `studio-split ${node.axis}`);
    wrap.style.setProperty("--split-ratio", `${Math.round((node.ratio || 0.5) * 100)}%`);
    wrap.append(layoutNode(node.first, dispatchUi, motion), layoutNode(node.second, dispatchUi, motion));
    return wrap;
  }
  const tile = el("section", `studio-tile${node.focused ? " focused" : ""}`);
  tile.dataset.tileId = node.tile_id || "";
  const motionName = motionForTile(node, motion);
  if (motionName) tile.dataset.motion = motionName;
  tile.addEventListener("mousedown", (event) => {
    if (event.target.closest("button,input,select,textarea,a")) return;
    if (node.focused) return;
    dispatchUi({ type: "focus_changed", target: node.tile_id || null });
  });
  tile.append(view(node.view || {}, node.tile_id || "", dispatchUi));
  return tile;
}

function motionForTile(node, motion) {
  const tileId = node.tile_id || "";
  if (!tileId) return "";
  if ((motion || []).some((event) => event.type === "tile_split" && event.new_tile_id === tileId)) return "split-enter";
  if ((motion || []).some((event) => event.type === "tile_enter" && event.tile_id === tileId)) return "enter";
  if ((motion || []).some((event) => event.type === "focus_changed" && event.tile_id === tileId)) return "focus";
  return "";
}

function view(viewVm, tileId, dispatchUi) {
  const body = el("div", "studio-tile-body");
  const listKind = listKindForView(viewVm);
  switch (viewVm.type) {
    case "map":
      body.append(sceneMap(viewVm.scene, dispatchUi));
      break;
    case "overview":
      body.append(metrics(viewVm.metrics || [], dispatchUi));
      for (const block of viewVm.sections || []) body.append(sectionBlock(block, dispatchUi));
      break;
    case "items":
      body.append(itemsToolbar(viewVm.filters || {}, tileId, dispatchUi), rows(viewVm.rows || [], tileId, listKind, dispatchUi));
      break;
    case "files":
      body.append(fileBrowser(viewVm, tileId, dispatchUi));
      break;
    case "thread_list":
      body.append(listHeader("threads", "runs and events"), rows(viewVm.rows || [], tileId, listKind, dispatchUi));
      break;
    case "thread":
      body.append(textEl("h2", viewVm.thread_id ? `Thread ${viewVm.thread_id}` : "New Thread"));
      for (const block of viewVm.sections || []) body.append(sectionBlock(block, dispatchUi));
      for (const block of viewVm.code_blocks || []) body.append(textEl("h3", block.label), code(block.content));
      break;
    case "rows":
      body.append(listHeader(viewVm.title, (viewVm.columns || []).join(" · ")), rows(viewVm.rows || [], tileId, listKind, dispatchUi));
      break;
    case "gc":
      body.append(textEl("h2", viewVm.running ? "GC running" : "GC idle"), code(JSON.stringify(viewVm.recent_events || [], null, 2)));
      break;
    case "inspector":
      body.append(inspector(viewVm));
      break;
    case "placeholder":
      body.append(textEl("h2", viewVm.title), textEl("p", viewVm.message));
      break;
    default:
      body.append(textEl("p", `Unknown view: ${viewVm.type || "missing"}`));
  }
  return body;
}

function listKindForView(viewVm) {
  switch (viewVm?.type) {
    case "items":
      return "items";
    case "files":
      return "files";
    case "thread_list":
      return "threads";
    default:
      return "rows";
  }
}

function listHeader(title, detail) {
  const header = el("div", "studio-list-header");
  header.append(textEl("strong", title || "list"), textEl("span", detail || ""));
  return header;
}

function fileBrowser(viewVm, tileId, dispatchUi) {
  const wrap = el("section", "studio-file-browser");
  const nav = el("div", "studio-file-nav");
  nav.append(
    textEl("strong", viewVm.root || "files"),
    textEl("span", `/${viewVm.path || ""}`),
  );

  const panes = el("div", "studio-file-panes");
  const listPane = el("div", "studio-file-list-pane");
  const fileRows = rows(viewVm.rows || [], tileId, "files", dispatchUi);
  fileRows.dataset.scrollKey = `file-list-${tileId}`;
  listPane.append(fileRows);
  panes.append(listPane, filePreview(viewVm.preview, viewVm.rows || []));
  wrap.append(nav, panes);
  return wrap;
}

function filePreview(preview, rowsVm) {
  const selected = rowsVm.find((row) => row.selected) || rowsVm[0] || null;
  const data = preview || {
    title: selected?.primary || "No file selected",
    subtitle: selected?.secondary || "",
    kind: selected?.kind || "empty",
    content: null,
    hint: "Select a file or directory.",
  };
  const pane = el("aside", `studio-file-preview ${data.kind || "unknown"}`);
  const head = el("div", "studio-file-preview-head");
  head.append(
    textEl("strong", data.title || "preview"),
    textEl("span", data.subtitle || ""),
    textEl("small", previewMeta(data)),
  );
  pane.append(head);
  if (data.content) {
    const pre = code(data.content);
    pre.classList.add("studio-file-preview-code");
    pane.append(pre);
  } else {
    const empty = el("div", "studio-file-preview-empty");
    empty.append(textEl("strong", data.kind === "directory" ? "directory" : "preview"), textEl("span", data.hint || "No preview loaded."));
    pane.append(empty);
  }
  return pane;
}

function previewMeta(preview) {
  const parts = [preview.kind || "file"];
  if (preview.size !== undefined && preview.size !== null) parts.push(formatBytes(preview.size));
  if (preview.truncated) parts.push("truncated");
  return parts.join(" · ");
}

function formatBytes(value) {
  if (value < 1024) return `${value}b`;
  if (value < 1024 * 1024) return `${(value / 1024).toFixed(1)}kb`;
  return `${(value / (1024 * 1024)).toFixed(1)}mb`;
}

function metrics(items, dispatchUi) {
  const wrap = el("div", "studio-metrics");
  for (const metric of items) {
    const card = el("button", `studio-metric ${metric.tone || "neutral"}`);
    card.type = "button";
    card.disabled = !metric.action;
    card.append(textEl("span", metric.label), textEl("strong", metric.value), textEl("small", metric.hint || ""));
    if (metric.action) card.addEventListener("click", () => dispatchUi({ type: "activate", action: metric.action }));
    wrap.append(card);
  }
  return wrap;
}

function sceneMap(scene, dispatchUi) {
  const wrap = el("section", "studio-scene");
  if (scene?.atlas) return atlasMap(scene.atlas, dispatchUi, wrap);
  const header = el("div", "studio-scene-header");
  header.append(textEl("h2", "Graph"), textEl("p", "Local node, remotes, and workspace topology."));
  const stage = el("div", "studio-scene-stage");
  for (const object of scene?.objects || []) {
    const node = el("button", `studio-scene-node ${object.kind} ${object.tone || "neutral"}`);
    node.type = "button";
    node.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    node.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    node.style.setProperty("--node-color", object.color || "#fabd2f");
    node.disabled = !object.action;
    node.append(textEl("strong", object.label || object.id), textEl("span", object.kind || "object"));
    if (object.action) node.addEventListener("click", () => dispatchUi({ type: "activate", action: object.action }));
    stage.append(node);
  }
  wrap.append(header, stage);
  return wrap;
}

function atlasMap(atlas, dispatchUi, wrap = el("section", "studio-scene")) {
  wrap.classList.add("studio-atlas-map");
  const atlasUi = atlas.ui || {};
  const visibleLayers = new Set(atlasUi.visible_layers || ["directive", "tool", "knowledge", "config", "other"]);
  const activeLens = atlasUi.active_lens || "none";
  for (const kind of ["directive", "tool", "knowledge", "config", "other"]) {
    if (!visibleLayers.has(kind)) wrap.classList.add(`hide-${kind}`);
  }
  if (activeLens === "knowledge") wrap.classList.add("lens-knowledge");
  const header = el("div", "studio-scene-header");
  const title = el("div", "studio-atlas-title");
  const stackCount = (atlas.nodes || []).filter((node) => (node.stack || []).length).length;
  title.append(textEl("h2", "Namespace Atlas"), textEl("p", `${stackCount} stacks · ${(atlas.regions || []).length} capability regions`));
  const controls = el("div", "studio-atlas-controls");
  for (const [kind, label] of [["directive", "Directives"], ["tool", "Tools"], ["knowledge", "Knowledge"], ["config", "Config"]]) {
    const pressed = visibleLayers.has(kind);
    const button = el("button", `studio-atlas-control ${kind}`);
    button.type = "button";
    button.textContent = label;
    button.setAttribute("aria-pressed", pressed ? "true" : "false");
    button.addEventListener("click", () => {
      dispatchUi({ type: "set_atlas_layer_visible", kind, visible: !pressed });
    });
    controls.append(button);
  }
  const knowledgeLens = el("button", "studio-atlas-control lens");
  const knowledgeLensEnabled = activeLens === "knowledge";
  knowledgeLens.type = "button";
  knowledgeLens.textContent = "Knowledge lens";
  knowledgeLens.setAttribute("aria-pressed", knowledgeLensEnabled ? "true" : "false");
  knowledgeLens.addEventListener("click", () => {
    dispatchUi({ type: "set_atlas_lens", lens: knowledgeLensEnabled ? "none" : "knowledge" });
  });
  controls.append(knowledgeLens);
  const legend = el("div", "studio-atlas-legend");
  for (const [kind, label] of [["directive", "Directive"], ["tool", "Tool"], ["knowledge", "Knowledge"], ["config", "Config"]]) {
    const item = textEl("span", label);
    item.className = `studio-atlas-legend-item ${kind}`;
    legend.append(item);
  }
  header.append(title, controls, legend);

  const stage = el("div", "studio-scene-stage studio-atlas-stage");
  const bounds = atlas.bounds || {};
  const xMin = bounds.x_min ?? -1;
  const xMax = bounds.x_max ?? 1;
  const zMin = bounds.z_min ?? -1;
  const zMax = bounds.z_max ?? 1;
  const xSpan = Math.max(1, Math.abs(xMax - xMin));
  const zSpan = Math.max(1, Math.abs(zMax - zMin));
  const position = (node) => {
    const p = node.position || [0, 0, 0];
    return {
      left: 8 + ((p[0] - xMin) / xSpan) * 84,
      top: 8 + ((p[2] - zMin) / zSpan) * 84,
    };
  };

  for (const region of atlas.regions || []) {
    const regionEl = el("div", "studio-atlas-region");
    regionEl.title = region.capability || "capability";
    const angle = ((region.angle_start || 0) + (region.angle_end || 0)) / 2;
    const radius = region.radius_max || region.radius_min || 0;
    const marker = position({ position: [Math.cos(angle) * radius, 0, Math.sin(angle) * radius] });
    regionEl.style.left = `${marker.left}%`;
    regionEl.style.top = `${marker.top}%`;
    regionEl.textContent = "cap";
    stage.append(regionEl);
  }

  const nodes = atlas.nodes || [];
  const nodeByKey = new Map(nodes.map((node) => [node.namespace_key || "", node]));
  const nodeById = new Map(nodes.map((node) => [node.id || "", node]));
  for (const node of nodes) {
    if (!node.path?.length) continue;
    const parent = nodeByKey.get(node.path.slice(0, -1).join("/"));
    if (!parent) continue;
    const a = position(parent);
    const b = position(node);
    const dx = b.left - a.left;
    const dy = b.top - a.top;
    const len = Math.hypot(dx, dy);
    const branch = el("div", `studio-atlas-branch${(node.stack || []).length ? " terminal" : ""}`);
    branch.style.left = `${a.left}%`;
    branch.style.top = `${a.top}%`;
    branch.style.width = `${len}%`;
    branch.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
    stage.append(branch);
  }

  for (const link of atlas.links || []) {
    const from = nodeById.get(link.from);
    const to = nodeById.get(link.to);
    if (!from || !to) continue;
    const a = position(from);
    const b = position(to);
    const dx = b.left - a.left;
    const dy = b.top - a.top;
    const len = Math.hypot(dx, dy);
    const contextLink = el("div", `studio-atlas-context-link ${link.kind || "context"}`);
    contextLink.style.left = `${a.left}%`;
    contextLink.style.top = `${a.top}%`;
    contextLink.style.width = `${len}%`;
    contextLink.style.transform = `rotate(${Math.atan2(dy, dx)}rad)`;
    stage.append(contextLink);
  }

  for (const node of nodes) {
    const stack = node.stack || [];
    const visibleStack = stack.filter((item) => atlasItemVisible(atlas, item));
    if (!visibleStack.length && !node.state?.selected && !node.state?.highlighted) continue;
    const p = position(node);
    const nodeEl = el("div", `studio-atlas-stack${node.state?.selected ? " selected" : ""}${node.state?.highlighted ? " highlighted" : ""}${node.state?.dimmed ? " dimmed" : ""}${visibleStack.length ? "" : " anchor-only"}`);
    nodeEl.style.left = `${p.left}%`;
    nodeEl.style.top = `${p.top}%`;
    nodeEl.title = node.namespace_key || node.label || "namespace";
    for (const item of visibleStack) {
      const button = el("button", `studio-atlas-layer ${item.kind || "other"}`);
      button.classList.add(`scope-${item.scope || "unknown"}`);
      button.type = "button";
      button.title = item.canonical_ref || item.label || node.namespace_key;
      button.textContent = atlasGlyph(item.kind);
      button.addEventListener("click", (event) => {
        event.stopPropagation();
        if (!item.canonical_ref) return;
        dispatchUi({ type: "activate", action: { type: "inspect_item", canonical_ref: item.canonical_ref } });
      });
      nodeEl.append(button);
    }
    const label = textEl("span", node.label || node.namespace_key || "node");
    label.className = "studio-atlas-stack-label";
    nodeEl.append(label);
    stage.append(nodeEl);
  }

  const directives = atlasDirectiveList(nodes, dispatchUi);
  if (directives) {
    const atlasBody = el("div", "studio-atlas-body");
    atlasBody.append(stage, directives);
    wrap.append(header, atlasBody);
  } else {
    wrap.append(header, stage);
  }
  return wrap;
}

function atlasItemVisible(atlas, item) {
  const atlasUi = atlas?.ui || {};
  const visibleLayers = new Set(atlasUi.visible_layers || ["directive", "tool", "knowledge", "config", "other"]);
  const kind = item.kind || "other";
  if (!visibleLayers.has(kind)) return false;
  if ((atlasUi.active_lens || "none") === "knowledge") return kind === "knowledge";
  return true;
}

function atlasDirectiveList(nodes, dispatchUi) {
  const directives = [];
  for (const node of nodes || []) {
    for (const item of node.stack || []) {
      if (item.kind === "directive") directives.push({ node, item });
    }
  }
  if (!directives.length) return null;
  directives.sort((a, b) => (a.item.canonical_ref || "").localeCompare(b.item.canonical_ref || ""));
  const aside = el("aside", "studio-atlas-directives");
  aside.append(textEl("strong", "Directives"), textEl("span", "select to reveal context"));
  const list = el("div", "studio-atlas-directive-list");
  for (const { node, item } of directives.slice(0, 120)) {
    const button = el("button", `studio-atlas-directive${node.state?.selected ? " selected" : ""}${node.state?.highlighted ? " highlighted" : ""}`);
    button.type = "button";
    button.append(textEl("strong", item.label || node.label || item.canonical_ref), textEl("small", item.canonical_ref || node.namespace_key));
    button.addEventListener("click", () => {
      if (!item.canonical_ref) return;
      dispatchUi({ type: "activate", action: { type: "inspect_item", canonical_ref: item.canonical_ref } });
    });
    list.append(button);
  }
  aside.append(list);
  return aside;
}

function atlasGlyph(kind) {
  switch (kind) {
    case "directive": return "◆";
    case "tool": return "⚙";
    case "knowledge": return "◈";
    case "config": return "◇";
    default: return "●";
  }
}

function itemsToolbar(filters, tileId, dispatchUi) {
  const targetTile = filters.tile_id || tileId || "";
  const toolbar = el("div", "studio-toolbar");
  const crumb = el("button", "studio-folder-crumb");
  crumb.type = "button";
  crumb.textContent = filters.items_path ? `items / ${filters.items_path}` : "items /";
  crumb.title = "Back to item root";
  crumb.addEventListener("click", () => dispatchUi({ type: "activate", action: { type: "enter_item_folder", tile_id: targetTile, path: "" } }));
  const query = document.createElement("input");
  query.type = "search";
  query.placeholder = "Filter items";
  query.autocomplete = "off";
  query.setAttribute("data-focus-key", `items-query-${targetTile}`);
  query.value = filters.items_query || "";
  query.addEventListener("input", () => dispatchUi({ type: "set_filter", tile_id: targetTile, field: "items_query", value: query.value }));
  const kind = document.createElement("select");
  kind.setAttribute("data-focus-key", `items-kind-${targetTile}`);
  for (const { value, label } of filters.item_kind_options || []) {
    const option = document.createElement("option");
    option.value = value;
    option.textContent = label;
    option.selected = value === (filters.items_kind || "");
    kind.append(option);
  }
  kind.addEventListener("change", () => dispatchUi({ type: "set_filter", tile_id: targetTile, field: "items_kind", value: kind.value }));
  toolbar.append(crumb, query, kind);
  return toolbar;
}

function sectionBlock(block, dispatchUi) {
  const section = el("section", "studio-section");
  section.append(textEl("h2", block.title));
  const dl = el("dl");
  for (const [key, value] of block.rows || []) dl.append(textEl("dt", key), textEl("dd", value));
  section.append(dl);
  if (block.action && dispatchUi) section.addEventListener("click", () => dispatchUi({ type: "activate", action: block.action }));
  return section;
}

function rows(items, tileId, kind, dispatchUi) {
  const list = el("div", `studio-rows lf ${kind || "rows"}`);
  items.forEach((item, index) => {
    const row = el("button", `studio-row ${item.tone || "neutral"}${item.selected ? " selected" : ""}`);
    row.type = "button";
    row.dataset.rowIndex = String(index);
    row.disabled = !item.action;
    row.append(
      textEl("span", rowGlyph(item, kind), "studio-row-glyph"),
      textEl("strong", item.primary),
      textEl("span", item.secondary || ""),
      textEl("small", item.meta || ""),
    );
    if (item.action) row.addEventListener("click", () => dispatchUi({ type: "activate", action: item.action }));
    list.append(row);
  });
  return list;
}

function rowGlyph(item, kind) {
  const rowKind = item.kind || "";
  if (kind === "files") return rowKind === "directory" ? "▸" : "·";
  if (kind === "items") {
    if (rowKind === "folder_up") return "↩";
    if (rowKind === "folder") return "▸";
    if (rowKind === "tool") return "⚙";
    if (rowKind === "directive") return "◆";
    if (rowKind === "knowledge") return "◈";
    if (rowKind === "config") return "◇";
    return "•";
  }
  if (kind === "threads") return "▶";
  return "•";
}

function inspector(vm) {
  const wrap = el("div", "studio-inspector-view");
  wrap.append(textEl("h2", vm.title));
  if (vm.subtitle) wrap.append(textEl("p", vm.subtitle));
  if (vm.empty) wrap.append(textEl("p", vm.empty_message || "Select an object to inspect it."));
  for (const section of vm.sections || []) wrap.append(sectionBlock(section, null));
  for (const block of vm.code_blocks || []) wrap.append(textEl("h3", block.label), code(block.content));
  return wrap;
}
