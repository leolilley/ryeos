import { mountStudioAmbientScene } from "/ui/assets/studio_ambient_scene.js";
import { el, textEl } from "/ui/assets/studio_components_primitives.js";

let ambientCanvas = null;
let ambientScene = null;
let homeShell = null;
let homeField = null;
let homeLanding = null;
let homeLandingSignature = "";
let atlasInspector = null;
let atlasInspectorSignature = "";
let atlasFocus = null;
let latestAmbient = {};
let atlasPanelVisible = false;
let atlasHoverCard = null;
let latestShell = null;
let typerTimer = null;
let typerLineIndex = 0;
let typerTarget = null;
let typerLinesSignature = "";
let transientTopbarUntil = 0;

const FALLBACK_TYPER_LINES = [
  "hashes for truth. signatures for agency. attestations for proof.",
  "content-addressed. tamper-evident. verified by math.",
  "identity is a keypair. trust is a pin. authority is local.",
  "every item carries a chain of custody. every node verifies it.",
  "descriptors are trust pins, not credentials.",
  "wildcards rejected. capabilities attenuate. no escalation.",
  "the CAS is the commitment. the attestation is the proof.",
  "admission is proof of possession. not proof of account.",
  "two nodes, zero prior relationship, shared verified state.",
  "swap the model. swap the machine. the signatures hold.",
  "no central authority. no bearer tokens. no provider in the loop.",
  "closure complete, hashes verified, attestation signed.",
  "staged. mirrored. accepted. every byte accounted for.",
  "the hosting provider runs dns. the node runs authority.",
  "convergence without consensus. trust without coordination.",
];

export function studioHome(vm, scene, shell) {
  const isHome = vm.workspace?.is_home !== false;
  const home = homeShell || el("section", "studio-home");
  const ambient = vm.session?.ambient || {};
  const namespaceAtlas = isNamespaceAtlasAmbient(ambient);
  latestShell = shell;
  latestAmbient = ambient;
  homeShell = home;
  if (!home.dataset.initialized) {
    home.setAttribute("aria-label", "RyeOS home space");
    homeField = el("div", "studio-home-field");
    home.append(ambientBackground(scene, ambient), homeField);
    home.dataset.initialized = "true";
  } else {
    ambientBackground(scene, ambient);
  }
  home.classList.toggle("ambient-hidden", ambient.show_background === false);
  home.classList.toggle("ambient-atlas-2d", namespaceAtlas && atlasStyle(ambient) === "flat_2d");
  home.classList.toggle("atlas-panel-visible", atlasPanelVisible);
  home.style.setProperty("--ambient-opacity", String(ambient.opacity ?? 1));
  home.style.setProperty("--scene-object-count", String(scene?.objects?.length || 0));
  if (!isHome) {
    home.classList.add("backdrop-only");
    home.setAttribute("aria-hidden", "true");
    if (homeLanding) homeLanding.hidden = true;
  } else {
    home.classList.remove("backdrop-only");
    home.removeAttribute("aria-hidden");
    const landing = homeLandingView(vm, shell);
    landing.hidden = false;
    if (!landing.parentNode) home.append(landing);
    if (namespaceAtlas && atlasFocus?.pinned) {
      const inspector = atlasInspectorView(scene);
      inspector.hidden = false;
      if (!inspector.parentNode) home.append(inspector);
    } else if (atlasInspector) {
      atlasInspector.hidden = true;
      setAtlasFocus(null, scene, ambient);
    }
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
    const marker = el("span", `studio-home-node ${object.kind || "object"} ${object.tone || "neutral"}`);
    marker.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    marker.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    marker.style.setProperty("--node-color", object.color || "#fabd2f");
    marker.title = object.label || object.id || "node";
    field.append(marker);
  }
}

function homeLandingView(vm, shell) {
  const identity = el("div", "studio-home-identity");
  const homeVm = vm.presentation?.home || {};
  const signature = JSON.stringify({
    home: homeVm,
    version: vm.presentation?.chrome?.version_label || "",
    readOnly: vm.session?.read_only || false,
    project: vm.session?.project_path || "",
  });
  if (homeLanding && signature === homeLandingSignature) return homeLanding;
  homeLandingSignature = signature;
  const landing = el("div", "studio-home-landing");
  identity.append(
    textEl("div", homeVm.brand || "RYE OS"),
    textEl("small", homeVm.tagline || "portable operating system for ai"),
    el("i", "studio-home-line"),
    textEl("p", homeVm.description || "Persistent, signed AI substrate that travels with you across spaces, machines, and models."),
    typerLine(homeVm.terminal_lines),
    heroCta(shell, homeVm),
  );

  const version = textEl("div", vm.presentation?.chrome?.version_label || `RYE OS - ${ryeosVersion(shell)}`);
  version.className = "studio-home-version";
  landing.append(identity, version);
  if (homeLanding?.parentNode) homeLanding.replaceWith(landing);
  homeLanding = landing;
  return landing;
}

