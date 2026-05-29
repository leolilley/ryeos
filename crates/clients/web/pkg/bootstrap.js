// RyeOS browser platform shell.
//
// JS owns browser transport and DOM events. The shared Rye client model,
// surface layout, reducer, effects, and frame construction live in WASM.

import init, {
  dispatch_cockpit_file_read,
  dispatch_cockpit_files,
  dispatch_cockpit_item_inspection,
  dispatch_cockpit_gc_status,
  dispatch_cockpit_items,
  dispatch_cockpit_schedules,
  dispatch_cockpit_snapshot,
  dispatch_cockpit_thread_inspection,
  dispatch_daemon_event,
  dispatch_key,
  dispatch_poll_snapshot,
  dispatch_resize,
  render_html,
  start_with_surface,
  take_effects,
  tick,
} from "/ui/assets/ryeos_web.js";

const app = document.getElementById("app");
let currentSession = null;
let currentFilesRoot = "project_ai";
let currentFilesPath = "";
let executingRefresh = false;
let refreshQueued = false;

async function boot() {
  try {
    await init("/ui/assets/ryeos_web_bg.wasm");

    const session = await getJson("/ui/api/session/current");
    currentSession = session;
    const effectiveSurface = await postJson("/ui/api/items/effective", {
      canonical_ref: session.surface_ref,
      expected_kind: "surface",
      project_path: session.project_path,
    });

    const size = measureCells();
    renderResult(start_with_surface(session, effectiveSurface, size.cols, size.rows));
    await refreshState();
    await drainEffects();

    attachInput();
    attachResize();
    attachSessionEvents(session);

    window.setInterval(async () => {
      renderResult(tick(Date.now() >>> 0));
      await drainEffects();
    }, 1000);

    window.setInterval(() => {
      refreshState();
    }, 5000);
  } catch (err) {
    renderError(err);
  }
}

async function getJson(url) {
  const response = await fetch(url);
  if (!response.ok) {
    throw new Error(`${url}: ${response.status}`);
  }
  return response.json();
}

async function postJson(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  if (!response.ok) {
    throw new Error(`${url}: ${response.status}`);
  }
  return response.json();
}

function attachInput() {
  document.addEventListener("keydown", async (event) => {
    const blocked = [
      "ArrowUp",
      "ArrowDown",
      "ArrowLeft",
      "ArrowRight",
      "Tab",
      " ",
    ];
    if (blocked.includes(event.key)) event.preventDefault();

    renderResult(
      dispatch_key(
        keyCode(event),
        event.shiftKey,
        event.ctrlKey,
        event.altKey,
      ),
    );
    await drainEffects();
  });
}

function attachResize() {
  let timer = null;
  window.addEventListener("resize", () => {
    window.clearTimeout(timer);
    timer = window.setTimeout(() => {
      const size = measureCells();
      renderResult(dispatch_resize(size.cols, size.rows));
      drainEffects();
    }, 100);
  });
}

function renderResult(result) {
  if (result && typeof result.html === "string") {
    app.innerHTML = result.html;
  } else {
    app.innerHTML = render_html();
  }
}

async function drainEffects() {
  const effects = take_effects();
  if (!Array.isArray(effects) || effects.length === 0) return;

  for (const effect of effects) {
    await executeEffect(effect);
  }
}

async function refreshState() {
  if (executingRefresh || !currentSession) {
    refreshQueued = Boolean(currentSession);
    return;
  }

  executingRefresh = true;
  refreshQueued = false;
  try {
    const snapshot = await loadCockpitSnapshot(currentSession);
    if (snapshot) renderResult(dispatch_cockpit_snapshot(snapshot));
    const items = await loadCockpitItems();
    if (items) renderResult(dispatch_cockpit_items(items));
    const schedules = await loadCockpitSchedules();
    if (schedules) renderResult(dispatch_cockpit_schedules(schedules));
    const gc = await loadCockpitGcStatus();
    if (gc) renderResult(dispatch_cockpit_gc_status(gc));
    const files = await loadCockpitFiles();
    if (files) renderResult(dispatch_cockpit_files(files));
    const poll = await loadPollSnapshot(currentSession);
    if (poll) renderResult(dispatch_poll_snapshot(poll));
  } finally {
    executingRefresh = false;
  }

  if (refreshQueued) {
    refreshQueued = false;
    await refreshState();
  }
}

