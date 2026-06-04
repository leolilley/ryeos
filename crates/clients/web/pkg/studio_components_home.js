import { mountStudioAmbientScene } from "/ui/assets/studio_ambient_scene.js";
import { el, textEl } from "/ui/assets/studio_components_primitives.js";

let ambientCanvas = null;
let ambientScene = null;
let homeShell = null;
let homeField = null;
let homeLanding = null;
let homeLandingSignature = "";
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
  homeShell = home;
  if (!home.dataset.initialized) {
    home.setAttribute("aria-label", "RyeOS home space");
    homeField = el("div", "studio-home-field");
    home.append(ambientBackground(scene), homeField);
    home.dataset.initialized = "true";
  } else {
    ambientBackground(scene);
  }
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
  }
  updateObjectField(scene);
  return home;
}

function ambientBackground(scene) {
  return ambientLayer(scene);
}

function updateObjectField(scene) {
  const field = homeField;
  if (!field) return;
  field.replaceChildren();
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

function ambientLayer(scene) {
  if (!ambientCanvas) {
    ambientCanvas = document.createElement("canvas");
    ambientCanvas.className = "studio-ambient-canvas";
    ambientCanvas.setAttribute("aria-hidden", "true");
    ambientScene = mountStudioAmbientScene(ambientCanvas, scene);
  } else {
    ambientScene?.update(scene);
  }
  return ambientCanvas;
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
