import { el, textEl } from "/ui/assets/ryeos_components_primitives.js";

const atlasViewport = {
  panX: 0,
  panY: 0,
  zoom: 1,
};

const utf8Encoder = new TextEncoder();

export function ryeosWorkspace(vm, ambient, motion, dispatchUi) {
  const main = el("main", "ryeos-workspace");
  // There is no "home" mode. The plane (docks incl. the real bottom input
  // slot) renders in every state; the only branch is backdrop-vs-tiles in
  // the center, handled inside workspacePlane.
  if (!vm) {
    main.append(textEl("p", "No workspace loaded."));
    return main;
  }
  if (vm.center_is_empty) main.classList.add("empty-center");
  const underlay = vm.root && vm.backdrop && ambient?.show_background !== false
    && Number(ambient?.opacity || 1) > 0
    && Number(ambient?.opacity || 1) < 1;
  if (underlay) main.classList.add("ambient-underlay");
  main.append(workspacePlane(vm, ambient, dispatchUi, motion));
  return main;
}

function workspacePlane(vm, ambient, dispatchUi, motion) {
  const plane = el("section", "ryeos-workspace-plane");
  const docks = vm.docks || {};
  const left = dockTile(docks.left, dispatchUi);
  const right = dockTile(docks.right, dispatchUi);
  const top = dockTile(docks.top, dispatchUi);
  const bottom = dockTile(docks.bottom, dispatchUi);

  if (left) {
    plane.classList.add("has-left-dock");
    plane.style.setProperty("--ryeos-dock-left", `${Math.max(18, left.__dockSize || 28)}ch`);
  }
  if (right) {
    plane.classList.add("has-right-dock");
    plane.style.setProperty("--ryeos-dock-right", `${Math.max(18, right.__dockSize || 34)}ch`);
  }
  if (top) {
    plane.classList.add("has-top-dock");
    plane.style.setProperty("--ryeos-dock-top", `${Math.max(3, top.__dockSize || 4) * 1.35}rem`);
  }
  if (bottom) {
    plane.classList.add("has-bottom-dock");
    plane.style.setProperty("--ryeos-dock-bottom", `${Math.max(3, bottom.__dockSize || 4) * 1.35}rem`);
  }

  if (left) plane.append(left);
  if (right) plane.append(right);
  if (top) plane.append(top);

  const stack = el("section", "ryeos-workspace-stack");
  const underlay = vm.root && vm.backdrop && ambient?.show_background !== false
    && Number(ambient?.opacity || 1) > 0
    && Number(ambient?.opacity || 1) < 1;
  if (underlay) {
    const backdrop = backdropScene(vm.backdrop, dispatchUi);
    backdrop.classList.add("underlay");
    backdrop.style.opacity = String(Number(ambient.opacity));
    stack.append(backdrop);
  }
  if (vm.root) {
    stack.append(layoutNode(vm.root, dispatchUi, motion));
  } else if (vm.backdrop) {
    // Empty center: the backdrop is content — drawn through the same
    // generic scene path. The background is a scene, never a renderer enum.
    stack.append(backdropScene(vm.backdrop, dispatchUi));
  }
  plane.append(stack);

  if (bottom) plane.append(bottom);
  return plane;
}

function dockTile(dockVm, dispatchUi) {
  if (!dockVm) return null;
  const edge = dockVm.edge || "bottom";
  const tile = el("aside", `ryeos-dock-tile ${edge}`);
  tile.__dockSize = dockVm.size;
  const chrome = el("header", "ryeos-dock-chrome");
  chrome.append(textEl("strong", dockVm.title || edge), textEl("small", edge));
  tile.append(chrome, dockView(dockVm, dispatchUi));
  return tile;
}

// A view instance that declares `input` renders as the prompt (input is an
// orthogonal capability, not a dock-content variant). Otherwise the bound
// widget renders.
function dockView(instanceVm, dispatchUi) {
  const body = el("div", "ryeos-dock-body");
  if (instanceVm.input) {
    body.append(inputDock(instanceVm.input, dispatchUi));
  } else {
    body.append(view(instanceVm.view || {}, dispatchUi));
  }
  return body;
}

