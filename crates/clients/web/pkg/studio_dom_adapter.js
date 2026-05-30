export function renderDom(root, vm, scene, dispatchUi, shell = {}) {
  root.className = "studio-app studio-os";
  root.replaceChildren(
    ambientHome(vm, scene, shell),
    notices(vm.notices || []),
    workspace(vm.workspace, dispatchUi),
    launcherDialog(vm.launcher || {}, shell),
  );
}

function ambientHome(vm, scene, shell) {
  const home = el("section", "studio-home");
  home.setAttribute("aria-label", "RyeOS Studio home space");

  const field = el("div", "studio-home-field");
  const objects = scene?.objects || [];
  for (const object of objects) {
    const marker = el("span", `studio-home-node ${object.kind || "object"} ${object.tone || "neutral"}`);
    marker.style.left = `${50 + (object.position?.[0] || 0) * 12}%`;
    marker.style.top = `${50 + (object.position?.[2] || 0) * 12}%`;
    marker.style.setProperty("--node-color", object.color || "#fabd2f");
    marker.title = object.label || object.id || "node";
    field.append(marker);
  }

  const identity = el("div", "studio-home-identity");
  identity.append(
    textEl("div", "RYE OS"),
    textEl("small", "portable operating system for ai"),
    el("i", "studio-home-line"),
    textEl("p", "Persistent, signed AI substrate that travels with you across spaces, machines, and models."),
    typerLine(),
    heroCta(shell),
  );

  const status = el("div", "studio-home-status");
  const health = textEl("span", vm.chrome?.health_label || "connecting");
  health.className = `studio-pill ${vm.chrome?.health_tone || "neutral"}`;
  status.append(
    health,
    textEl("span", vm.session?.read_only ? "read-only session" : "write-enabled session"),
    textEl("small", vm.session?.surface_ref || "surface:ryeos/studio/base"),
  );

  const hud = textEl("div", "RYE OS · STUDIO · LOCAL OPERATING SURFACE");
  hud.className = "studio-home-hud tl";
  const hint = textEl("div", "ALT+K launcher · ARROWS focus · ALT+Q close tile · ESC exit tile");
  hint.className = "studio-home-hint";

  for (const corner of ["tl", "tr", "bl", "br"]) home.append(el("i", `studio-corner ${corner}`));
  home.append(field, identity, status, hud, hint);
  return home;
}

function typerLine() {
  const line = el("div", "studio-home-typer");
  line.append(textEl("span", "> "), textEl("span", "directives, tools, knowledge — signed and portable."));
  return line;
}

