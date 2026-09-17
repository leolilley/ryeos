import { el, textEl } from "/ui/assets/ryeos_components_primitives.js";
import { fieldComponent } from "/ui/assets/ryeos_components_field.js";

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
  // A constrained viewport projects the focused group, not a rewritten tree.
  // Every visible slot and centre group remains reachable through shared focus.
  if (window.matchMedia("(max-width: 760px)").matches) {
    const groups = [];
    const collect = (node) => {
      if (!node) return;
      if (node.type === "split") { collect(node.first); collect(node.second); }
      else groups.push(node);
    };
    collect(vm.root);
    const slots = Object.entries(vm.docks || {}).filter(([, slot]) => slot);
    const focusedSlot = slots.find(([, slot]) => slot.focused);
    const focused = groups.find((node) => node.focused) || groups[0];
    if (focused || focusedSlot) {
      const plane = el("section", "ryeos-compact-plane");
      const chooser = el("nav", "ryeos-region-chooser");
      chooser.setAttribute("aria-label", "Workspace regions");
      for (const group of groups) {
        const button = textEl("button", group.title, !focusedSlot && group === focused ? "active" : "");
        button.type = "button";
        button.addEventListener("click", () => dispatchUi({ type: "focus_changed", target: group.tile_id }));
        chooser.append(button);
      }
      for (const [edge, slot] of slots) {
        const button = textEl("button", slot.title || edge, slot.focused ? "active" : "");
        button.type = "button";
        button.addEventListener("click", () => dispatchUi({ type: "focus_dock", edge }));
        chooser.append(button);
      }
      const content = el("div", "ryeos-compact-content");
      content.append(focusedSlot ? dockTile(focusedSlot[1], dispatchUi) : layoutNode(focused, dispatchUi, motion, {
        guard: vm.layout_guard, min: vm.split_min_ratio, max: vm.split_max_ratio,
      }));
      plane.append(chooser, content);
      return plane;
    }
  }
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
    stack.append(layoutNode(vm.root, dispatchUi, motion, {
      guard: vm.layout_guard, min: vm.split_min_ratio, max: vm.split_max_ratio,
    }));
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
  const tile = el("aside", `ryeos-dock-tile ${edge}${dockVm.focused ? " focused" : ""}`);
  tile.dataset.viewInstanceKey = dockVm.instance_key || "";
  tile.__dockSize = dockVm.size;
  tile.addEventListener("mousedown", (event) => {
    if (event.target.closest("button,input,select,textarea,a")) return;
    dispatchUi({ type: "focus_dock", edge });
  });
  const chrome = el("header", "ryeos-dock-chrome");
  chrome.title = dockVm.view?.provenance || "";
  chrome.append(textEl("strong", dockVm.supplement?.frame_label || dockVm.title || edge));
  if (dockVm.supplement?.frame_detail) chrome.append(textEl("small", dockVm.supplement.frame_detail));
  tile.append(chrome, dockView(dockVm, dispatchUi));
  return tile;
}

// Input is an orthogonal capability: it does not replace a view's content.
// Filters precede their content; composers remain below it in the same region.
function dockView(instanceVm, dispatchUi) {
  const body = el("div", "ryeos-dock-body");
  appendViewContent(body, instanceVm, dispatchUi);
  if (instanceVm.supplement?.footer) body.append(contentFooter(instanceVm.supplement));
  // Slot chrome already labels the view. Keep distinct authored content headings.
  if (instanceVm.title === instanceVm.view?.title) {
    body.querySelector(".ryeos-list-header")?.remove();
  }
  return body;
}

