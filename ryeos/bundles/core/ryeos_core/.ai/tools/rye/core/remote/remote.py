# rye:signed:2026-03-10T04:07:14Z:b01aeff0af52fbfd5a861832c724d9718a061a07c89390966b433b58c5db8ad8:RQ0geuwnLgODYUhDEptfUfzW_JRQ8kw_ArMV9uvMiIzB3kU5kYGXBPcyYZXVffKy1ZzNjwerzcCSizizokPMCg==:4b987fd4e40303ac
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
import os
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

ACTIONS = ["push", "pull", "status", "execute"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Remote operation: push, pull, status, execute",
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


def _get_api_key() -> Optional[str]:
    """Get remote API key from environment."""
    return os.environ.get("RYE_REMOTE_API_KEY")


def _get_client() -> RemoteHttpClient:
    """Create HTTP client with auth."""
    remote_url = os.environ.get("RYE_REMOTE_URL", "")
    if not remote_url:
        raise ValueError(
            "RYE_REMOTE_URL not set. "
            "Set it via: export RYE_REMOTE_URL=https://your-remote-server"
        )
    api_key = _get_api_key()
    if not api_key:
        raise ValueError(
            "RYE_REMOTE_API_KEY not set. "
            "Set it via: export RYE_REMOTE_API_KEY=your_key"
        )
    return RemoteHttpClient(remote_url, api_key)


# ---------------------------------------------------------------------------
# Actions
# ---------------------------------------------------------------------------


async def _push(project_path: Path, params: Dict) -> Dict:
    """Build manifests, sync missing objects to remote.

    Flow:
      1. Build project + user manifests (local CAS)
      2. Collect all transitive object hashes
      3. POST /objects/has → get missing list
      4. Export missing objects, POST /objects/put
    """
    client = _get_client()
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

    if not missing:
        return {
            "project_manifest_hash": ph,
            "user_manifest_hash": uh,
            "objects_synced": 0,
            "total_objects": len(all_hashes),
            "message": "Remote is up to date",
        }

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

    return {
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "objects_synced": len(put_body.get("stored", [])),
        "total_objects": len(all_hashes),
        "message": f"Synced {len(missing)} objects to remote",
    }


async def _pull(project_path: Path, params: Dict) -> Dict:
    """Fetch new objects from remote.

    Pulls objects by hash from the remote CAS into the local CAS.
    Typically called after a remote execute to retrieve results.
    """
    hashes = params.get("hashes", [])
    if not hashes:
        return {"error": "No hashes specified. Provide hashes to pull."}

    client = _get_client()
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
    """Show local manifest hashes and system version."""
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

    return {
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "system_version": get_system_version(),
        "total_objects": len(all_hashes),
        "project_items": len(pm.get("items", {})),
        "project_files": len(pm.get("files", {})),
        "user_items": len(um.get("items", {})),
    }


async def _verify_remote_key(client: "RemoteHttpClient") -> Optional[Dict]:
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
    pinned_key = trust_store.get_remote_key()

    if pinned_key is None:
        # TOFU: first contact, pin the key
        trust_store.pin_remote_key(remote_pem)
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

    if not item_type or not item_id:
        return {"error": "item_type and item_id are required for execute"}

    # 1. Verify remote server key (before push — fail early)
    client = _get_client()
    key_err = await _verify_remote_key(client)
    if key_err:
        return key_err

    # 2. Push
    push_result = await _push(project_path, {})
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
        pull_result = await _pull(project_path, {"hashes": pull_hashes})
    else:
        pull_result = {"fetched": 0}

    return {
        "status": exec_body.get("status"),
        "execution_snapshot_hash": snapshot_hash,
        "result": exec_body.get("result"),
        "objects_pushed": push_result.get("objects_synced", 0),
        "objects_pulled": pull_result.get("fetched", 0),
        "system_version": exec_body.get("system_version"),
    }


# ---------------------------------------------------------------------------
# Entry point
# ---------------------------------------------------------------------------

_ACTION_MAP = {
    "push": _push,
    "pull": _pull,
    "status": _status,
    "execute": _execute,
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
