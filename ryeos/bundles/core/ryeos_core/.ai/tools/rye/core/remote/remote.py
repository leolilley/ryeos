# rye:signed:2026-03-11T09:35:05Z:5c57e113c68df81fd116ebce11df006fe5b5119d52f15a1727b9201a3a44a5de:er7PuadUjAiCjrOrgvbpVGWC8SciSB88uqoIvhR_CgDReFSC1WmWEzLDdCvW2tPeweYVtCx-z_ZSmbdDdE5sCQ==:4b987fd4e40303ac
"""
Remote tool — sync and execute against ryeos-remote server.

Actions:
  push    - Build manifests, sync missing objects to remote.
  pull    - Fetch new objects from remote (execution results).
  status  - Show local vs remote manifest hashes.
  execute - Push + trigger remote execution + pull results.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/remote"
__tool_description__ = "Sync and execute against ryeos-remote server"

import json
import logging
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.cas.manifest import build_manifest
from rye.cas.store import cas_root
from rye.cas.sync import collect_object_hashes, export_objects, import_objects
from rye.cas.materializer import get_system_version
from rye.constants import AI_DIR

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "remote",
    "description": "Sync and execute against ryeos-remote server",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["push", "pull", "status", "execute", "threads", "thread_status"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Remote operation: push, pull, status, execute, threads, thread_status",
        },
        "item_type": {
            "type": "string",
            "description": "Item type for execute (tool, directive, knowledge)",
        },
        "item_id": {
            "type": "string",
            "description": "Item ID for execute",
        },
        "parameters": {
            "type": "object",
            "description": "Parameters for remote execute",
        },
        "remote": {
            "type": "string",
            "description": "Named remote to target (from cas/remote.yaml). Defaults to 'default'.",
        },
        "thread_id": {
            "type": "string",
            "description": "Thread ID for thread_status action",
        },
        "limit": {
            "type": "integer",
            "description": "Max threads to return for threads action (default: 20)",
        },
        "project_name": {
            "type": "string",
            "description": "Filter threads by project name",
        },
    },
    "required": ["action"],
}


# ---------------------------------------------------------------------------
# HTTP client (uses lillux primitive, same pattern as registry tool)
# ---------------------------------------------------------------------------


class RemoteHttpClient:
    """HTTP client for ryeos-remote API calls."""

    def __init__(self, base_url: str, api_key: str):
        self.base_url = base_url.rstrip("/")
        self.api_key = api_key
        self._http = None

    async def _get_http(self):
        if self._http is None:
            from lillux.primitives.http_client import HttpClientPrimitive
            self._http = HttpClientPrimitive()
        return self._http

    async def get(self, path: str) -> Dict:
        http = await self._get_http()
        config = {
            "method": "GET",
            "url": f"{self.base_url}{path}",
            "headers": {
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
            },
            "timeout": 30,
        }
        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }

    async def post(self, path: str, body: Dict, timeout: int = 60) -> Dict:
        http = await self._get_http()
        config = {
            "method": "POST",
            "url": f"{self.base_url}{path}",
            "headers": {
                "Authorization": f"Bearer {self.api_key}",
                "Content-Type": "application/json",
            },
            "body": body,
            "timeout": timeout,
        }
        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }


def _get_client(remote_name=None, project_path=None) -> RemoteHttpClient:
    """Create HTTP client using named remote config."""
    from remote_config import resolve_remote
    config = resolve_remote(remote_name, project_path)
    return RemoteHttpClient(config.url, config.api_key)


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------


async def _push(project_path: Path, params: Dict) -> Dict:
    """Build manifests, sync missing objects to remote, publish project ref.

    Flow:
      1. Build project + user manifests (local CAS)
      2. Collect all transitive object hashes
      3. POST /objects/has → get missing list
      4. Export missing objects, POST /objects/put
      5. POST /push → upsert project ref on remote
    """
    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    root = cas_root(project_path)

    # Build manifests
    ph, pm = build_manifest(project_path, "project")

    from rye.utils.path_utils import get_user_space
    user_root = get_user_space()
    uh, um = build_manifest(user_root, "user", project_path=project_path)

    # Collect all object hashes (transitive)
    all_hashes = list(set(
        collect_object_hashes(pm, root)
        + collect_object_hashes(um, root)
        + [ph, uh]
    ))

    # Check which objects remote already has
    has_resp = await client.post("/objects/has", {"hashes": all_hashes})
    if not has_resp["success"]:
        return {"error": f"Failed to check objects: {has_resp['error']}"}

    has_body = has_resp["body"]
    if isinstance(has_body, str):
        has_body = json.loads(has_body)
    missing = has_body.get("missing", [])

    objects_synced = 0
    if missing:
        # Export and upload missing objects
        entries = export_objects(missing, root)
        put_resp = await client.post("/objects/put", {
            "entries": [e if isinstance(e, dict) else e.to_dict() for e in entries],
        })
        if not put_resp["success"]:
            return {"error": f"Failed to upload objects: {put_resp['error']}"}

        put_body = put_resp["body"]
        if isinstance(put_body, str):
            put_body = json.loads(put_body)
        objects_synced = len(put_body.get("stored", []))

    # Publish project ref on remote
    from remote_config import get_project_name
    proj_name = get_project_name(project_path)
    sys_ver = get_system_version()

    push_resp = await client.post("/push", {
        "project_name": proj_name,
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "system_version": sys_ver,
    })

    push_body = push_resp.get("body", {})
    if isinstance(push_body, str):
        push_body = json.loads(push_body)

    ref_published = push_resp["success"]
    if not ref_published:
        logger.warning("Failed to publish project ref: %s", push_resp.get("error"))

    result = {
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "project_name": proj_name,
        "remote_name": push_body.get("remote_name", remote_name or "default"),
        "objects_synced": objects_synced,
        "total_objects": len(all_hashes),
        "ref_published": ref_published,
        "message": f"Synced {len(missing)} objects to remote" if missing else "Remote is up to date",
    }
    if not ref_published:
        result["ref_warning"] = f"Objects uploaded but project ref not published: {push_resp.get('error')}"
    return result


async def _pull(project_path: Path, params: Dict) -> Dict:
    """Fetch new objects from remote.

    Pulls objects by hash from the remote CAS into the local CAS.
    Typically called after a remote execute to retrieve results.
    """
    hashes = params.get("hashes", [])
    if not hashes:
        return {"error": "No hashes specified. Provide hashes to pull."}

    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    root = cas_root(project_path)

    get_resp = await client.post("/objects/get", {"hashes": hashes})
    if not get_resp["success"]:
        return {"error": f"Failed to fetch objects: {get_resp['error']}"}

    get_body = get_resp["body"]
    if isinstance(get_body, str):
        get_body = json.loads(get_body)

    entries = get_body.get("entries", [])
    if not entries:
        return {"fetched": 0, "message": "No objects found on remote"}

    stored = import_objects(entries, root)
    return {
        "fetched": len(stored),
        "hashes": stored,
        "message": f"Pulled {len(stored)} objects from remote",
    }


async def _status(project_path: Path, params: Dict) -> Dict:
    """Show local manifest hashes, system version, and configured remotes."""
    root = cas_root(project_path)

    ph, pm = build_manifest(project_path, "project")

    from rye.utils.path_utils import get_user_space
    user_root = get_user_space()
    uh, um = build_manifest(user_root, "user", project_path=project_path)

    all_hashes = list(set(
        collect_object_hashes(pm, root)
        + collect_object_hashes(um, root)
        + [ph, uh]
    ))

    result = {
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "system_version": get_system_version(),
        "total_objects": len(all_hashes),
        "project_items": len(pm.get("items", {})),
        "project_files": len(pm.get("files", {})),
        "user_items": len(um.get("items", {})),
    }

    from remote_config import list_remotes
    remote_name = params.get("remote")
    if remote_name:
        # Show status for a specific remote
        result["remotes"] = {remote_name: list_remotes(project_path).get(remote_name, {"error": "not found"})}
    else:
        # Show all configured remotes
        result["remotes"] = list_remotes(project_path)

    return result


async def _verify_remote_key(client: "RemoteHttpClient", remote_name: Optional[str] = None) -> Optional[Dict]:
    """Fetch remote public key and verify against pinned fingerprint.

    TOFU: pins on first contact. Hard-fails on fingerprint mismatch or
    fetch failure. Returns error dict on failure, None if OK.
    """
    resp = await client.get("/public-key")
    if not resp["success"]:
        return {
            "error": f"Could not verify remote server key: {resp.get('error')}",
        }

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)

    pem_text = body.get("public_key_pem", "")
    if not pem_text:
        return {"error": "Remote server returned empty public key"}

    remote_pem = pem_text.encode("utf-8")

    from lillux.primitives.signing import compute_key_fingerprint
    from rye.utils.trust_store import TrustStore

    remote_fp = compute_key_fingerprint(remote_pem)
    trust_store = TrustStore()
    from urllib.parse import urlparse
    host = urlparse(client.base_url).netloc
    name = remote_name or "default"
    owner = f"remote:{name}:{host}"
    pinned_key = trust_store.get_remote_key(remote_name=owner)

    if pinned_key is None:
        # TOFU: first contact, pin the key
        trust_store.pin_remote_key(remote_pem, remote_name=owner)
        logger.info("Pinned remote server key (TOFU): %s", remote_fp)
        return None

    pinned_fp = compute_key_fingerprint(pinned_key)
    if remote_fp == pinned_fp:
        return None

    # Key rotation detected — hard fail
    return {
        "error": (
            f"Remote server key mismatch (pinned: {pinned_fp}, "
            f"remote: {remote_fp}). To re-pin, remove the old key via "
            f"'rye execute tool rye/core/keys action=remove fingerprint={pinned_fp}' "
            f"then re-run the remote command to TOFU-pin the new key."
        ),
    }


async def _execute(project_path: Path, params: Dict) -> Dict:
    """Push + trigger remote execution + pull results.

    End-to-end flow:
      1. Push (sync objects)
      2. POST /execute on remote
      3. Pull new result objects
    """
    item_type = params.get("item_type")
    item_id = params.get("item_id")
    exec_params = params.get("parameters", {})
    thread = params.get("thread")

    if not item_type or not item_id:
        return {"error": "item_type and item_id are required for execute"}
    if not thread:
        return {"error": "thread is required for execute"}

    # Validate thread/item_type before hitting the server
    if item_type == "directive" and thread != "fork":
        return {
            "error": (
                f"Directives must use thread=fork on remote, got thread={thread!r}. "
                "The remote server needs to spawn an LLM thread to follow directive steps."
            ),
        }
    if item_type == "tool" and thread != "inline":
        return {
            "error": (
                f"Tools must use thread=inline on remote, got thread={thread!r}. "
                "Tools execute directly — fork spawns an LLM thread, which only applies to directives."
            ),
        }

    # 1. Verify remote server key (before push — fail early)
    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)
    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    # 2. Push
    push_result = await _push(project_path, {"remote": remote_name})
    if "error" in push_result:
        return push_result

    # 3. Execute on remote (longer timeout for execution)
    exec_resp = await client.post("/execute", {
        "project_manifest_hash": push_result["project_manifest_hash"],
        "user_manifest_hash": push_result["user_manifest_hash"],
        "system_version": get_system_version(),
        "item_type": item_type,
        "item_id": item_id,
        "parameters": exec_params,
        "thread": thread,
    }, timeout=300)

    if not exec_resp["success"]:
        return {"error": f"Remote execution failed: {exec_resp['error']}"}

    exec_body = exec_resp["body"]
    if isinstance(exec_body, str):
        exec_body = json.loads(exec_body)

    # 4. Pull execution outputs
    snapshot_hash = exec_body.get("execution_snapshot_hash")
    pull_hashes = []
    if snapshot_hash:
        pull_hashes.append(snapshot_hash)
    new_object_hashes = exec_body.get("new_object_hashes", [])
    pull_hashes.extend(new_object_hashes)
    if pull_hashes:
        pull_result = await _pull(project_path, {"hashes": pull_hashes, "remote": remote_name})
    else:
        pull_result = {"fetched": 0}

    return {
        "status": exec_body.get("status"),
        "thread_id": exec_body.get("thread_id"),
        "execution_snapshot_hash": snapshot_hash,
        "result": exec_body.get("result"),
        "objects_pushed": push_result.get("objects_synced", 0),
        "objects_pulled": pull_result.get("fetched", 0),
        "system_version": exec_body.get("system_version"),
    }


async def _threads(project_path: Path, params: Dict) -> Dict:
    """List remote executions from the server."""
    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    limit = params.get("limit", 20)
    proj_name = params.get("project_name")

    path = f"/threads?limit={limit}"
    if proj_name:
        path += f"&project_name={proj_name}"

    resp = await client.get(path)
    if not resp["success"]:
        return {"error": f"Failed to list threads: {resp.get('error')}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)
    return body


async def _thread_status(project_path: Path, params: Dict) -> Dict:
    """Get status of a specific remote thread."""
    thread_id = params.get("thread_id")
    if not thread_id:
        return {"error": "thread_id is required for thread_status"}

    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    resp = await client.get(f"/threads/{thread_id}")
    if not resp["success"]:
        return {"error": f"Failed to get thread: {resp.get('error')}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)
    return body


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

_ACTION_MAP = {
    "push": _push,
    "pull": _pull,
    "status": _status,
    "execute": _execute,
    "threads": _threads,
    "thread_status": _thread_status,
}


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.pop("action", None)
    if not action:
        return {"success": False, "error": "action required in params"}
    if action not in ACTIONS:
        return {"success": False, "error": f"Unknown action: {action}", "valid_actions": ACTIONS}

    pp = Path(project_path).resolve()
    if not pp.is_dir():
        return {"success": False, "error": f"Project path does not exist: {project_path}"}

    result = await _ACTION_MAP[action](pp, params)
    if "error" in result:
        result["success"] = False
    elif "success" not in result:
        result["success"] = True
    return result
