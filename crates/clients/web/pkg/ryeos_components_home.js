import { mountRyeOsAmbientScene } from "/ui/assets/ryeos_ambient_scene.js";
import { el, textEl } from "/ui/assets/ryeos_components_primitives.js";
import { droppedView, VIEW_DRAG_TYPE } from "/ui/assets/ryeos_components_workspace.js";

let ambientCanvas = null;
let ambientScene = null;
let homeShell = null;
let homeField = null;
let atlasInspector = null;
let atlasInspectorSignature = "";
let atlasFocus = null;
let latestAmbient = {};
let atlasPanelVisible = false;
let atlasHoverCard = null;
let latestShell = null;
let transientTopbarUntil = 0;

// The always-on ambient layer (style.md: ambient sits behind content,
// never blocking input). There is no "home" mode anymore — this renders
// the surface-declared ambient (the Three.js / 2D atlas topology scene)
// behind everything in every state. The deleted home brand/welcome/typer
// block does NOT render here; the empty-center backdrop is content drawn
// by the generic scene renderer in the workspace plane, not here.
export function ryeosHome(vm, scene, shell) {
  const home = homeShell || el("section", "ryeos-home");
  const ambient = vm.session?.ambient || {};
  const namespaceAtlas = isNamespaceAtlasAmbient(ambient);
  latestShell = shell;
  latestAmbient = ambient;
  homeShell = home;
  if (!home.dataset.initialized) {
    home.setAttribute("aria-label", "RyeOS ambient layer");
    home.setAttribute("aria-hidden", "true");
    homeField = el("div", "ryeos-home-field");
    home.append(ambientBackground(scene, ambient), homeField);
    home.dataset.initialized = "true";
  } else {
    ambientBackground(scene, ambient);
  }
  home.classList.add("backdrop-only");
  home.classList.toggle("ambient-hidden", ambient.show_background === false);
  home.classList.toggle("ambient-atlas-2d", namespaceAtlas && atlasStyle(ambient) === "flat_2d");
  home.classList.toggle("atlas-panel-visible", atlasPanelVisible);
  home.style.setProperty("--ambient-opacity", String(ambient.opacity ?? 1));
  home.style.setProperty("--scene-object-count", String(scene?.objects?.length || 0));
  if (namespaceAtlas && atlasFocus?.pinned) {
    const inspector = atlasInspectorView(scene);
    inspector.hidden = false;
    if (!inspector.parentNode) home.append(inspector);
  } else if (atlasInspector) {
    atlasInspector.hidden = true;
    setAtlasFocus(null, scene, ambient);
  }
  updateObjectField(scene, ambient);
  return home;
}

function ambientBackground(scene, ambient) {
  return ambientLayer(scene, ambient);
}

function updateObjectField(scene, ambient = {}) {
  const field = homeField;
  if (!field) return;
  field.replaceChildren();
  if (isNamespaceAtlasAmbient(ambient)) return;
  for (const object of scene?.objects || []) {
    const marker = el("span", `ryeos-home-node ${object.kind || "object"} ${object.tone || "neutral"}`);
    marker.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    marker.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    marker.style.setProperty("--node-color", object.color || "#fabd2f");
    marker.title = object.label || object.id || "node";
    field.append(marker);
  }
}

export function opticFrame(frame = {}) {
  // No frame mode anymore: the optic frame is the corner-mark accent in
  // every state. Tone comes from the frame VM; the class is static.
  const node = el("div", `ryeos-optic-frame ${frame.corners?.tone || "accent"}`);
  node.setAttribute("aria-hidden", "true");
  if (frame.corners?.visible !== false) {
    for (const corner of ["tl", "tr", "bl", "br"]) node.append(el("i", `ryeos-corner ${corner}`));
  }
  return node;
}