function appendViewContent(host, vm, dispatchUi) {
  if (vm.input?.live_filter) host.append(inputDock(vm.input, dispatchUi));
  const content = view(vm.view || {}, vm.instance_key || "", vm.tile_id || "", dispatchUi);
  if (vm.heading) {
    const heading = el("header", "ryeos-content-heading");
    if (vm.heading.eyebrow) heading.append(textEl("small", vm.heading.eyebrow));
    heading.append(textEl("h1", vm.heading.title));
    if (vm.heading.summary) heading.append(textEl("p", vm.heading.summary));
    const metadata = el("div", "ryeos-content-metadata");
    for (const value of vm.heading.metadata || []) metadata.append(textEl("span", value));
    if (metadata.childElementCount) heading.append(metadata);
    content.querySelector(".ryeos-list-header")?.remove();
    content.prepend(heading);
  } else if (vm.title === vm.view?.title) {
    // Navigation labels are not content headings. Do not repeat one title
    // in the frame, tabs and body when no distinct introduction was authored.
    content.querySelector(".ryeos-list-header")?.remove();
  }
  if (vm.supplement?.scene) {
    const diagram = sceneDiagram(vm.supplement.scene);
    const heading = content.querySelector(".ryeos-content-heading");
    if (heading) heading.after(diagram); else content.prepend(diagram);
  }
  if (vm.supplement?.excerpt?.length) {
    const excerpt = el("section", "ryeos-code-excerpt");
    if (vm.supplement.excerpt_title) excerpt.append(textEl("header", vm.supplement.excerpt_title));
    for (const line of vm.supplement.excerpt) {
      const row = el("div", `ryeos-code-line tone-${line.tone || "neutral"}`);
      row.append(textEl("small", line.field), textEl("code", line.value));
      excerpt.append(row);
    }
    content.append(excerpt);
  }
  host.append(content);
  if (vm.input && !vm.input.live_filter) host.append(inputDock(vm.input, dispatchUi));
}