function inputDock(inputVm, dispatchUi) {
  const wrap = el("section", "ryeos-input-dock");
  const meta = el("div", "ryeos-input-meta");
  meta.append(
    textEl("span", "→", "ryeos-input-arrow"),
    textEl("strong", inputVm.route_label || "target: ryeos"),
    textEl("small", inputVm.hint || ""),
  );

  const row = el("div", "ryeos-input-row");
  const prompt = textEl("span", "$", "ryeos-input-prompt");
  const input = document.createElement("textarea");
  input.rows = 1;
  input.value = inputVm.text || "";
  input.placeholder = inputVm.placeholder || "type RyeOS input…";
  input.spellcheck = false;
  input.autocomplete = "off";
  input.setAttribute("data-focus-key", "ryeos-input-dock");
  input.addEventListener("input", () => {
    dispatchUi({ type: "set_input_text", text: input.value, cursor: byteCursor(input.value, input.selectionStart || 0) });
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.shiftKey) {
      event.preventDefault();
      dispatchUi({ type: "submit_input" });
    } else if (event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey) {
      event.preventDefault();
      dispatchUi({ type: "complete_input" });
    }
  });
  const submit = el("button", "ryeos-input-submit");
  submit.type = "button";
  submit.disabled = !inputVm.submit_enabled;
  submit.textContent = "send";
  submit.addEventListener("click", () => dispatchUi({ type: "submit_input" }));
  row.append(prompt, input, submit);
  wrap.append(meta, row);

  // Completion suggestions from the input's `completion` source.
  const suggestions = inputVm.completion || [];
  if (suggestions.length) {
    const completion = el("div", "ryeos-input-completion");
    for (const suggestion of suggestions) {
      completion.append(textEl("small", suggestion, "ryeos-input-suggestion"));
    }
    wrap.append(completion);
  }
  return wrap;
}

function byteCursor(value, codeUnitCursor) {
  return utf8Encoder.encode(value.slice(0, codeUnitCursor)).length;
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
    const wrap = el("div", `ryeos-split ${node.axis}`);
    wrap.style.setProperty("--split-ratio", `${Math.round((node.ratio || 0.5) * 100)}%`);
    wrap.append(layoutNode(node.first, dispatchUi, motion), layoutNode(node.second, dispatchUi, motion));
    return wrap;
  }
  const tile = el("section", `ryeos-tile${node.focused ? " focused" : ""}`);
  if (node.chrome_hidden) tile.classList.add("chrome-hidden");
  tile.dataset.tileId = node.tile_id || "";
  const motionName = motionForTile(node, motion);
  if (motionName) tile.dataset.motion = motionName;
  tile.addEventListener("mousedown", (event) => {
    if (event.target.closest("button,input,select,textarea,a")) return;
    if (node.focused) return;
    dispatchUi({ type: "focus_changed", target: node.tile_id || null });
  });
  const chrome = el("header", "ryeos-tile-chrome");
  chrome.append(textEl("strong", node.title || "tile"), textEl("small", node.tile_id || ""));
  // A tile whose view declares `input` renders as the prompt.
  if (node.input) {
    tile.append(chrome, inputDock(node.input, dispatchUi));
  } else if (node.chrome_hidden) {
    tile.append(view(node.view || {}, dispatchUi));
  } else {
    tile.append(chrome, view(node.view || {}, dispatchUi), viewFooter(node.view || {}));
  }
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