export function topStatusLine(vm, shell) {
  const line = el("header", "ryeos-topbar");
  const top = vm.presentation?.chrome?.top_bar || {};
  const tabChanged = (vm.presentation?.motion || []).find((motion) => motion.type === "tab_changed");
  if (!top.visible && tabChanged?.workspace_number) {
    transientTopbarUntil = Date.now() + 1050;
  }
  const transient = !top.visible && Date.now() < transientTopbarUntil;
  line.classList.toggle("hidden", !top.visible);
  line.classList.toggle("transient", transient);
  const system = el("div", "ryeos-systembar");
  const launch = () => shell?.dispatchUi?.({ type: "open_overlay", overlay_id: "views" });
  const brand = el("button", "ryeos-brand-launch");
  // Reuse the approved vector mark; never load a runtime font or icon CDN.
  const mark = document.createElementNS("http://www.w3.org/2000/svg", "svg");
  mark.setAttribute("viewBox", "0 0 28 28");
  mark.setAttribute("aria-hidden", "true");
  const path = document.createElementNS(mark.namespaceURI, "path");
  path.setAttribute("d", "M4 22V6h10l9 8-9 8H4Zm0-8h19M14 6v16");
  mark.append(path);
  const wordmark = textEl("span", "rye", "ryeos-wordmark");
  wordmark.append(textEl("span", "os"));
  brand.append(mark, wordmark, textEl("small", "⌄"));
  brand.type = "button";
  brand.setAttribute("aria-label", "Open RyeOS launcher");
  brand.addEventListener("click", launch);
  system.append(brand, textEl("span", vm.session?.user_principal_id || "Session", "ryeos-system-principal"),
    textEl("span", vm.session?.project_path || "Node workspace", "ryeos-system-context"));
  const health = textEl("span", vm.chrome?.health_label || "", `ryeos-system-health tone-${vm.chrome?.health_tone || "neutral"}`);
  const launcher = textEl("button", "Launch", "ryeos-launch-control");
  launcher.type = "button";
  launcher.append(textEl("kbd", "Ctrl K"));
  launcher.addEventListener("click", launch);
  system.append(health, launcher);
  const strip = el("div", "ryeos-workspace-strip");
  const tabs = el("nav", "ryeos-workspace-tabs");
  tabs.setAttribute("aria-label", "RyeOS workspaces");
  for (const tab of top.tabs || []) {
    const button = textEl("button", tab.title || String(tab.number), tab.active ? "active" : "");
    const index = textEl("small", String(tab.number).padStart(2, "0"), "ryeos-workspace-index");
    index.setAttribute("aria-hidden", "true");
    button.prepend(index);
    if (tab.active) button.append(el("i", "ryeos-workspace-active-mark"));
    button.type = "button";
    button.title = `workspace ${tab.number} · ${tab.tile_count || 0} tiles`;
    button.addEventListener("click", () => shell?.dispatchUi?.({
      type: "activate",
      intent: { type: "select_workspace", workspace_id: tab.workspace_id },
    }));
    // Rename is local presentation editing, not a command or a new route.
    // Keep the exact identity captured by this render even if another tab closes.
    const rename = () => {
      if (!button.isConnected) return;
      const editor = el("input", "ryeos-workspace-name");
      editor.value = tab.title;
      editor.setAttribute("aria-label", "Workspace name");
      let finished = false;
      const finish = (save) => {
        if (finished) return;
        finished = true;
        editor.replaceWith(button);
        button.focus();
        if (save) shell?.dispatchUi?.({ type: "activate", intent: {
          type: "rename_workspace", workspace_id: tab.workspace_id, title: editor.value,
        } });
      };
      editor.addEventListener("keydown", (event) => {
        // Do not let workspace keyboard shortcuts consume name entry.
        event.stopPropagation();
        if (event.key === "Enter" || event.key === "Escape") {
          event.preventDefault();
          finish(event.key === "Enter");
        }
      });
      editor.addEventListener("blur", () => finish(false));
      button.replaceWith(editor);
      editor.focus();
      editor.select();
    };
    button.addEventListener("keydown", (event) => {
      if (event.key === "F2") { event.preventDefault(); event.stopPropagation(); rename(); }
    });
    button.addEventListener("dblclick", rename);
    button.title += " · F2 to rename";
    button.addEventListener("dragover", (event) => {
      if (event.dataTransfer?.types.includes(VIEW_DRAG_TYPE)) event.preventDefault();
    });
    button.addEventListener("drop", (event) => {
      const source = droppedView(event, { guard: vm.workspace?.layout_guard });
      if (!source) return;
      event.preventDefault();
      shell?.dispatchUi?.({ type: "activate", intent: {
        type: "move_tile_to_workspace", layout_guard: source.guard,
        tile_id: source.tile_id, workspace_id: tab.workspace_id,
      } });
    });
    tabs.append(button);
    if (tab.active && top.tabs.length > 1) {
      const close = textEl("button", "×", "ryeos-workspace-close");
      close.type = "button";
      close.setAttribute("aria-label", `Close workspace ${tab.title}`);
      close.title = "Close this arrangement; running work is not stopped";
      close.addEventListener("click", () => shell?.dispatchUi?.({ type: "activate", intent: {
        type: "close_workspace", workspace_id: tab.workspace_id,
      } }));
      tabs.append(close);
    }
  }
  const add = textEl("button", "+");
  add.type = "button";
  add.setAttribute("aria-label", "New workspace");
  add.addEventListener("click", () => shell?.dispatchUi?.({ type: "activate", intent: { type: "new_workspace" } }));
  tabs.append(add);
  strip.append(tabs);
  strip.append(textEl("span", top.focused_title || "", "focused-title"));
  const views = textEl("button", "⊞", "ryeos-workspace-launch");
  views.type = "button";
  views.setAttribute("aria-label", "Open views and arrangements");
  views.addEventListener("click", launch);
  strip.append(views);
  line.append(system, strip);
  return line;
}