async function executeEffect(effect) {
  const [kind, payload] = effectKind(effect);
  switch (kind) {
    case "RefreshState":
      await refreshState();
      break;
    case "Execute":
      if (currentSession?.read_only) {
        console.debug("RyeOS web ignored Execute effect in read-only session", payload);
        break;
      }
      await postJson("/ui/api/actions/invoke", {
        command_id: payload.item_ref,
        args: payload.parameters || {},
      });
      await refreshState();
      break;
    case "InspectItem":
      await inspectItem(payload.item_ref);
      break;
    case "InspectThread":
      await inspectThread(payload.thread_id);
      break;
    case "ListFiles":
      await listFiles(payload.root, payload.path);
      break;
    case "ReadFile":
      await readFile(payload.root, payload.path);
      break;
    case "SendThreadCommand":
      if (currentSession?.read_only) {
        console.debug("RyeOS web ignored thread command in read-only session", payload);
        break;
      }
      await executeThreadCommand(payload);
      break;
    case "PersistSession":
      // Browser session persistence is owned by the daemon-side session store.
      break;
    case "Quit":
      app.innerHTML = '<div class="rye-error"><strong>RyeOS web client closed</strong></div>';
      break;
    default:
      console.debug("RyeOS web effect ignored", effect);
  }
}

async function inspectItem(itemRef) {
  if (!itemRef) return;
  const inspection = await postJson("/ui/api/cockpit/item/inspect", {
    canonical_ref: itemRef,
    include_raw: true,
    include_effective: false,
  });
  renderResult(dispatch_cockpit_item_inspection(inspection));
}

async function inspectThread(threadId) {
  if (!threadId) return;
  const inspection = await postJson("/ui/api/cockpit/thread/inspect", {
    thread_id: threadId,
    event_limit: 100,
  });
  renderResult(dispatch_cockpit_thread_inspection(inspection));
}

async function listFiles(root, path) {
  currentFilesRoot = root || currentFilesRoot;
  currentFilesPath = path || "";
  const listing = await postJson("/ui/api/cockpit/files/list", {
    root: currentFilesRoot,
    path: currentFilesPath,
  });
  renderResult(dispatch_cockpit_files(listing));
}

async function readFile(root, path) {
  if (!path) return;
  const file = await postJson("/ui/api/cockpit/files/read", {
    root: root || currentFilesRoot,
    path,
  });
  renderResult(dispatch_cockpit_file_read(file));
}

async function executeThreadCommand(payload) {
  const command = String(payload.command || "").toLowerCase();
  const threadId = normalizeThreadId(payload.thread_id);
  if (!threadId) return;

  if (command === "cancel") {
    await postJson("/ui/api/actions/invoke", {
      command_id: "service:threads/cancel",
      args: { thread_id: threadId },
    });
    await refreshState();
    return;
  }

  console.debug("RyeOS web thread command has no browser route yet", payload);
}

function normalizeThreadId(value) {
  if (typeof value === "number" || typeof value === "string") {
    return String(value);
  }
  if (value && typeof value === "object") {
    if (typeof value.id === "number" || typeof value.id === "string") return String(value.id);
    if (typeof value[0] === "number" || typeof value[0] === "string") return String(value[0]);
  }
  return "";
}

function effectKind(effect) {
  if (typeof effect === "string") return [effect, {}];
  if (!effect || typeof effect !== "object") return ["", {}];
  if (effect.kind) return [effect.kind, effect];
  const keys = Object.keys(effect);
  if (keys.length === 1) return [keys[0], effect[keys[0]] || {}];
  return ["", effect];
}

async function loadCockpitSnapshot(session) {
  const result = await Promise.allSettled([getJson("/ui/api/cockpit/snapshot")]);
  const snapshot = resultValue(result[0]);
  if (!snapshot) return null;
  return {
    ...snapshot,
    session: {
      ...(snapshot.session || {}),
      session_id: snapshot.session?.session_id || session.session_id,
      surface_ref: snapshot.session?.surface_ref || session.surface_ref,
    },
  };
}

async function loadPollSnapshot(session) {
  const [threadsResult, remotesResult] = await Promise.allSettled([
    getJson("/ui/api/cockpit/threads/list"),
    getJson("/ui/api/cockpit/remotes/list"),
  ]);

  return {
    threads: normalizeThreads((resultValue(threadsResult) || {}).threads || []),
    remotes: normalizeRemotes((resultValue(remotesResult) || {}).remotes || []),
    daemon_url: window.location.origin,
    daemon_alive: true,
    session_id: session.session_id,
  };
}