export function opticFrame(frame = {}) {
  const node = el("div", `studio-optic-frame ${frame.mode || "home"}`);
  node.setAttribute("aria-hidden", "true");
  if (frame.corners?.visible !== false) {
    for (const corner of ["tl", "tr", "bl", "br"]) node.append(el("i", `studio-corner ${corner}`));
  }
  return node;
}

export function topStatusLine(vm, shell) {
  const line = el("header", "studio-topbar");
  const top = vm.presentation?.chrome?.top_bar || {};
  const tabChanged = (vm.presentation?.motion || []).find((motion) => motion.type === "tab_changed");
  if (!top.visible && tabChanged?.workspace_number) {
    transientTopbarUntil = Date.now() + 1050;
  }
  const transient = !top.visible && Date.now() < transientTopbarUntil;
  line.classList.toggle("hidden", !top.visible);
  line.classList.toggle("transient", transient);
  const tabs = el("nav", "studio-workspace-tabs");
  tabs.setAttribute("aria-label", "RyeOS workspaces");
  for (const tab of top.tabs || []) {
    const button = textEl("button", String(tab.number), tab.active ? "active" : "");
    button.type = "button";
    button.title = `workspace ${tab.number} · ${tab.tile_count || 0} tiles`;
    button.addEventListener("click", () => shell?.dispatchUi?.({
      type: "activate",
      action: { type: "switch_tab", index: Math.max(0, (tab.number || 1) - 1) },
    }));
    tabs.append(button);
  }
  line.append(tabs);
  line.append(textEl("span", top.focused_title || "home", "focused-title"));
  line.append(textEl("span", top.layout_symbol || "M1│S0", "layout-symbol"));
  return line;
}

export function statusLine(vm, shell) {
  const line = el("footer", "studio-statusbar");
  const status = vm.presentation?.chrome?.status_bar;
  line.classList.toggle("hidden", status?.visible === false);
  const segments = status?.segments || [];
  if (segments.length === 0) {
    const mode = vm.session?.read_only ? "ro" : "rw";
    const health = vm.chrome?.health_label || "connecting";
    const version = ryeosVersion(shell);
    const project = vm.session?.project_path || shell?.dimension?.project?.path || "home";
    line.append(
      textEl("strong", "rye os"),
      textEl("span", `v${version}`),
      textEl("span", health, `tone-${vm.chrome?.health_tone || "neutral"}`),
      textEl("span", mode),
      textEl("span", project, "grow"),
      textEl("span", "alt+k open · alt+t/b bars · ctrl+←/→ tab · ctrl+↑/↓ move", "keys"),
    );
    return line;
  }
  for (const segment of segments) {
    const tag = segment.id === "brand" ? "strong" : "span";
    const classes = [`tone-${segment.tone || "neutral"}`];
    if (segment.grow) classes.push("grow");
    const value = segment.label ? `${segment.label} ${segment.value}` : segment.value;
    line.append(textEl(tag, value, classes.join(" ")));
  }
  appendCompatMetrics(line, vm, segments);
  if (status?.key_hint) line.append(textEl("span", status.key_hint, "keys"));
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
  const key = `${mode}:${style}`;
  const options = { mode, atlasStyle: style, atlasFocus };
  if (!ambientCanvas) {
    ambientCanvas = document.createElement("canvas");
    ambientCanvas.className = "studio-ambient-canvas";
    ambientCanvas.setAttribute("aria-hidden", "true");
    ambientCanvas.dataset.ambientKey = key;
    bindAtlasCanvasEvents(ambientCanvas);
    ambientScene = mountStudioAmbientScene(ambientCanvas, scene, options);
  } else {
    if (ambientCanvas.dataset.ambientKey !== key) {
      ambientScene?.dispose?.();
      ambientCanvas.dataset.ambientKey = key;
      bindAtlasCanvasEvents(ambientCanvas);
      ambientScene = mountStudioAmbientScene(ambientCanvas, scene, options);
    } else {
      ambientScene?.update(scene, options);
    }
  }
  return ambientCanvas;
}

