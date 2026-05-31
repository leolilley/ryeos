import { mountStudioAmbientScene } from "/ui/assets/studio_ambient_scene.js";
import { el, textEl } from "/ui/assets/studio_components_primitives.js";

let ambientCanvas = null;
let ambientScene = null;
let typerTimer = null;
let typerLineIndex = 0;

const FALLBACK_TYPER_LINES = [
  "solve once, solve everywhere.",
  "swap the model. the substrate remains.",
  "four tools. one substrate.",
  "directives, tools, knowledge — signed and portable.",
  "ed25519 seals every item.",
  "tampered items fail closed.",
  "runtimes live in yaml, not code.",
  "space resolves: project → user → system.",
  "data-driven execution engine.",
  "lillux microkernel.",
  "any tool. two primitives.",
  "tool → runtime → primitive. verified chain.",
  "pull from the registry. trust the author.",
  "content-addressed. immutable. keyed by hash.",
  "search. load. execute. sign.",
];

export function studioHome(vm, scene, shell) {
  const isHome = vm.workspace?.is_home !== false;
  const home = el("section", "studio-home");
  home.setAttribute("aria-label", "RyeOS Studio home space");
  home.style.setProperty("--scene-object-count", String(scene?.objects?.length || 0));
  if (!isHome) {
    home.classList.add("backdrop-only");
    home.setAttribute("aria-hidden", "true");
  }

  home.append(ambientBackground(scene), objectField(scene));
  if (isHome) home.append(homeLanding(vm, shell));
  return home;
}

function ambientBackground(scene) {
  return ambientLayer(scene);
}

function objectField(scene) {
  const field = el("div", "studio-home-field");
  for (const object of scene?.objects || []) {
    const marker = el("span", `studio-home-node ${object.kind || "object"} ${object.tone || "neutral"}`);
    marker.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    marker.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    marker.style.setProperty("--node-color", object.color || "#fabd2f");
    marker.title = object.label || object.id || "node";
    field.append(marker);
  }
  return field;
}

function homeLanding(vm, shell) {
  const landing = document.createDocumentFragment();
  const identity = el("div", "studio-home-identity");
  const homeVm = vm.presentation?.home || {};
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

export function statusLine(vm, shell) {
  const line = el("footer", "studio-statusbar");
  const status = vm.presentation?.chrome?.status_bar;
  const segments = status?.segments || [];
  if (segments.length === 0) {
    const mode = vm.session?.read_only ? "ro" : "rw";
    const health = vm.chrome?.health_label || "connecting";
    const version = ryeosVersion(shell);
    const project = vm.session?.project_path || shell?.snapshot?.project?.path || "home";
    line.append(
      textEl("strong", "rye os"),
      textEl("span", `v${version}`),
      textEl("span", health, `tone-${vm.chrome?.health_tone || "neutral"}`),
      textEl("span", mode),
      textEl("span", project, "grow"),
      textEl("span", "alt+k open · arrows focus · enter select · esc close", "keys"),
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
  return shell?.snapshot?.local_node?.status?.version || "0.1.0";
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
  const line = el("div", "studio-home-typer");
  const text = textEl("span", "");
  text.className = "typer-text";
  line.append(textEl("span", "> ", "typer-cursor"), text, el("span", "typer-caret"));
  window.queueMicrotask(() => startTypewriter(text, lines));
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