function view(viewVm, dispatchUi) {
  const body = el("div", "ryeos-tile-body");
  switch (viewVm.type) {
    case "text":
      body.append(textView(viewVm));
      break;
    case "map":
      body.append(sceneMap(viewVm.scene, dispatchUi));
      break;
    case "atlas":
      body.append(atlasTile(viewVm.scene, dispatchUi));
      break;
    case "rows":
      body.append(listHeader(viewVm.title, (viewVm.columns || []).join(" · ")), rows(viewVm.rows || [], "rows", dispatchUi));
      break;
    case "table":
      body.append(tableView(viewVm, dispatchUi));
      break;
    case "sections":
      body.append(sectionsView(viewVm, dispatchUi));
      break;
    case "timeline":
      body.append(timeline(viewVm));
      break;
    case "placeholder":
      body.append(textEl("h2", viewVm.title), textEl("p", viewVm.message));
      break;
    default:
      body.append(textEl("p", `Unknown view: ${viewVm.type || "missing"}`));
  }
  return body;
}

function textView(viewVm) {
  const wrap = el("div", "ryeos-text-view");
  const position = viewVm.position || { x: 0.5, y: 0.5 };
  wrap.style.left = `${Math.max(0, Math.min(1, Number(position.x ?? 0.5))) * 100}%`;
  wrap.style.top = `${Math.max(0, Math.min(1, Number(position.y ?? 0.5))) * 100}%`;
  for (const line of viewVm.lines || []) {
    wrap.append(textEl("div", line.text || "", `ryeos-text-line ${line.tone || "neutral"}`));
  }
  return wrap;
}

function listHeader(title, detail) {
  const header = el("div", "ryeos-list-header");
  header.append(textEl("strong", title || "list"), textEl("span", detail || ""));
  return header;
}

function viewFooter(viewVm) {
  const footer = el("footer", "ryeos-tile-footer");
  const provenance = viewVm.provenance || "";
  const hints = (viewVm.affordance_hints || []).join(" · ");
  footer.append(textEl("span", provenance), textEl("small", hints));
  return footer;
}

function timeline(viewVm) {
  const wrap = el("section", "ryeos-timeline");
  wrap.append(listHeader(viewVm.title || "timeline", ""));
  const entries = el("div", "ryeos-timeline-entries");
  for (const entry of viewVm.entries || []) {
    entries.append(timelineEntry(entry));
  }
  if (!(viewVm.entries || []).length) entries.append(textEl("p", "No timeline events loaded."));
  wrap.append(entries);
  return wrap;
}

function timelineEntry(entry) {
  switch (entry.type) {
    case "block":
      return textEl("p", entry.text || "", `ryeos-timeline-block ${entry.tone || "neutral"}`);
    case "pair": {
      const row = el("div", `ryeos-timeline-pair ${entry.tone || "neutral"}${entry.pending ? " pending" : ""}`);
      row.append(textEl("span", entry.pending ? "▸" : entry.tone === "danger" ? "✗" : "✓"), textEl("strong", entry.summary || "tool"), textEl("small", entry.meta || ""));
      return row;
    }
    case "separator":
      return textEl("div", entry.label || "turn", "ryeos-timeline-separator");
    case "line":
    default: {
      const row = el("div", `ryeos-timeline-line ${entry.tone || "neutral"}`);
      row.append(textEl("span", toneGlyph(entry.tone)), textEl("strong", entry.primary || "event"), textEl("small", entry.meta || ""));
      return row;
    }
  }
}

// Tone → glyph, mirroring the terminal's theme::tone_glyph so a toned line
// reads the same on both clients (✓ done, ✗ failed, ! warned, › accent).
function toneGlyph(tone) {
  switch (tone) {
    case "good": return "✓";
    case "warn": return "!";
    case "danger": return "✗";
    case "accent": return "›";
    default: return "•";
  }
}

// The generic backdrop scene renderer (web parity with the terminal's
// widgets/scene.rs): the same RyeOsSceneModel drives both. Objects are
// orthographically projected into the stage; particles twinkle by a
// function of the scene's `generation` (CSS/JS opacity + glyph size),
// with a per-object phase so they don't pulse in unison. No per-art code,
// no `ambient` enum — new backgrounds are new scene content.
const TWINKLE_GLYPHS = ["·", "•", "●"];