export function statusLine(vm, shell) {
  const line = el("footer", "ryeos-statusbar");
  const status = vm.presentation?.chrome?.status_bar;
  line.classList.toggle("hidden", status?.visible === false);
  const segments = status?.segments || [];
  if (segments.length === 0) {
    const posture = vm.session?.posture || "observation_only";
    const health = vm.chrome?.health_label || "connecting";
    const version = ryeosVersion(shell);
    const project = vm.session?.project_path || shell?.dimension?.project?.path || "home";
    line.append(
      textEl("strong", "rye os"),
      textEl("span", `v${version}`),
      textEl("span", health, `tone-${vm.chrome?.health_tone || "neutral"}`),
      textEl("span", posture),
      textEl("span", project, "grow"),
      textEl("span", "ctrl+k open · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move", "keys"),
    );
    return line;
  }
  for (const segment of segments) {
    const tag = segment.id === "brand" ? "strong" : "span";
    const classes = [`tone-${segment.tone || "neutral"}`];
    if (segment.grow) classes.push("grow");
    const value = segment.label ? `${segment.label} ${segment.value}` : segment.value;
    const item = textEl(tag, value, classes.join(" "));
    item.dataset.segment = segment.id;
    line.append(item);
  }
  const details = el("details", "ryeos-status-details");
  details.append(textEl("summary", "Session details"));
  const inventory = el("div", "ryeos-status-inventory");
  for (const item of [...line.children]) {
    if (!["health", "project"].includes(item.dataset.segment)) inventory.append(item);
  }
  if (status?.key_hint) inventory.append(textEl("span", status.key_hint, "keys"));
  details.append(inventory);
  const active = vm.presentation?.chrome?.top_bar?.tabs?.find((tab) => tab.active);
  if (active) line.append(textEl("span", `${active.title} · ${active.tile_count} views`, "ryeos-status-workspace"));
  line.append(details);
  return line;
}

function appendCompatMetrics(line, vm, segments) {
  const seen = new Set(segments.map((segment) => segment.id));
  const metrics = vm.presentation?.metrics || {};
  const values = [
    ["tiles", metrics.tile_count ?? vm.workspace?.tile_count],
    ["items", metrics.item_count],
    ["threads", metrics.thread_count],
  ];
  for (const [label, value] of values) {
    if (seen.has(label) || value === undefined || value === null) continue;
    line.append(textEl("span", `${label} ${value}`, "tone-neutral"));
  }
}

function ryeosVersion(shell) {
  return (shell?.dimension?.local_node?.status?.version || "0.1.0").replace(/^ryeosd-/, "");
}