function inputDock(inputVm, dispatchUi) {
  const send = (action) => dispatchUi({ type: "input_at", address: inputVm.address, action });
  const wrap = el("section", "ryeos-input-dock");
  const meta = el("div", "ryeos-input-meta");
  meta.append(
    textEl("span", "→", "ryeos-input-arrow"),
    textEl("strong", inputVm.route_label || "target: ryeos"),
    textEl("small", inputVm.text ? "DRAFT" : ""),
  );
  meta.title = inputVm.hint || "";

  const row = el("div", "ryeos-input-row");
  const input = document.createElement("textarea");
  input.rows = 1;
  input.value = inputVm.text || "";
  input.placeholder = inputVm.placeholder || "type RyeOS input…";
  input.setAttribute("aria-label", inputVm.route_label ? `Input for ${inputVm.route_label}` : "RyeOS input");
  input.spellcheck = false;
  input.autocomplete = "off";
  input.setAttribute("data-focus-key", `input:${JSON.stringify(inputVm.address)}`);
  input.addEventListener("focus", () => { if (!inputVm.focused) send({ type: "focus" }); });
  input.addEventListener("input", () => {
    send({ type: "set_text", text: input.value, cursor: byteCursor(input.value, input.selectionStart || 0) });
  });
  input.addEventListener("keydown", (event) => {
    if (event.key === "Enter" && event.shiftKey) {
      event.preventDefault();
      send({ type: "submit", interrupt: false });
    } else if (event.key === "Tab" && !event.shiftKey && !event.ctrlKey && !event.altKey && !event.metaKey) {
      event.preventDefault();
      send({ type: "complete" });
    }
  });
  const submit = el("button", "ryeos-input-submit");
  submit.type = "button";
  submit.disabled = !inputVm.submit_enabled;
  submit.textContent = "↑";
  submit.setAttribute("aria-label", "Send RyeOS input");
  submit.addEventListener("click", () => send({ type: "submit", interrupt: false }));
  row.append(input);
  const actions = el("div", "ryeos-input-actions");
  actions.append(textEl("span", "Tab · complete", "ryeos-input-help"));
  actions.append(textEl("small", "Shift Enter to send"), submit);
  wrap.append(meta, row, actions);

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

export const VIEW_DRAG_TYPE = "application/x-ryeos-view";

function draggableView(element, tileId, edit) {
  element.draggable = true;
  element.addEventListener("dragstart", (event) => {
    if (!edit.guard || !event.dataTransfer) { event.preventDefault(); return; }
    event.dataTransfer.effectAllowed = "move";
    event.dataTransfer.setData(VIEW_DRAG_TYPE, JSON.stringify({ tile_id: tileId, guard: edit.guard }));
  });
}

export function droppedView(event, edit) {
  try {
    const raw = event.dataTransfer?.getData(VIEW_DRAG_TYPE) || "";
    if (raw.length > 512) return null;
    const value = JSON.parse(raw);
    return value.guard === edit.guard && typeof value.tile_id === "string" ? value : null;
  } catch { return null; }
}

function splitDivider(wrap, node, edit, path, dispatchUi) {
  const divider = el("div", "ryeos-split-divider");
  const horizontal = node.axis === "horizontal";
  divider.tabIndex = 0;
  divider.setAttribute("role", "separator");
  divider.setAttribute("aria-label", "Resize adjacent regions");
  divider.setAttribute("aria-orientation", horizontal ? "vertical" : "horizontal");
  divider.setAttribute("aria-valuemin", String(edit.min * 100));
  divider.setAttribute("aria-valuemax", String(edit.max * 100));
  divider.setAttribute("aria-valuenow", String(Math.round(node.ratio * 100)));
  divider.dataset.focusKey = `divider:${path.join("/")}`;
  const resize = (ratio) => {
    if (!Number.isFinite(ratio) || !edit.guard) return;
    dispatchUi({ type: "activate", intent: { type: "resize_split", layout_guard: edit.guard, path,
      ratio: Math.min(edit.max, Math.max(edit.min, ratio)) } });
  };
  divider.addEventListener("keydown", (event) => {
    const delta = (horizontal ? ["ArrowLeft", "ArrowRight"] : ["ArrowUp", "ArrowDown"]).indexOf(event.key);
    if (delta < 0) return;
    event.preventDefault(); event.stopPropagation();
    resize(node.ratio + (delta === 0 ? -0.01 : 0.01));
  });
  divider.addEventListener("pointerdown", (event) => {
    if (event.button !== 0) return;
    event.preventDefault(); event.stopPropagation();
    divider.focus();
    divider.setPointerCapture(event.pointerId);
    const finish = (end) => {
      divider.removeEventListener("pointerup", finish);
      divider.removeEventListener("pointercancel", cancel);
      if (end.type !== "pointerup") return;
      const rect = wrap.getBoundingClientRect();
      resize(horizontal ? (end.clientX - rect.left) / rect.width : (end.clientY - rect.top) / rect.height);
    };
    const cancel = () => { divider.removeEventListener("pointerup", finish); divider.removeEventListener("pointercancel", cancel); };
    divider.addEventListener("pointerup", finish, { once: true });
    divider.addEventListener("pointercancel", cancel, { once: true });
  });
  return divider;
}

function layoutNode(node, dispatchUi, motion = [], edit = {}, path = []) {
  if (node.type === "split") {
    const wrap = el("div", `ryeos-split ${node.axis}`);
    wrap.style.setProperty("--split-ratio", `${Math.round((node.ratio || 0.5) * 100)}%`);
    wrap.append(layoutNode(node.first, dispatchUi, motion, edit, [...path, "first"]),
      layoutNode(node.second, dispatchUi, motion, edit, [...path, "second"]),
      splitDivider(wrap, node, edit, path, dispatchUi));
    return wrap;
  }
  const tile = el("section", `ryeos-tile${node.focused ? " focused" : ""}`);
  if (node.chrome_hidden) tile.classList.add("chrome-hidden");
  tile.dataset.viewInstanceKey = node.instance_key || "";
  tile.dataset.tileId = node.tile_id || "";
  tile.addEventListener("dragover", (event) => {
    if (event.dataTransfer?.types.includes(VIEW_DRAG_TYPE)) { event.preventDefault(); event.dataTransfer.dropEffect = "move"; }
  });
  tile.addEventListener("drop", (event) => {
    event.preventDefault(); event.stopPropagation();
    const source = droppedView(event, edit);
    if (!source || source.tile_id === node.tile_id) return;
    const rect = tile.getBoundingClientRect();
    const x = (event.clientX - rect.left) / rect.width;
    const y = (event.clientY - rect.top) / rect.height;
    const edge = x < .2 ? "left" : x > .8 ? "right" : y < .2 ? "up" : y > .8 ? "down" : null;
    dispatchUi({ type: "activate", intent: edge
      ? { type: "move_tile_beside", layout_guard: edit.guard, tile_id: source.tile_id, target_tile_id: node.tile_id, edge }
      : { type: "move_tile_to_group", layout_guard: edit.guard, tile_id: source.tile_id, target_tile_id: node.tile_id, index: node.tabs.length } });
  });
  const motionName = motionForTile(node, motion);
  if (motionName) tile.dataset.motion = motionName;
  tile.addEventListener("mousedown", (event) => {
    if (event.target.closest("button,input,select,textarea,a")) return;
    if (node.focused) return;
    dispatchUi({ type: "focus_changed", target: node.tile_id || null });
  });
  const chrome = el("header", "ryeos-tile-chrome");
  chrome.title = node.view?.provenance || "";
  const title = textEl("strong", node.supplement?.frame_label || node.title || "View");
  draggableView(title, node.tile_id, edit);
  chrome.append(title);
  if (node.supplement?.frame_detail) chrome.append(textEl("span", node.supplement.frame_detail, "ryeos-frame-detail"));
  if (node.intents?.length) {
    const menu = el("details", "ryeos-frame-menu");
    const toggle = textEl("summary", "⋮");
    toggle.setAttribute("aria-label", `Actions for ${node.title}`);
    menu.append(toggle);
    const choices = el("div", "ryeos-frame-choices");
    for (const entry of node.intents) {
      const button = textEl("button", entry.title || entry.label);
      button.type = "button";
      button.addEventListener("click", () => dispatchUi({type:"activate", intent:entry.intent}));
      choices.append(button);
    }
    menu.append(choices);
    chrome.append(menu);
  }
  if (!node.chrome_hidden || (node.tabs || []).length > 1) tile.append(chrome);
  if ((node.tabs || []).length > 1) {
    const tabs = el("nav", "ryeos-view-tabs");
    tabs.setAttribute("role", "tablist");
    tabs.setAttribute("aria-label", "Views in this region");
    node.tabs.forEach((tab, index) => {
      const button = textEl("button", tab.title, tab.active ? "active" : "");
      button.type = "button";
      button.setAttribute("role", "tab");
      button.setAttribute("aria-selected", String(tab.active));
      button.tabIndex = tab.active ? 0 : -1;
      button.dataset.focusKey = `view-tab:${tab.tile_id}`;
      draggableView(button, tab.tile_id, edit);
      button.addEventListener("drop", (event) => {
        event.preventDefault(); event.stopPropagation();
        const source = droppedView(event, edit);
        if (source) dispatchUi({ type: "activate", intent: { type: "move_tile_to_group", layout_guard: edit.guard,
          tile_id: source.tile_id, target_tile_id: tab.tile_id, index } });
      });
      button.addEventListener("click", () => dispatchUi({ type: "focus_changed", target: tab.tile_id }));
      button.addEventListener("keydown", (event) => {
        const offset = event.key === "ArrowRight" ? 1 : event.key === "ArrowLeft" ? -1 : 0;
        if (!offset) return;
        event.preventDefault();
        const nextIndex = (index + offset + node.tabs.length) % node.tabs.length;
        const next = node.tabs[nextIndex];
        // The shell captures DOM focus before replacing this projection.
        // Move it to the selected tab first so restoration follows selection.
        tabs.children[nextIndex].focus();
        dispatchUi({ type: "focus_changed", target: next.tile_id });
      });
      tabs.append(button);
    });
    tile.append(tabs);
  }
  appendViewContent(tile, node, dispatchUi);
  if (node.supplement?.footer) tile.append(contentFooter(node.supplement));
  if (!node.chrome_hidden) tile.append(viewFooter(node.view || {}));
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

function view(viewVm, instanceKey, tileId, dispatchUi) {
  const body = el("div", "ryeos-tile-body");
  switch (viewVm.type) {
    case "field":
      body.append(fieldComponent(viewVm.field || {}, instanceKey, dispatchUi));
      break;
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
      body.append(listHeader(viewVm.title, (viewVm.columns || []).join(" · ")), rows(viewVm.rows || [], "rows", tileId, 0, dispatchUi));
      break;
    case "table":
      body.append(tableView(viewVm, tileId, dispatchUi));
      break;
    case "sections":
      body.append(sectionsView(viewVm, tileId, dispatchUi));
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
  // Provenance remains inspectable without occupying a full row in every tile.
  footer.hidden = !hints;
  footer.title = provenance;
  footer.append(textEl("small", hints));
  return footer;
}

function contentFooter(content) {
  const footer = el("footer", "ryeos-content-footer");
  footer.append(textEl("div", content.footer));
  for (const row of content.footer_rows || []) {
    const line = el("div", `ryeos-footer-line tone-${row.tone || "neutral"}`);
    line.append(textEl("span", row.field), textEl("small", row.value));
    footer.append(line);
  }
  return footer;
}

function timeline(viewVm) {
  const wrap = el("section", "ryeos-timeline");
  wrap.append(listHeader(viewVm.title || "timeline", ""));
  const entries = el("div", "ryeos-timeline-entries");
  for (const [index, entry] of (viewVm.entries || []).entries()) {
    entries.append(timelineEntry(entry, viewVm.entry_arrived_at_ms?.[index]));
  }
  if (!(viewVm.entries || []).length) entries.append(textEl("p", "No timeline events loaded."));
  wrap.append(entries);
  return wrap;
}

function timelineEntry(entry, arrivedAtMs) {
  let node;
  switch (entry.type) {
    case "block":
      node = textEl("p", entry.text || "", `ryeos-timeline-block ${entry.tone || "neutral"}`);
      break;
    case "pair": {
      node = el("div", `ryeos-timeline-pair ${entry.tone || "neutral"}${entry.pending ? " pending" : ""}`);
      node.append(textEl("span", entry.pending ? "▸" : entry.tone === "danger" ? "✗" : "✓"), textEl("strong", entry.summary || "tool"), textEl("small", entry.meta || ""));
      break;
    }
    case "separator":
      return textEl("div", entry.label || "turn", "ryeos-timeline-separator");
    case "line":
    default: {
      node = el("div", `ryeos-timeline-line ${entry.tone || "neutral"}`);
      node.append(textEl("span", toneGlyph(entry.tone)), textEl("strong", entry.primary || "event"), textEl("small", entry.meta || ""));
      break;
    }
  }
  applyTimedClass(node, "arrived", arrivedAtMs, 1_200);
  return node;
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
    node.disabled = !object.intent;
    node.append(textEl("strong", object.label || object.id), textEl("span", object.kind || "object"));
    if (object.intent) node.addEventListener("click", () => dispatchUi({ type: "activate", intent: object.intent }));
    stage.append(node);
  }
  wrap.append(header, stage);
  return wrap;
}

// A compact, noninteractive vector projection of the existing scene model.
// Geometry and labels are authored data; it knows no worker or project nouns.
function sceneDiagram(scene) {
  const ns = "http://www.w3.org/2000/svg";
  const svg = document.createElementNS(ns, "svg");
  svg.classList.add("ryeos-scene-diagram");
  svg.setAttribute("role", "img");
  const points = (scene.objects || []).flatMap((object) => [object.position, object.end].filter(Boolean));
  const xs = points.map((point) => Number(point[0]));
  const ys = points.map((point) => -Number(point[1]));
  const minX = xs.length ? Math.min(...xs) : 0, maxX = xs.length ? Math.max(...xs) : 1;
  const minY = ys.length ? Math.min(...ys) : 0, maxY = ys.length ? Math.max(...ys) : 1;
  svg.setAttribute("viewBox", `${minX - 20} ${minY - 15} ${maxX - minX + 40} ${maxY - minY + 30}`);
  svg.setAttribute("aria-label", (scene.objects || []).filter((o) => o.label).map((o) => o.label).join("; "));
  for (const object of scene.objects || []) {
    const x = Number(object.position?.[0] || 0), y = -Number(object.position?.[1] || 0);
    const node = document.createElementNS(ns, object.end ? "line" : object.kind === "text" ? "text" : object.glyph === "diamond" ? "polygon" : object.glyph === "square" ? "rect" : "circle");
    const radius = Math.max(1, Number(object.scale?.[0] || 5));
    const attributes = object.end ? {x1:x,y1:y,x2:object.end[0],y2:-object.end[1]}
      : object.kind === "text" ? {x,y}
      : object.glyph === "diamond" ? {points:`${x},${y-radius} ${x+radius},${y} ${x},${y+radius} ${x-radius},${y}`}
      : object.glyph === "square" ? {x:x-radius,y:y-radius,width:radius*2,height:radius*2}
      : {cx:x,cy:y,r:radius};
    for (const [key,value] of Object.entries(attributes)) node.setAttribute(key, String(value));
    node.setAttribute("stroke", object.color || "currentColor");
    node.setAttribute("fill", object.kind === "text" ? object.color || "currentColor" : "var(--ryeos-panel)");
    if (object.kind === "text") { node.setAttribute("stroke", "none"); node.textContent = object.label || ""; }
    svg.append(node);
  }
  return svg;
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
          dispatchUi({ type: "activate", intent: { type: "inspect_item", canonical_ref: item.canonical_ref } });
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

function rows(items, kind, tileId, cursorOffset, dispatchUi) {
  const list = el("div", `ryeos-rows lf ${kind || "rows"}`);
  items.forEach((item, index) => {
    const row = el("button", `ryeos-row ${item.tone || "neutral"}${item.selected ? " selected" : ""}`);
    applyMotion(row, item);
    row.type = "button";
    row.dataset.rowIndex = String(index);
    row.disabled = !item.intent;
    row.append(
      textEl("span", rowGlyph(item, kind), "ryeos-row-glyph"),
      textEl("strong", item.primary),
      textEl("span", item.secondary || ""),
      textEl("small", item.meta || ""),
    );
    if (item.intent) row.addEventListener("click", () => dispatchUi({ type: "activate", intent: item.intent }));
    const wrap = el("div", "ryeos-row-wrap");
    wrap.append(row);
    if (item.expandable) {
      const expand = el("button", "ryeos-row-expand");
      expand.type = "button";
      expand.setAttribute("aria-expanded", String(!!item.expanded));
      expand.setAttribute("aria-label", `${item.expanded ? "Collapse" : "Expand"} ${item.primary}`);
      expand.textContent = item.expanded ? "▾" : "▸";
      expand.addEventListener("click", () => {
        dispatchUi({ type: "set_tile_cursor", tile_id: tileId, index: cursorOffset + index });
        dispatchUi({ type: "expand_selected_row", expand: !item.expanded });
      });
      wrap.append(expand);
    }
    if (item.expanded && (item.detail || []).length) {
      const detail = el("dl", "ryeos-row-detail");
      for (const entry of item.detail) detail.append(textEl("dt", entry.field), textEl("dd", entry.value));
      wrap.append(detail);
    }
    list.append(wrap);
  });
  return list;
}

function rowGlyph(item) {
  if (item.glyph !== undefined && item.glyph !== null) return item.glyph;
  switch (item.tone || "neutral") {
    case "good": return "✓";
    case "warn": return "!";
    case "danger": return "✗";
    case "accent": return "›";
    default: return "•";
  }
}

function applyMotion(node, item) {
  if (item.tone === "accent") node.classList.add("motion-breathe");
  if (item.tone === "warn") node.classList.add("motion-swell");
  if (applyTimedClass(node, "changed", item.changed_at_ms, 1_200)) {
    node.classList.add(`changed-${item.changed_tone || "accent"}`);
  }
}

function applyTimedClass(node, className, timestamp, durationMs) {
  if (timestamp == null) return false;
  const at = Number(timestamp);
  if (!Number.isFinite(at)) return false;
  const age = Math.max(0, Date.now() - at);
  if (age >= durationMs) return false;
  node.classList.add(className);
  // Re-renders replace DOM nodes. Start the replacement at the marker's
  // current age so unrelated commits cannot restart a one-shot animation.
  node.style.animationDelay = `${-age}ms`;
  return true;
}

// The table widget: aligned cells under column headers, a leading tone-glyph
// gutter, full-width selection — the typed list surface for non-chat lenses
// (threads/bundles/schedules). Reference semantics live in the terminal's
// widgets/table.rs: the header row and every body row share column origins,
// the first cell is the identifier (foreground) and later cells are secondary
// detail (muted) unless the whole row is selected. Column count prefers the
// declared headers, else the widest row so cells still align when headers are
// absent.
function tableView(viewVm, tileId, dispatchUi) {
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
  items.forEach((item, index) => grid.append(tableRow(item, ncols, index, tileId, dispatchUi)));
  if (!items.length) grid.append(textEl("p", "No rows loaded.", "ryeos-table-empty"));
  wrap.append(grid);
  return wrap;
}

function tableRow(item, ncols, index, tileId, dispatchUi) {
  const row = el("button", `ryeos-table-row ${item.tone || "neutral"}${item.selected ? " selected" : ""}`);
  applyMotion(row, item);
  row.type = "button";
  row.dataset.rowIndex = String(index);
  row.disabled = !item.intent;
  row.append(textEl("span", rowGlyph(item), "ryeos-table-glyph"));
  const cells = item.cells || [];
  // Per-cell tone overrides (parallel to cells; absent for tables whose
  // columns declare no tone) — a toned cell renders distinctly from the
  // muted secondary default, mirroring the terminal table widget. Neutral
  // means "no override" on both renderers, never a color.
  const cellTones = item.cell_tones || [];
  for (let i = 0; i < ncols; i += 1) {
    const tone = cellTones[i] && cellTones[i] !== "neutral" ? ` tone-${cellTones[i]}` : "";
    const tree = i === 0 ? hierarchyPrefix(item.hierarchy) : "";
    row.append(textEl("span", `${tree}${cells[i] || ""}`, `ryeos-table-cell${i === 0 ? " lead" : ""}${tone}`));
  }
  if (item.intent) row.addEventListener("click", () => dispatchUi({ type: "activate", intent: item.intent }));
  const wrap = el("div", "ryeos-table-row-wrap");
  wrap.append(row);
  if (item.expandable) {
    const expand = el("button", "ryeos-row-expand");
    expand.type = "button";
    expand.setAttribute("aria-expanded", String(!!item.expanded));
    expand.setAttribute("aria-label", `${item.expanded ? "Collapse" : "Expand"} ${(item.cells || [])[0] || "row"}`);
    expand.textContent = item.expanded ? "▾" : "▸";
    expand.addEventListener("click", () => {
      dispatchUi({ type: "set_tile_cursor", tile_id: tileId, index });
      dispatchUi({ type: "expand_selected_row", expand: !item.expanded });
    });
    wrap.append(expand);
  }
  if (item.expanded && (item.detail || []).length) {
    const detail = el("dl", "ryeos-row-detail");
    for (const entry of item.detail) detail.append(textEl("dt", entry.field), textEl("dd", entry.value));
    wrap.append(detail);
  }
  return wrap;
}

function hierarchyPrefix(hierarchy) {
  if (!hierarchy) return "";
  const ancestors = hierarchy.ancestor_continues || [];
  let prefix = "";
  for (let i = 0; i + 1 < ancestors.length; i += 1) {
    prefix += ancestors[i] ? "│ " : "  ";
  }
  if (ancestors.length) prefix += hierarchy.is_last ? "└─" : "├─";
  if (hierarchy.has_children) prefix += hierarchy.collapsed ? "▸ " : "▾ ";
  else prefix += "· ";
  return prefix;
}

// The sections widget: a foldable multi-section list (the magit-style status
// surface). Each section is a `▾/▸ Title (count)` header followed by its rows,
// indented; a collapsed section shows only its header, and its `count` still
// reflects the hidden rows. Reference semantics live in the terminal's
// widgets/sections.rs. Rows reuse the rows-widget renderer (RyeOsRowVm), so
// tone glyph, primary/secondary/meta, and per-row intents come for free.
function sectionsView(viewVm, tileId, dispatchUi) {
  const wrap = el("section", "ryeos-sections");
  wrap.append(listHeader(viewVm.title || "sections", ""));
  const body = el("div", "ryeos-sections-body");
  const sections = viewVm.sections || [];
  let cursorOffset = 0;
  for (const [index, section] of sections.entries()) {
    body.append(sectionGroup(section, index, tileId, cursorOffset, dispatchUi));
    cursorOffset += section.collapsed ? 1 : (section.rows || []).length;
  }
  if (!sections.length) body.append(textEl("p", "No sections loaded.", "ryeos-sections-empty"));
  wrap.append(body);
  return wrap;
}

function sectionGroup(section, sectionIndex, tileId, cursorOffset, dispatchUi) {
  const collapsed = !!section.collapsed;
  const group = el("div", `ryeos-section${collapsed ? " collapsed" : ""}`);
  // The header is the point that re-expands a collapsed section; when it
  // carries the cursor, highlight the full line like a selected row.
  const header = el("button", `ryeos-section-header${section.header_selected ? " selected" : ""}`);
  header.type = "button";
  header.setAttribute("aria-expanded", String(!collapsed));
  const count = section.count ?? (section.rows || []).length;
  header.append(
    textEl("span", collapsed ? "▸" : "▾", "ryeos-section-fold"),
    textEl("strong", section.title || "section"),
    textEl("span", `(${count})`, "ryeos-section-count"),
  );
  header.addEventListener("click", () => dispatchUi({
    type: "set_fold",
    tile_id: tileId,
    section: sectionIndex,
    collapsed: !collapsed,
  }));
  group.append(header);
  if (!collapsed) group.append(rows(section.rows || [], "section", tileId, cursorOffset, dispatchUi));
  return group;
}