function backdropScene(scene, _dispatchUi) {
  const wrap = el("section", "ryeos-backdrop ryeos-scene");
  const stage = el("div", "ryeos-backdrop-stage ryeos-scene-stage");
  const generation = Number(scene?.generation || 0);
  const objects = scene?.objects || [];
  // Fit the object cloud to the stage (orthographic; +y up → top flips).
  const fitObjects = objects.filter((o) => o.fit !== false);
  const fitSource = fitObjects.length ? fitObjects : objects;
  const xs = [];
  const ys = [];
  fitSource.forEach((object) => {
    const positions = [object.position || [0, 0, 0]];
    const motion = object.break;
    if (motion?.away) {
      const base = object.position || [0, 0, 0];
      positions.push([
        Number(base[0] || 0) + Number(motion.away[0] || 0),
        Number(base[1] || 0) + Number(motion.away[1] || 0),
        Number(base[2] || 0) + Number(motion.away[2] || 0),
      ]);
    }
    if (object.kind === "fill" && Array.isArray(object.scale)) {
      const radius = Number(object.scale[0] || 1);
      const reach = Number(object.scale[1] || 1) + Number(object.scale[2] || 0);
      positions.forEach((px) => {
        xs.push(px[0] - radius, px[0] + radius);
        ys.push(px[1] - reach, px[1] + reach);
      });
    } else {
      positions.forEach((px) => {
        xs.push(px[0] || 0);
        ys.push(px[1] || 0);
      });
    }
  });
  const minX = Math.min(...xs, -1), maxX = Math.max(...xs, 1);
  const minY = Math.min(...ys, -1), maxY = Math.max(...ys, 1);
  const spanX = Math.max(0.001, maxX - minX);
  const spanY = Math.max(0.001, maxY - minY);
  objects.forEach((object, index) => {
    const px = animatedObjectPosition(object, generation);
    const left = 6 + ((px[0] - minX) / spanX) * 88;
    const top = 6 + (1 - (px[1] - minY) / spanY) * 88;
    if (object.kind === "text" || object.kind === "label_anchor") {
      const label = textEl("div", object.label || "", `ryeos-backdrop-text ${object.tone || "neutral"}`);
      label.style.left = `${left}%`;
      label.style.top = `${top}%`;
      label.style.setProperty("--node-color", object.color || "#d65d0e");
      stage.append(label);
      return;
    }
    if (object.kind === "fill") {
      const shard = el("span", `ryeos-backdrop-shard ${object.tone || "neutral"}`);
      const scale = object.scale || [1, 1, 0];
      const clip = object.clip || {};
      const fullWidth = Math.max(8, (Number(scale[0] || 1) * 2 / spanX) * 88);
      const fullHeight = Math.max(14, ((Number(scale[1] || 1) + Number(scale[2] || 0)) * 2 / spanY) * 88);
      const xMin = Number(clip.x_min ?? -Number(scale[0] || 1));
      const xMax = Number(clip.x_max ?? Number(scale[0] || 1));
      const sliceWidth = Math.max(0.12, Math.min(1, (xMax - xMin) / Math.max(0.001, Number(scale[0] || 1) * 2)));
      const sliceCenter = (xMin + xMax) / 2 / Math.max(0.001, Number(scale[0] || 1) * 2);
      const spin = Number(object.spin || 0) * generation;
      shard.style.left = `${left + sliceCenter * fullWidth * 0.18}%`;
      shard.style.top = `${top}%`;
      shard.style.width = `${fullWidth * sliceWidth}%`;
      shard.style.height = `${fullHeight}%`;
      shard.style.opacity = String(object.opacity ?? 1);
      shard.style.setProperty("--node-color", object.color || "#d65d0e");
      shard.style.setProperty("--shard-tilt", `${((spin % 18) - 9).toFixed(2)}deg`);
      stage.append(shard);
      return;
    }
    const dot = el("span", `ryeos-backdrop-dot ${object.tone || "neutral"}`);
    dot.style.left = `${left}%`;
    dot.style.top = `${top}%`;
    dot.style.setProperty("--node-color", object.color || "#a89984");
    if (object.kind === "particle") {
      const phase = phaseFor(object.id || "", index);
      const step = (generation + phase) % 4;
      const base = sizeIndex(object.scale?.[0] ?? 0.5);
      const delta = step === 1 ? 1 : step === 3 ? -1 : 0;
      const idx = Math.max(0, Math.min(TWINKLE_GLYPHS.length - 1, base + delta));
      dot.textContent = TWINKLE_GLYPHS[idx];
      dot.style.opacity = String(step === 3 ? 0.4 : (object.opacity ?? 0.8));
    } else {
      dot.textContent = TWINKLE_GLYPHS[sizeIndex(object.scale?.[0] ?? 0.5)];
      dot.style.opacity = String(object.opacity ?? 1);
    }
    stage.append(dot);
  });
  wrap.append(stage);
  return wrap;
}