function ambientLayer(scene, ambient = {}) {
  const mode = ambientSceneFamily(ambient);
  const style = atlasStyle(ambient);
  // Ambient browser mechanics belong to the shell adapter, not the Rust
  // semantic model. The optional key makes a changed adapter an explicit
  // graphics-runtime generation boundary (used by deterministic browser
  // qualification); ordinary production shells omit both values.
  const platformKey = latestShell?.ambientPlatformKey || "browser";
  const key = `${mode}:${style}:${platformKey}`;
  const options = {
    mode,
    atlasStyle: style,
    atlasFocus,
    platform: latestShell?.ambientPlatform,
  };
  if (!ambientCanvas) {
    ambientCanvas = document.createElement("canvas");
    ambientCanvas.className = "ryeos-ambient-canvas";
    ambientCanvas.setAttribute("aria-hidden", "true");
    ambientCanvas.dataset.ambientKey = key;
    bindAtlasCanvasEvents(ambientCanvas);
    ambientScene = mountRyeOsAmbientScene(ambientCanvas, scene, options);
  } else {
    if (ambientCanvas.dataset.ambientKey !== key) {
      ambientScene?.dispose?.();
      ambientCanvas.dataset.ambientKey = key;
      bindAtlasCanvasEvents(ambientCanvas);
      ambientScene = mountRyeOsAmbientScene(ambientCanvas, scene, options);
    } else {
      ambientScene?.update(scene, options);
    }
  }
  return ambientCanvas;
}

function bindAtlasCanvasEvents(canvas) {
  if (canvas.dataset.atlasEventsBound === "true") return;
  canvas.addEventListener("ryeos-atlas-hover", onAtlasCanvasHover);
  canvas.addEventListener("ryeos-atlas-navigate", onAtlasNavigate);
  canvas.addEventListener("ryeos-atlas-select", onAtlasSelect);
  canvas.dataset.atlasEventsBound = "true";
}

function atlasInspectorView(scene = {}) {
  const atlas = scene?.atlas || {};
  const nodes = atlas.nodes || [];
  const items = nodes.flatMap((node) => (node.stack || []).map((item) => ({ ...item, folder: node.namespace_key || node.label || "root" })));
  const roots = nodes.filter((node) => (node.path?.length || 0) === 1);
  const kinds = [...new Set(items.map((item) => item.kind || "other"))].sort();
  const signature = JSON.stringify({
    generation: atlas.generation,
    selected: atlas.selected_ref,
    roots: roots.map((node) => [node.id, node.namespace_key, node.stack?.length || 0]),
    kinds,
    items: items.slice(0, 24).map((item) => item.canonical_ref || item.label),
  });
  if (atlasInspector && atlasInspectorSignature === signature) return atlasInspector;
  atlasInspectorSignature = signature;

  const panel = el("aside", "ryeos-atlas-inspector");
  panel.append(
    textEl("div", "ATLAS", "ryeos-atlas-kicker"),
    textEl("h2", "inspect namespace"),
    textEl("p", "Hover or select folders, kinds, and items to highlight the related atlas shapes.", "ryeos-atlas-help"),
  );
  const clear = textEl("button", "CLEAR", "ryeos-atlas-chip clear");
  clear.type = "button";
  clear.addEventListener("click", () => setAtlasFocus(null, scene));
  panel.append(clear);
  panel.append(atlasGroup("root folders", roots.slice(0, 10).map((node) => ({
    label: node.label || node.namespace_key || "root",
    meta: `${node.stack?.length || 0} items`,
    focus: { type: "folder", id: node.id, path: node.namespace_key || "" },
  })), scene));
  panel.append(atlasGroup("kinds", kinds.map((kind) => ({
    label: kind,
    meta: String(items.filter((item) => (item.kind || "other") === kind).length),
    focus: { type: "kind", kind },
  })), scene));
  panel.append(atlasGroup("items", items.slice(0, 14).map((item) => ({
    label: item.label || item.canonical_ref || "item",
    meta: item.folder,
    focus: { type: "item", ref: atlasItemRef(item), kind: item.kind || "other" },
  })), scene));

  if (atlasInspector?.parentNode) atlasInspector.replaceWith(panel);
  atlasInspector = panel;
  return panel;
}

function atlasItemRef(item = {}) {
  return item.canonical_ref || item.id || item.label || "";
}

function atlasGroup(title, entries, scene) {
  const group = el("section", "ryeos-atlas-group");
  group.append(textEl("h3", title));
  if (!entries.length) {
    group.append(textEl("span", "none", "ryeos-atlas-empty"));
    return group;
  }
  for (const entry of entries) {
    const row = el("button", "ryeos-atlas-row");
    row.type = "button";
    row.classList.toggle("active", atlasFocusMatches(entry.focus));
    row.append(textEl("span", entry.label), textEl("small", entry.meta || ""));
    row.addEventListener("mouseenter", () => setAtlasFocus(atlasInspectorFocus(entry.focus), scene));
    row.addEventListener("focus", () => setAtlasFocus(atlasInspectorFocus(entry.focus), scene));
    row.addEventListener("click", () => setAtlasFocus({ ...entry.focus, pinned: true }, scene));
    group.append(row);
  }
  group.addEventListener("mouseleave", () => {
    if (!atlasFocus?.pinned) setAtlasFocus(null, scene);
  });
  return group;
}