function bindAtlasCanvasEvents(canvas) {
  if (canvas.dataset.atlasEventsBound === "true") return;
  canvas.addEventListener("studio-atlas-hover", onAtlasCanvasHover);
  canvas.addEventListener("studio-atlas-navigate", onAtlasNavigate);
  canvas.addEventListener("studio-atlas-select", onAtlasSelect);
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

  const panel = el("aside", "studio-atlas-inspector");
  panel.append(
    textEl("div", "ATLAS", "studio-atlas-kicker"),
    textEl("h2", "inspect namespace"),
    textEl("p", "Hover or select folders, kinds, and items to highlight the related atlas shapes.", "studio-atlas-help"),
  );
  const clear = textEl("button", "CLEAR", "studio-atlas-chip clear");
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
  const group = el("section", "studio-atlas-group");
  group.append(textEl("h3", title));
  if (!entries.length) {
    group.append(textEl("span", "none", "studio-atlas-empty"));
    return group;
  }
  for (const entry of entries) {
    const row = el("button", "studio-atlas-row");
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
        action: { type: "inspect_item", canonical_ref: interaction.canonical_ref },
      });
      return true;
    case "read_file":
      latestShell.dispatchUi({
        type: "activate",
        action: { type: "read_file", root: interaction.root, path: interaction.path },
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
    atlasHoverCard = el("div", "studio-atlas-hover-card");
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

function typerLine(lines = FALLBACK_TYPER_LINES) {
  const choices = Array.isArray(lines) && lines.length > 0 ? lines : FALLBACK_TYPER_LINES;
  const signature = JSON.stringify(choices);
  if (typerTarget?.isConnected && typerLinesSignature === signature) {
    return typerTarget.closest(".studio-home-typer");
  }
  const line = el("div", "studio-home-typer");
  const text = textEl("span", "");
  text.className = "typer-text";
  line.append(textEl("span", "> ", "typer-cursor"), text, el("span", "typer-caret"));
  typerTarget = text;
  typerLinesSignature = signature;
  window.queueMicrotask(() => startTypewriter(text, choices));
  return line;
}

function startTypewriter(target, lines) {
  window.clearTimeout(typerTimer);
  const choices = Array.isArray(lines) && lines.length > 0 ? lines : FALLBACK_TYPER_LINES;
  const typeCurrentLine = () => {
    if (!target.isConnected) return;
    const value = choices[typerLineIndex % choices.length];
    let index = 0;
    target.textContent = "";
    const typeChar = () => {
      if (!target.isConnected) return;
      if (index < value.length) {
        target.textContent += value[index];
        index += 1;
        typerTimer = window.setTimeout(typeChar, 40 + Math.random() * 30);
      } else {
        typerTimer = window.setTimeout(eraseLine, 2400);
      }
    };
    const eraseLine = () => {
      if (!target.isConnected) return;
      if (target.textContent.length > 0) {
        target.textContent = target.textContent.slice(0, -1);
        typerTimer = window.setTimeout(eraseLine, 20);
      } else {
        typerLineIndex = (typerLineIndex + 1) % choices.length;
        typerTimer = window.setTimeout(typeCurrentLine, 400);
      }
    };
    typeChar();
  };
  typeCurrentLine();
}

function heroCta(shell, homeVm = {}) {
  const cta = el("div", "studio-home-cta");
  const actions = el("div", "studio-home-actions");
  const primary = el("button", "studio-home-btn primary");
  primary.type = "button";
  primary.textContent = homeVm.primary_label || "OPEN";
  primary.addEventListener("click", () => shell.openLauncher?.());
  const secondary = el("a", "studio-home-btn secondary");
  secondary.href = homeVm.secondary_url || "https://github.com/leolilley/ryeos";
  secondary.target = "_blank";
  secondary.rel = "noreferrer";
  secondary.textContent = homeVm.secondary_label || "GITHUB";
  actions.append(primary, secondary);

  const install = el("button", "studio-install-card");
  install.type = "button";
  install.append(
    textEl("span", "$", "prompt"),
    textEl("span", homeVm.install_command || "pip install ryeos-mcp"),
    textEl("span", "CLICK TO COPY", "copy-hint"),
  );
  install.addEventListener("click", async () => {
    await navigator.clipboard?.writeText?.(homeVm.install_command || "pip install ryeos-mcp");
    install.classList.add("copied");
    const hint = install.querySelector(".copy-hint");
    if (hint) hint.textContent = "COPIED ✓";
    window.setTimeout(() => {
      install.classList.remove("copied");
      if (hint) hint.textContent = "CLICK TO COPY";
    }, 1600);
  });
  cta.append(actions, install);
  return cta;
}