function animatedObjectPosition(object, generation) {
  const base = object?.position || [0, 0, 0];
  const motion = object?.break;
  if (!motion?.away) return base;
  const period = Math.max(4, Number(motion.period || 96));
  const phase = Number(motion.phase || 0);
  const progress = ((generation + phase) % period) / period;
  const eased = 0.5 - 0.5 * Math.cos(progress * Math.PI * 2);
  return [
    Number(base[0] || 0) + Number(motion.away[0] || 0) * eased,
    Number(base[1] || 0) + Number(motion.away[1] || 0) * eased,
    Number(base[2] || 0) + Number(motion.away[2] || 0) * eased,
  ];
}

function sizeIndex(scale) {
  if (scale >= 0.85) return 2;
  if (scale >= 0.5) return 1;
  return 0;
}

function phaseFor(id, index) {
  let hash = index >>> 0;
  for (let i = 0; i < id.length; i += 1) {
    hash = (hash * 31 + id.charCodeAt(i)) >>> 0;
  }
  return hash % 4;
}

function sceneMap(scene, dispatchUi) {
  const wrap = el("section", "ryeos-scene");
  const header = el("div", "ryeos-scene-header");
  header.append(textEl("h2", "Graph"), textEl("p", "Local node, remotes, and workspace topology."));
  const stage = el("div", "ryeos-scene-stage");
  for (const object of scene?.objects || []) {
    const node = el("button", `ryeos-scene-node ${object.kind} ${object.tone || "neutral"}`);
    node.type = "button";
    node.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    node.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    node.style.setProperty("--node-color", object.color || "#fabd2f");
    node.style.opacity = String(object.opacity ?? 1);
    if (object.kind === "link") node.style.width = `${Math.max(72, (object.scale?.[0] || 1) * 24)}px`;
    node.disabled = !object.action;
    node.append(textEl("strong", object.label || object.id), textEl("span", object.kind || "object"));
    if (object.action) node.addEventListener("click", () => dispatchUi({ type: "activate", action: object.action }));
    stage.append(node);
  }
  wrap.append(header, stage);
  return wrap;
}

function atlasTile(scene, dispatchUi) {
  const wrap = el("section", "ryeos-scene ryeos-atlas-map");
  if (scene?.atlas) return atlasMap(scene.atlas, dispatchUi, wrap);
  const empty = el("div", "ryeos-atlas-empty");
  empty.append(textEl("h2", "Namespace Atlas"), textEl("p", "Loading item graph…"));
  wrap.append(empty);
  return wrap;
}