function heroCta(shell) {
  const cta = el("div", "studio-home-cta");
  const actions = el("div", "studio-home-actions");
  const primary = el("button", "studio-home-btn primary");
  primary.type = "button";
  primary.textContent = "OPEN STUDIO";
  primary.addEventListener("click", () => shell.openLauncher?.());
  const secondary = el("a", "studio-home-btn secondary");
  secondary.href = "https://github.com/leolilley/ryeos";
  secondary.target = "_blank";
  secondary.rel = "noreferrer";
  secondary.textContent = "GITHUB";
  actions.append(primary, secondary);

  const install = el("button", "studio-install-card");
  install.type = "button";
  install.append(
    textEl("span", "$", "prompt"),
    textEl("span", "pip install ryeos-mcp"),
    textEl("span", "CLICK TO COPY", "copy-hint"),
  );
  install.addEventListener("click", async () => {
    await navigator.clipboard?.writeText?.("pip install ryeos-mcp");
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

function workspace(vm, dispatchUi) {
  const main = el("main", "studio-workspace");
  if (!vm?.root) {
    main.append(textEl("p", "No workspace loaded."));
    return main;
  }
  if (vm.is_home) {
    main.classList.add("home-space");
    return main;
  }
  main.append(layoutNode(vm.root, dispatchUi));
  return main;
}

function launcherDialog(state, shell) {
  const overlay = el("div", `studio-command-overlay${state.open ? " open" : ""}`);
  if (!state.open) return overlay;

  const choices = state.items || [];
  const selected = Math.min(state.selected || 0, Math.max(choices.length - 1, 0));
  const dialog = el("section", "studio-command-dialog");
  dialog.setAttribute("role", "dialog");
  dialog.setAttribute("aria-label", "Open RyeOS tile");

  const input = document.createElement("input");
  input.type = "search";
  input.placeholder = "open tile…";
  input.autocomplete = "off";
  input.spellcheck = false;
  input.value = state.query || "";
  input.setAttribute("data-studio-launcher-input", "");
  input.addEventListener("input", () => shell.setLauncherQuery?.(input.value));
  input.addEventListener("keydown", (event) => {
    if (event.key === "Escape") {
      event.preventDefault();
      shell.closeLauncher?.();
    } else if (event.key === "ArrowDown") {
      event.preventDefault();
      shell.moveLauncher?.(1);
    } else if (event.key === "ArrowUp") {
      event.preventDefault();
      shell.moveLauncher?.(-1);
    } else if (event.key === "Enter") {
      event.preventDefault();
      shell.chooseLauncher?.(event.shiftKey);
    }
  });

  const list = el("div", "studio-command-list");
  if (choices.length === 0) {
    const empty = el("div", "studio-command-empty");
    empty.textContent = "No matching tile.";
    list.append(empty);
  }
  choices.forEach((item, index) => {
    const row = el("button", `studio-command-choice${index === selected ? " selected" : ""}`);
    row.type = "button";
    row.append(textEl("strong", item.label || "View"), textEl("span", item.hint || ""));
    row.addEventListener("mouseenter", () => {
      if (index !== state.selected) shell.moveLauncher?.(index - (state.selected || 0));
    });
    row.addEventListener("click", () => shell.chooseLauncher?.(false));
    list.append(row);
  });

  const hint = textEl("div", state.hint || "Alt+K open · ↑/↓ select · Enter choose · Shift+Enter new tile · Esc close");
  hint.className = "studio-command-hint";
  dialog.append(input, list, hint);
  overlay.append(dialog);
  overlay.addEventListener("mousedown", (event) => {
    if (event.target === overlay) shell.closeLauncher?.();
  });
  return overlay;
}

function layoutNode(node, dispatchUi) {
  if (node.type === "split") {
    const wrap = el("div", `studio-split ${node.axis}`);
    wrap.style.setProperty("--split-ratio", `${Math.round((node.ratio || 0.5) * 100)}%`);
    wrap.append(layoutNode(node.first, dispatchUi), layoutNode(node.second, dispatchUi));
    return wrap;
  }
  const tile = el("section", `studio-tile${node.focused ? " focused" : ""}`);
  tile.dataset.tileId = node.tile_id || "";
  tile.addEventListener("pointerenter", (event) => {
    if (event.pointerType && event.pointerType !== "mouse") return;
    if (node.focused) return;
    dispatchUi({ type: "focus_changed", target: node.tile_id || null });
  });
  tile.addEventListener("mousedown", (event) => {
    if (event.target.closest("button,input,select,textarea,a")) return;
    if (node.focused) return;
    dispatchUi({ type: "focus_changed", target: node.tile_id || null });
  });
  const chrome = el("header", "studio-tile-chrome");
  const title = el("div", "studio-tile-title");
  title.append(textEl("strong", node.title || "Tile"));
  chrome.append(title);
  tile.append(chrome, view(node.view || {}, node.tile_id || "", dispatchUi));
  return tile;
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
      body.append(listHeader("files", `${viewVm.root}: /${viewVm.path}`), rows(viewVm.rows || [], tileId, listKind, dispatchUi));
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

function itemsToolbar(filters, tileId, dispatchUi) {
  const targetTile = filters.tile_id || tileId || "";
  const toolbar = el("div", "studio-toolbar");
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
  toolbar.append(query, kind);
  return toolbar;
}

function notices(items) {
  const wrap = el("div", "studio-notices");
  for (const item of items) {
    const notice = el("div", `studio-notice ${item.tone || "neutral"}`);
    notice.textContent = item.message || "";
    wrap.append(notice);
  }
  return wrap;
}

function columns(items) {
  const wrap = el("div", "studio-columns");
  for (const item of items) wrap.append(textEl("span", item));
  return wrap;
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
    row.disabled = !item.action;
    row.append(
      textEl("span", rowGlyph(item, kind), "studio-row-glyph"),
      textEl("strong", item.primary),
      textEl("span", item.secondary || ""),
      textEl("small", item.meta || ""),
    );
    row.addEventListener("mouseenter", () => {
      if (tileId) dispatchUi({ type: "set_tile_cursor", tile_id: tileId, index });
    });
    if (item.action) row.addEventListener("click", () => dispatchUi({ type: "activate", action: item.action }));
    list.append(row);
  });
  return list;
}

function rowGlyph(item, kind) {
  const meta = (item.meta || "").toLowerCase();
  const secondary = (item.secondary || "").toLowerCase();
  if (kind === "files") return secondary.includes("directory") ? "▸" : "·";
  if (kind === "items") {
    if (meta.includes("tool")) return "⚙";
    if (meta.includes("directive")) return "◆";
    if (meta.includes("knowledge")) return "◈";
    if (meta.includes("config")) return "◇";
    return "•";
  }
  if (kind === "threads") return "▶";
  return "•";
}

function inspector(vm) {
  const wrap = el("div", "studio-inspector-view");
  wrap.append(textEl("h2", vm.title));
  if (vm.subtitle) wrap.append(textEl("p", vm.subtitle));
  if (vm.empty) wrap.append(textEl("p", "Select a Studio object to inspect it."));
  for (const section of vm.sections || []) wrap.append(sectionBlock(section, null));
  for (const block of vm.code_blocks || []) wrap.append(textEl("h3", block.label), code(block.content));
  return wrap;
}

function code(content) {
  const pre = el("pre", "studio-code");
  pre.textContent = content || "";
  return pre;
}

function textEl(tag, text, className = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  node.textContent = text || "";
  return node;
}

function el(tag, className = "") {
  const node = document.createElement(tag);
  if (className) node.className = className;
  return node;
}