function atlasInspectorFocus(focus) {
  return atlasFocus?.pinned ? { ...focus, pinned: true } : focus;
}

function setAtlasFocus(focus, scene, ambient = {}) {
  atlasFocus = focus;
  const currentAmbient = Object.keys(ambient).length ? ambient : latestAmbient;
  ambientScene?.update?.(scene, { mode: ambientSceneFamily(currentAmbient), atlasStyle: atlasStyle(currentAmbient), atlasFocus });
  atlasInspectorSignature = "";
  if (homeShell) {
    homeShell.classList.toggle("atlas-inspector-visible", Boolean(atlasFocus?.pinned));
    if (atlasFocus?.pinned) {
      const inspector = atlasInspector?.parentNode ? atlasInspector : atlasInspectorView(scene);
      inspector.hidden = false;
      if (!inspector.parentNode) homeShell.append(inspector);
    } else if (atlasInspector) {
      atlasInspector.hidden = true;
    }
  }
}

function atlasFocusMatches(focus) {
  if (!atlasFocus || !focus) return !atlasFocus && !focus;
  const { pinned: _focusPinned, ...current } = atlasFocus;
  const { pinned: _entryPinned, ...entry } = focus;
  return JSON.stringify(current) === JSON.stringify(entry);
}

function onAtlasCanvasHover(event) {
  const detail = event.detail || null;
  updateAtlasHoverCard(detail);
}

function onAtlasNavigate() {
  atlasPanelVisible = true;
  homeShell?.classList.add("atlas-panel-visible");
}

function onAtlasSelect(event) {
  const detail = event.detail || null;
  if (!detail?.id) return;
  if (dispatchAtlasInteraction(detail.interaction)) return;
  setAtlasFocus({
    type: detail.kind?.startsWith("atlas_") ? "folder" : "item",
    id: detail.id,
    ref: detail.id,
    kind: detail.kind,
    path: detail.path || "",
    pinned: true,
  }, event.detail?.sceneModel || {});
}

function dispatchAtlasInteraction(interaction) {
  if (!interaction || !latestShell?.dispatchUi) return false;
  switch (interaction.type) {
    case "inspect_item":
      latestShell.dispatchUi({
        type: "activate",
        intent: { type: "inspect_item", canonical_ref: interaction.canonical_ref },
      });
      return true;
    case "read_file":
      latestShell.dispatchUi({
        type: "activate",
        intent: { type: "read_file", root: interaction.root, path: interaction.path },
      });
      return true;
    case "focus_folder":
      if (interaction.root) {
        latestShell.dispatchUi({
          type: "set_atlas_file_space_path",
          root: interaction.root,
          path: interaction.path || "",
        });
        return true;
      }
      return false;
    default:
      return false;
  }
}

function updateAtlasHoverCard(detail) {
  if (!homeShell) return;
  if (!detail?.id) {
    atlasHoverCard?.remove();
    atlasHoverCard = null;
    return;
  }
  if (!atlasHoverCard) {
    atlasHoverCard = el("div", "ryeos-atlas-hover-card");
    homeShell.append(atlasHoverCard);
  }
  atlasHoverCard.replaceChildren(
    textEl("strong", detail.label || detail.id),
    textEl("span", detail.kind || "atlas item"),
  );
}

function ambientSceneFamily(ambient = {}) {
  return isNamespaceAtlasAmbient(ambient) ? "namespace_atlas" : "ambient";
}

function isNamespaceAtlasAmbient(ambient = {}) {
  return ambient.mode === "namespace_atlas" || ambient.mode === "atlas_2d" || ambient.mode === "atlas_paper_3d";
}

function atlasStyle(ambient = {}) {
  if (ambient.atlas?.style) return ambient.atlas.style;
  if (ambient.mode === "atlas_paper_3d") return "paper_3d";
  if (ambient.mode === "namespace_atlas" || ambient.mode === "atlas_2d") return "flat_2d";
  return "flat_2d";
}