function atlasMap(atlas, dispatchUi, wrap = el("section", "ryeos-scene")) {
  wrap.classList.add("ryeos-atlas-map");
  const atlasUi = atlas.ui || {};
  const visibleLayers = new Set(atlasUi.visible_layers || ["directive", "tool", "knowledge", "config", "other"]);
  const activeLens = atlasUi.active_lens || "none";
  for (const kind of ["directive", "tool", "knowledge", "config", "other"]) {
    if (!visibleLayers.has(kind)) wrap.classList.add(`hide-${kind}`);
  }
  if (activeLens === "knowledge") wrap.classList.add("lens-knowledge");
  const header = el("div", "ryeos-scene-header");
  const title = el("div", "ryeos-atlas-title");
  const stackCount = (atlas.nodes || []).filter((node) => (node.stack || []).length).length;
  title.append(textEl("h2", "Namespace Atlas"), textEl("p", `${stackCount} stacks · ${(atlas.regions || []).length} capability regions`));
  const controls = el("div", "ryeos-atlas-controls");
  for (const [kind, label] of [["directive", "Directives"], ["tool", "Tools"], ["knowledge", "Knowledge"], ["config", "Config"]]) {
    const pressed = visibleLayers.has(kind);
    const button = el("button", `ryeos-atlas-control ${kind}`);
    button.type = "button";
    button.textContent = label;
    button.setAttribute("aria-pressed", pressed ? "true" : "false");
    button.addEventListener("click", () => {
      dispatchUi({ type: "set_atlas_layer_visible", kind, visible: !pressed });
    });
    controls.append(button);
  }
  const knowledgeLens = el("button", "ryeos-atlas-control lens");
  const knowledgeLensEnabled = activeLens === "knowledge";
  knowledgeLens.type = "button";
  knowledgeLens.textContent = "Knowledge lens";
  knowledgeLens.setAttribute("aria-pressed", knowledgeLensEnabled ? "true" : "false");
  knowledgeLens.addEventListener("click", () => {
    dispatchUi({ type: "set_atlas_lens", lens: knowledgeLensEnabled ? "none" : "knowledge" });
  });
  controls.append(knowledgeLens);
  const legend = el("div", "ryeos-atlas-legend");
  for (const [kind, label] of [["directive", "Directive"], ["tool", "Tool"], ["knowledge", "Knowledge"], ["config", "Config"]]) {
    const item = textEl("span", label);
    item.className = `ryeos-atlas-legend-item ${kind}`;
    legend.append(item);
  }
  header.append(title, controls, legend);

  const stage = el("div", "ryeos-scene-stage ryeos-atlas-stage simple");
  const viewport = el("div", "ryeos-atlas-viewport");
  viewport.append(el("div", "ryeos-atlas-grid"));
  applyAtlasViewport(viewport);
  wireAtlasViewport(stage, viewport);
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
      left: 12 + ((p[0] - xMin) / xSpan) * 76,
      top: 12 + ((p[2] - zMin) / zSpan) * 76,
    };
  };

  const nodes = atlas.nodes || [];
  const kinds = ["directive", "tool", "knowledge", "config", "other"];
  for (const kind of kinds) {
    const layer = el("div", `ryeos-atlas-kind-layer ${kind}`);
    if (!visibleLayers.has(kind)) layer.hidden = true;
    for (const node of nodes) {
      const stack = (node.stack || []).filter((item) => (item.kind || "other") === kind && atlasItemVisible(atlas, item));
      if (!stack.length) continue;
      const p = position(node);
      const cluster = el("div", `ryeos-atlas-cluster ${kind}${node.state?.selected ? " selected" : ""}${node.state?.highlighted ? " highlighted" : ""}`);
      cluster.style.left = `${p.left}%`;
      cluster.style.top = `${p.top}%`;
      cluster.title = node.namespace_key || node.label || kind;
      for (const [index, item] of stack.slice(0, 5).entries()) {
        const dot = el("button", `ryeos-atlas-dot ${item.kind || "other"}`);
        dot.classList.add(`scope-${item.scope || "unknown"}`);
        dot.type = "button";
        dot.style.setProperty("--dot-index", String(index));
        dot.title = item.canonical_ref || item.label || node.namespace_key;
        dot.textContent = stack.length > 1 && index === 4 ? "+" : "";
        dot.addEventListener("click", (event) => {
          event.stopPropagation();
          if (!item.canonical_ref) return;
          dispatchUi({ type: "activate", action: { type: "inspect_item", canonical_ref: item.canonical_ref } });
        });
        cluster.append(dot);
      }
      const label = textEl("span", node.label || node.namespace_key || kind);
      label.className = "ryeos-atlas-cluster-label";
      cluster.append(label);
      layer.append(cluster);
    }
    viewport.append(layer);
  }
  stage.append(viewport);
  wrap.append(header, stage);
  return wrap;
}