async function loadCockpitItems() {
  const result = await Promise.allSettled([getJson("/ui/api/cockpit/items/list")]);
  return resultValue(result[0]);
}

async function loadCockpitSchedules() {
  const result = await Promise.allSettled([getJson("/ui/api/cockpit/schedules/list")]);
  return resultValue(result[0]);
}

async function loadCockpitGcStatus() {
  const result = await Promise.allSettled([getJson("/ui/api/cockpit/gc/status")]);
  return resultValue(result[0]);
}

async function loadCockpitFiles() {
  const result = await Promise.allSettled([
    postJson("/ui/api/cockpit/files/list", { root: currentFilesRoot, path: currentFilesPath }),
  ]);
  return resultValue(result[0]);
}

function resultValue(result) {
  if (result.status === "fulfilled") return result.value || {};
  console.warn("RyeOS web snapshot source failed", result.reason);
  return null;
}

function normalizeThreads(threads) {
  return threads.map((thread) => ({
    id: thread.id || thread.thread_id,
    status: thread.status || "unknown",
    item_ref: thread.item_ref || thread.item_id,
    parent_id: thread.parent_id || null,
    started_at_ms: millis(thread.started_at || thread.created_at),
    duration_ms: thread.duration_ms || null,
    cost_usd: thread.cost_usd || null,
  })).filter((thread) => thread.id !== undefined && thread.id !== null);
}

function normalizeRemotes(remotes) {
  return remotes.map((remote, index) => ({
    id: remote.id || index,
    name: remote.name || `remote-${index}`,
    url: remote.url || "",
    alive: remote.alive || remote.health?.status === "ok" || false,
  }));
}

function attachSessionEvents(session) {
  if (!session.events_url || typeof EventSource === "undefined") return;
  const source = new EventSource(session.events_url);

  source.addEventListener("snapshot_required", async () => {
    await refreshState();
  });

  source.onmessage = (event) => {
    dispatchBrowserEvent("message", event.data);
  };

  for (const type of [
    "thread.created",
    "thread.started",
    "thread.completed",
    "thread.failed",
    "thread.upsert",
    "thread.text_delta",
    "text_delta",
  ]) {
    source.addEventListener(type, (event) => dispatchBrowserEvent(type, event.data));
  }

  source.onerror = (event) => {
    console.debug("RyeOS session event stream disconnected", event);
  };
}

function dispatchBrowserEvent(type, data) {
  try {
    const payload = data ? JSON.parse(data) : {};
    renderResult(dispatch_daemon_event({ event_type: type, payload }));
    drainEffects();
  } catch (err) {
    console.warn("RyeOS web ignored malformed event", err);
  }
}

function millis(value) {
  if (typeof value === "number") return value;
  if (!value) return null;
  const parsed = Date.parse(value);
  return Number.isFinite(parsed) ? parsed : null;
}

function renderError(err) {
  const message = err && err.message ? err.message : String(err);
  app.innerHTML = `<div class="rye-error"><strong>RyeOS web boot failed</strong><pre>${escapeHtml(message)}</pre></div>`;
}

function measureCells() {
  const probe = document.createElement("span");
  probe.className = "rye-cell-probe";
  probe.textContent = "MMMMMMMMMM";
  document.body.appendChild(probe);
  const rect = probe.getBoundingClientRect();
  probe.remove();

  const cellW = Math.max(1, rect.width / 10);
  const cellH = Math.max(1, rect.height);
  return {
    cols: Math.max(40, Math.floor(window.innerWidth / cellW)),
    rows: Math.max(16, Math.floor(window.innerHeight / cellH)),
  };
}

function keyCode(event) {
  if (event.key.length === 1) return event.key.charCodeAt(0);
  switch (event.key) {
    case "Enter":
      return 13;
    case "Tab":
      return 9;
    case "Backspace":
      return 8;
    case "Delete":
      return 46;
    case "Escape":
      return 27;
    case "ArrowLeft":
      return 37;
    case "ArrowUp":
      return 38;
    case "ArrowRight":
      return 39;
    case "ArrowDown":
      return 40;
    case "PageUp":
      return 33;
    case "PageDown":
      return 34;
    case "Home":
      return 36;
    case "End":
      return 35;
    default:
      return 0;
  }
}

function escapeHtml(value) {
  return String(value)
    .replace(/&/g, "&amp;")
    .replace(/</g, "&lt;")
    .replace(/>/g, "&gt;")
    .replace(/"/g, "&quot;");
}

boot();
