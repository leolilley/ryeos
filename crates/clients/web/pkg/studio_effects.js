export async function runEffect(effect) {
  const kind = effect.kind;
  switch (kind.type) {
    case "fetch_dimension":
      return result(effect, "dimension", await getJson("/ui/api/studio/dimension"));
    case "fetch_projects":
      return result(effect, "projects", await optionalProjectsJson());
    case "fetch_topology":
      return result(effect, "topology", await getJson("/ui/api/graph/topology"));
    case "add_project":
      return result(effect, "project_added", await postJson("/ui/api/studio/projects/add", { root: kind.root }));
    case "open_project":
      return result(effect, "project_opened", await postJson("/ui/api/studio/projects/open", { local_id: kind.local_id }));
    case "fetch_threads":
      return result(effect, "threads", await getJson(withParams("/ui/api/studio/threads/list", { limit: kind.limit })));
    case "fetch_items":
      return result(effect, "items", await getJson(withParams("/ui/api/studio/items/list", {
        limit: kind.limit,
        query: kind.query,
        kind: kind.kind,
      })));
    case "fetch_schedules":
      return result(effect, "schedules", await getJson("/ui/api/studio/schedules/list"));
    case "fetch_gc_status":
      return result(effect, "gc_status", await getJson("/ui/api/studio/gc/status"));
    case "list_files":
      return result(effect, "files_list", await fileJson("/ui/api/studio/files/list", kind));
    case "read_file":
      return result(effect, "file_read", await fileJson("/ui/api/studio/files/read", kind));
    case "inspect_item":
      return result(effect, "item_inspection", await postJson("/ui/api/studio/item/inspect", {
        canonical_ref: kind.canonical_ref,
        include_raw: kind.include_raw,
        include_effective: kind.include_effective,
      }));
    case "inspect_thread":
      return result(effect, "thread_inspection", await postJson("/ui/api/studio/thread/inspect", {
        thread_id: kind.thread_id,
        event_limit: kind.event_limit,
      }));
    case "set_location_hash":
      location.hash = kind.hash;
      return result(effect, "browser_only", null);
    case "copy_to_clipboard":
      await navigator.clipboard.writeText(kind.text);
      return result(effect, "browser_only", null);
    case "open_url":
      window.open(kind.url, "_blank", "noopener,noreferrer");
      return result(effect, "browser_only", null);
    default:
      throw new Error(`Unhandled RyeOS effect: ${kind.type}`);
  }
}

function fileRoot(root) {
  return root === "project_ai" ? "project" : root;
}

async function fileJson(url, kind) {
  let data;
  try {
    data = await postJson(url, { root: fileRoot(kind.root), path: kind.path });
  } catch (error) {
    if (isNoBoundProjectError(error) && url.endsWith("/files/list")) {
      return { root: kind.root || "project", path: kind.path || "", truncated: false, entries: [] };
    }
    throw error;
  }
  if (kind.root && data && typeof data === "object") data.root = kind.root;
  return data;
}

async function optionalProjectsJson() {
  try {
    return await getJson("/ui/api/studio/projects/list");
  } catch (error) {
    if (String(error?.message || error).includes("/ui/api/studio/projects/list: 404")) {
      return { version: 1, projects: [] };
    }
    throw error;
  }
}

function isNoBoundProjectError(error) {
  const message = String(error?.message || error);
  return message.includes("/ui/api/studio/files/list: 400") && message.includes("no project bound to this session");
}

export function failedResultFor(effect, error) {
  return {
    id: effect.id,
    ok: false,
    kind: resultKindFor(effect),
    error: error?.message || String(error),
  };
}

function result(effect, kind, data) {
  return { id: effect.id, ok: true, kind, data };
}

function resultKindFor(effect) {
  const type = effect?.kind?.type;
  if (type === "fetch_dimension") return "dimension";
  if (type === "fetch_projects") return "projects";
  if (type === "fetch_topology") return "topology";
  if (type === "add_project") return "project_added";
  if (type === "open_project") return "project_opened";
  if (type === "fetch_threads") return "threads";
  if (type === "fetch_items") return "items";
  if (type === "fetch_schedules") return "schedules";
  if (type === "fetch_gc_status") return "gc_status";
  if (type === "list_files") return "files_list";
  if (type === "read_file") return "file_read";
  if (type === "inspect_item") return "item_inspection";
  if (type === "inspect_thread") return "thread_inspection";
  return "browser_only";
}

async function getJson(url) {
  const response = await fetch(url);
  if (!response.ok) throw new Error(`${url}: ${response.status}`);
  return response.json();
}

async function postJson(url, body) {
  const response = await fetch(url, {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body || {}),
  });
  if (!response.ok) throw new Error(`${url}: ${response.status} ${await response.text()}`);
  return response.json();
}

function withParams(url, params) {
  const query = new URLSearchParams();
  for (const [key, value] of Object.entries(params || {})) {
    if (value !== undefined && value !== null && value !== "") query.set(key, value);
  }
  const text = query.toString();
  return text ? `${url}?${text}` : url;
}