function applyAtlasViewport(viewport) {
  viewport.style.transform = `translate(${atlasViewport.panX}px, ${atlasViewport.panY}px) scale(${atlasViewport.zoom})`;
  viewport.style.setProperty("--atlas-grid-scale", String(atlasViewport.zoom));
}

function wireAtlasViewport(stage, viewport) {
  stage.addEventListener("wheel", (event) => {
    event.preventDefault();
    const rect = stage.getBoundingClientRect();
    const cursorX = event.clientX - rect.left;
    const cursorY = event.clientY - rect.top;
    const previousZoom = atlasViewport.zoom;
    const nextZoom = clamp(previousZoom * Math.exp(-event.deltaY * 0.0012), 0.45, 3.8);
    const ratio = nextZoom / previousZoom;
    atlasViewport.panX = cursorX - (cursorX - atlasViewport.panX) * ratio;
    atlasViewport.panY = cursorY - (cursorY - atlasViewport.panY) * ratio;
    atlasViewport.zoom = nextZoom;
    applyAtlasViewport(viewport);
  }, { passive: false });

  let drag = null;
  stage.addEventListener("pointerdown", (event) => {
    if (event.target.closest("button")) return;
    drag = {
      pointerId: event.pointerId,
      x: event.clientX,
      y: event.clientY,
      panX: atlasViewport.panX,
      panY: atlasViewport.panY,
    };
    stage.setPointerCapture?.(event.pointerId);
    stage.classList.add("panning");
  });
  stage.addEventListener("pointermove", (event) => {
    if (!drag || drag.pointerId !== event.pointerId) return;
    atlasViewport.panX = drag.panX + event.clientX - drag.x;
    atlasViewport.panY = drag.panY + event.clientY - drag.y;
    applyAtlasViewport(viewport);
  });
  const endDrag = (event) => {
    if (!drag || drag.pointerId !== event.pointerId) return;
    drag = null;
    stage.classList.remove("panning");
  };
  stage.addEventListener("pointerup", endDrag);
  stage.addEventListener("pointercancel", endDrag);
}

function clamp(value, min, max) {
  return Math.min(max, Math.max(min, value));
}

function atlasItemVisible(atlas, item) {
  const atlasUi = atlas?.ui || {};
  const visibleLayers = new Set(atlasUi.visible_layers || ["directive", "tool", "knowledge", "config", "other"]);
  const kind = item.kind || "other";
  if (!visibleLayers.has(kind)) return false;
  if ((atlasUi.active_lens || "none") === "knowledge") return kind === "knowledge";
  return true;
}

function rows(items, kind, dispatchUi) {
  const list = el("div", `ryeos-rows lf ${kind || "rows"}`);
  items.forEach((item, index) => {
    const row = el("button", `ryeos-row ${item.tone || "neutral"}${item.selected ? " selected" : ""}`);
    row.type = "button";
    row.dataset.rowIndex = String(index);
    row.disabled = !item.action;
    row.append(
      textEl("span", rowGlyph(item, kind), "ryeos-row-glyph"),
      textEl("strong", item.primary),
      textEl("span", item.secondary || ""),
      textEl("small", item.meta || ""),
    );
    if (item.action) row.addEventListener("click", () => dispatchUi({ type: "activate", action: item.action }));
    list.append(row);
  });
  return list;
}

function rowGlyph(item) {
  switch (item.tone || "neutral") {
    case "good": return "✓";
    case "warn": return "!";
    case "danger": return "✗";
    case "accent": return "›";
    default: return "•";
  }
}

// The table widget: aligned cells under column headers, a leading tone-glyph
// gutter, full-width selection — the typed list surface for non-chat lenses
// (threads/bundles/schedules). Reference semantics live in the terminal's
// widgets/table.rs: the header row and every body row share column origins,
// the first cell is the identifier (foreground) and later cells are secondary
// detail (muted) unless the whole row is selected. Column count prefers the
// declared headers, else the widest row so cells still align when headers are
// absent.
function tableView(viewVm, dispatchUi) {
  const wrap = el("section", "ryeos-table lf");
  wrap.append(listHeader(viewVm.title || "table", ""));
  const columns = viewVm.columns || [];
  const items = viewVm.rows || [];
  const ncols = Math.max(
    1,
    columns.length,
    items.reduce((widest, row) => Math.max(widest, (row.cells || []).length), 0),
  );
  const grid = el("div", "ryeos-table-grid");
  grid.style.setProperty("--table-cols", String(ncols));
  if (columns.length) {
    const head = el("div", "ryeos-table-head");
    head.append(el("span", "ryeos-table-glyph"));
    for (const column of columns) head.append(textEl("span", column, "ryeos-table-col"));
    grid.append(head);
  }
  items.forEach((item, index) => grid.append(tableRow(item, ncols, index, dispatchUi)));
  if (!items.length) grid.append(textEl("p", "No rows loaded.", "ryeos-table-empty"));
  wrap.append(grid);
  return wrap;
}

function tableRow(item, ncols, index, dispatchUi) {
  const row = el("button", `ryeos-table-row ${item.tone || "neutral"}${item.selected ? " selected" : ""}`);
  row.type = "button";
  row.dataset.rowIndex = String(index);
  row.disabled = !item.action;
  row.append(textEl("span", rowGlyph(item), "ryeos-table-glyph"));
  const cells = item.cells || [];
  // Per-cell tone overrides (parallel to cells; absent for tables whose
  // columns declare no tone) — a toned cell renders distinctly from the
  // muted secondary default, mirroring the terminal table widget. Neutral
  // means "no override" on both renderers, never a color.
  const cellTones = item.cell_tones || [];
  for (let i = 0; i < ncols; i += 1) {
    const tone = cellTones[i] && cellTones[i] !== "neutral" ? ` tone-${cellTones[i]}` : "";
    row.append(textEl("span", cells[i] || "", `ryeos-table-cell${i === 0 ? " lead" : ""}${tone}`));
  }
  if (item.action) row.addEventListener("click", () => dispatchUi({ type: "activate", action: item.action }));
  return row;
}

// The sections widget: a foldable multi-section list (the magit-style status
// surface). Each section is a `▾/▸ Title (count)` header followed by its rows,
// indented; a collapsed section shows only its header, and its `count` still
// reflects the hidden rows. Reference semantics live in the terminal's
// widgets/sections.rs. Rows reuse the rows-widget renderer (RyeOsRowVm), so
// tone glyph, primary/secondary/meta, and per-row actions come for free.
function sectionsView(viewVm, dispatchUi) {
  const wrap = el("section", "ryeos-sections");
  wrap.append(listHeader(viewVm.title || "sections", ""));
  const body = el("div", "ryeos-sections-body");
  const sections = viewVm.sections || [];
  for (const section of sections) body.append(sectionGroup(section, dispatchUi));
  if (!sections.length) body.append(textEl("p", "No sections loaded.", "ryeos-sections-empty"));
  wrap.append(body);
  return wrap;
}

function sectionGroup(section, dispatchUi) {
  const collapsed = !!section.collapsed;
  const group = el("div", `ryeos-section${collapsed ? " collapsed" : ""}`);
  // The header is the point that re-expands a collapsed section; when it
  // carries the cursor, highlight the full line like a selected row.
  const header = el("div", `ryeos-section-header${section.header_selected ? " selected" : ""}`);
  const count = section.count ?? (section.rows || []).length;
  header.append(
    textEl("span", collapsed ? "▸" : "▾", "ryeos-section-fold"),
    textEl("strong", section.title || "section"),
    textEl("span", `(${count})`, "ryeos-section-count"),
  );
  group.append(header);
  if (!collapsed) group.append(rows(section.rows || [], "section", dispatchUi));
  return group;
}
