# rye:signed:2026-03-29T05:38:21Z:b23b0d34aaf8592d373afe936ae448a0f224b5761cb337309da51ebbba4eca20:21-IVYCvr0md1bpiSZ1IFiY1G8akD-xUFoA-4OF9sFZJwJVSmATrDz61g8Nlc_6H3BmuXmxaLmku6zV6aPbxDw==:4b987fd4e40303ac
"""
Remote tool — sync and execute against ryeos-remote server.

Actions:
  push           - Build manifests, sync missing objects to remote.
  pull           - Fetch new objects from remote (execution results).
  status         - Show local vs remote manifest hashes.
  execute        - Push + trigger remote execution + pull results.
  secrets_push   - Push secrets from .env file or env vars to remote vault.
  secrets_list   - List secret names stored on remote (no values).
  secrets_remove - Remove a named secret from remote vault.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/remote"
__tool_description__ = "Sync and execute against ryeos-remote server"

import hashlib
import json
import logging
import os
import tempfile
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

ACTIONS = [
    "push", "pull", "status", "execute", "threads", "thread_status",
    "secrets_push", "secrets_list", "secrets_remove",
]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Remote operation: push, pull, status, execute, threads, thread_status, secrets_push, secrets_list, secrets_remove",
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
        "project_path": {
            "type": "string",
            "description": "Filter threads by project path",
        },
        "env_file": {
            "type": "string",
            "description": "Path to .env file for secrets_push (reads KEY=VALUE pairs)",
        },
        "names": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Secret names to push from env vars (for secrets_push without env_file)",
        },
        "secret_name": {
            "type": "string",
            "description": "Secret name for secrets_remove",
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
            from rye.runtime.http_client import HttpClientPrimitive
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

    async def delete(self, path: str) -> Dict:
        http = await self._get_http()
        config = {
            "method": "DELETE",
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


def _get_client(remote_name=None, project_path=None) -> RemoteHttpClient:
    """Create HTTP client using named remote config."""
    from remote_config import resolve_remote
    config = resolve_remote(remote_name, project_path)
    return RemoteHttpClient(config.url, config.api_key)


# ---------------------------------------------------------------------------
# Local snapshot ref tracking
# ---------------------------------------------------------------------------


def _remote_ref_path(project_path: Path, remote_name: str) -> Path:
    """Path to the local file tracking a remote's HEAD snapshot hash."""
    return project_path / AI_DIR / "objects" / "refs" / "remotes" / f"{remote_name}.json"


def _load_remote_snapshot_hash(project_path: Path, remote_name: str) -> Optional[str]:
    """Load the last-known snapshot_hash for a remote. Returns None if not tracked."""
    ref_file = _remote_ref_path(project_path, remote_name)
    if not ref_file.exists():
        return None
    try:
        data = json.loads(ref_file.read_text())
        return data.get("snapshot_hash")
    except Exception:
        logger.warning("Failed to read remote ref %s", ref_file, exc_info=True)
        return None


def _store_remote_snapshot_hash(project_path: Path, remote_name: str, snapshot_hash: str) -> None:
    """Store the remote's HEAD snapshot_hash locally for next push."""
    ref_file = _remote_ref_path(project_path, remote_name)
    ref_file.parent.mkdir(parents=True, exist_ok=True)
    ref_file.write_text(json.dumps({"snapshot_hash": snapshot_hash}))


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

    from remote_config import get_project_path
    proj_name = get_project_path(project_path)
    sys_ver = get_system_version()
    effective_remote = remote_name or "default"

    # Push user space first — project snapshot references user_manifest_hash
    user_space_pushed = False
    user_space_resp = await client.post("/push/user-space", {
        "user_manifest_hash": uh,
    })
    if user_space_resp["success"]:
        user_space_pushed = True
    else:
        # 409 = revision moved, retry with current revision
        status_code = user_space_resp.get("status_code", 0)
        if status_code == 409:
            us_ref = await client.get("/user-space")
            if us_ref["success"]:
                ref_body = us_ref["body"]
                if isinstance(ref_body, str):
                    ref_body = json.loads(ref_body)
                current_rev = ref_body.get("snapshot_revision")
                if current_rev is not None:
                    retry_resp = await client.post("/push/user-space", {
                        "user_manifest_hash": uh,
                        "expected_revision": current_rev,
                    })
                    user_space_pushed = retry_resp["success"]
        if not user_space_pushed:
            logger.warning("Failed to push user space: %s", user_space_resp.get("error"))

    # Load expected_snapshot_hash from local ref tracking
    expected_snapshot_hash = _load_remote_snapshot_hash(project_path, effective_remote)

    # Publish project ref on remote (user space already pushed above)
    push_resp = await client.post("/push", {
        "project_path": proj_name,
        "project_manifest_hash": ph,
        "system_version": sys_ver,
        "expected_snapshot_hash": expected_snapshot_hash,
    })

    push_body = push_resp.get("body", {})
    if isinstance(push_body, str):
        push_body = json.loads(push_body)

    ref_published = push_resp["success"]

    # 409 = HEAD moved (e.g. fold-back from execution), retry with current HEAD
    if not ref_published and push_resp.get("status_code") == 409:
        detail = push_body.get("detail", push_body) if isinstance(push_body, dict) else {}
        actual_head = detail.get("actual") if isinstance(detail, dict) else None
        if actual_head is not None:
            logger.info("HEAD moved, retrying push with actual=%s", actual_head)
            _store_remote_snapshot_hash(project_path, effective_remote, actual_head)
            retry_resp = await client.post("/push", {
                "project_path": proj_name,
                "project_manifest_hash": ph,
                "system_version": sys_ver,
                "expected_snapshot_hash": actual_head,
            })
            retry_body = retry_resp.get("body", {})
            if isinstance(retry_body, str):
                retry_body = json.loads(retry_body)
            if retry_resp["success"]:
                push_resp = retry_resp
                push_body = retry_body
                ref_published = True

    if not ref_published:
        logger.warning("Failed to publish project ref: %s", push_resp.get("error"))

    # Store returned snapshot_hash for next push
    if ref_published and push_body.get("snapshot_hash"):
        _store_remote_snapshot_hash(
            project_path, effective_remote, push_body["snapshot_hash"],
        )

    result = {
        "project_manifest_hash": ph,
        "user_manifest_hash": uh,
        "project_path": proj_name,
        "remote_name": push_body.get("remote_name", effective_remote),
        "snapshot_hash": push_body.get("snapshot_hash"),
        "objects_synced": objects_synced,
        "total_objects": len(all_hashes),
        "ref_published": ref_published,
        "user_space_pushed": user_space_pushed,
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


# Allowlisted path prefixes for runtime output materialization.
_RUNTIME_OUTPUT_PREFIXES = (
    ".ai/agent/",
    ".ai/knowledge/agent/",
    ".ai/objects/refs/",
)


def _materialize_runtime_outputs(
    bundle_hash: str,
    project_path: Path,
) -> int:
    """Materialize a RuntimeOutputsBundle into the local project tree.

    Reads the bundle object from local CAS, then for each file entry,
    reads the blob and writes it to the corresponding local path.
    Refs (.ai/objects/refs/) are written last so targets exist first.

    Returns count of files materialized.
    """
    from lillux.primitives import cas

    root = cas_root(project_path)
    obj = cas.get_object(bundle_hash, root)
    if obj is None:
        logger.warning("RuntimeOutputsBundle %s not found in local CAS", bundle_hash[:16])
        return 0

    if obj.get("kind") != "runtime_outputs_bundle":
        logger.warning("Expected runtime_outputs_bundle, got %s", obj.get("kind"))
        return 0

    files = obj.get("files", {})
    if not files:
        return 0

    resolved_root = project_path.resolve()
    count = 0

    # Split into regular files and refs (refs written last)
    regular_files = {}
    ref_files = {}
    for rel_path, blob_hash in files.items():
        if rel_path.startswith(".ai/objects/refs/"):
            ref_files[rel_path] = blob_hash
        else:
            regular_files[rel_path] = blob_hash

    for batch in (regular_files, ref_files):
        for rel_path, blob_hash in batch.items():
            # Validate path: no absolute, no escapes, must match allowlist
            if os.path.isabs(rel_path) or ".." in rel_path.split("/"):
                logger.warning("Rejecting invalid path from bundle: %s", rel_path)
                continue

            if not any(rel_path.startswith(p) for p in _RUNTIME_OUTPUT_PREFIXES):
                logger.warning("Path not in allowlist: %s", rel_path)
                continue

            target = (project_path / rel_path).resolve()
            if not target.is_relative_to(resolved_root):
                logger.warning("Path escapes project root: %s", rel_path)
                continue

            blob_data = cas.get_blob(blob_hash, root)
            if blob_data is None:
                logger.warning(
                    "Blob %s for %s not found in local CAS",
                    blob_hash[:16], rel_path,
                )
                continue

            # Atomic write (tmp + rename)
            target.parent.mkdir(parents=True, exist_ok=True)
            fd, tmp_path = tempfile.mkstemp(dir=target.parent)
            try:
                os.write(fd, blob_data)
                os.close(fd)
                os.replace(tmp_path, target)
                count += 1
            except BaseException:
                os.close(fd)
                try:
                    os.unlink(tmp_path)
                except OSError:
                    pass
                raise

    if count:
        logger.info(
            "Materialized %d runtime output files from bundle %s",
            count, bundle_hash[:16],
        )

    return count


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
    from remote_config import get_project_path
    proj_name = get_project_path(project_path)
    exec_resp = await client.post("/execute", {
        "project_path": proj_name,
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

    # 4. Pull execution outputs (CAS objects + runtime output blobs)
    snapshot_hash = exec_body.get("execution_snapshot_hash")
    bundle_hash = exec_body.get("runtime_outputs_bundle_hash")
    pull_hashes = []
    if snapshot_hash:
        pull_hashes.append(snapshot_hash)
    new_object_hashes = exec_body.get("new_object_hashes", [])
    pull_hashes.extend(new_object_hashes)
    if pull_hashes:
        pull_result = await _pull(project_path, {"hashes": pull_hashes, "remote": remote_name})
    else:
        pull_result = {"fetched": 0}

    # 5. Materialize runtime outputs into local project tree
    outputs_materialized = 0
    if bundle_hash:
        outputs_materialized = _materialize_runtime_outputs(
            bundle_hash, project_path,
        )

    return {
        "status": exec_body.get("status"),
        "thread_id": exec_body.get("thread_id"),
        "execution_snapshot_hash": snapshot_hash,
        "runtime_outputs_bundle_hash": bundle_hash,
        "result": exec_body.get("result"),
        "objects_pushed": push_result.get("objects_synced", 0),
        "objects_pulled": pull_result.get("fetched", 0),
        "outputs_materialized": outputs_materialized,
        "system_version": exec_body.get("system_version"),
    }


async def _threads(project_path: Path, params: Dict) -> Dict:
    """List remote executions from the server."""
    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    limit = params.get("limit", 20)
    proj_name = params.get("project_path")

    path = f"/threads?limit={limit}"
    if proj_name:
        path += f"&project_path={proj_name}"

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
# Secrets management
# ---------------------------------------------------------------------------


def _parse_env_file(env_path: Path) -> Dict[str, str]:
    """Parse a .env file into a dict of KEY=VALUE pairs.

    Skips comments and blank lines. Does not expand variables.
    """
    secrets = {}
    for line in env_path.read_text().splitlines():
        line = line.strip()
        if not line or line.startswith("#"):
            continue
        if "=" not in line:
            continue
        key, _, value = line.partition("=")
        key = key.strip()
        value = value.strip()
        if not key or not value:
            continue
        secrets[key] = value
    return secrets


async def _secrets_push(project_path: Path, params: Dict) -> Dict:
    """Push secrets from .env file or env vars to remote vault.

    The LLM never sees secret values — it only provides key names
    or an .env file path, and this function reads values directly.
    """
    env_file = params.get("env_file")
    names = params.get("names", [])

    # Resolve secrets locally before connecting to remote
    secrets_to_push: Dict[str, str] = {}

    if env_file:
        env_path = Path(env_file)
        if not env_path.is_absolute():
            env_path = project_path / env_path
        if not env_path.is_file():
            return {"error": f"File not found: {env_path}"}
        secrets_to_push = _parse_env_file(env_path)
    elif names:
        for name in names:
            value = os.environ.get(name, "")
            if value:
                secrets_to_push[name] = value
            else:
                logger.warning("Env var %s not set, skipping", name)
    else:
        return {
            "error": "Provide either env_file (path to .env) or names (list of env var names)",
        }

    if not secrets_to_push:
        return {"error": "No secrets found to push"}

    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    payload = [{"name": k, "value": v} for k, v in secrets_to_push.items()]
    resp = await client.post("/secrets", {"secrets": payload})
    if not resp["success"]:
        return {"error": f"Failed to push secrets: {resp['error']}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)

    return {
        "stored": body.get("stored", []),
        "count": body.get("count", 0),
        "message": f"Pushed {body.get('count', 0)} secrets to remote",
    }


async def _secrets_list(project_path: Path, params: Dict) -> Dict:
    """List secret names stored on remote (values are never returned)."""
    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    resp = await client.get("/secrets")
    if not resp["success"]:
        return {"error": f"Failed to list secrets: {resp['error']}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)

    secrets = body.get("secrets", [])
    return {
        "secrets": [s["name"] for s in secrets],
        "count": len(secrets),
    }


async def _secrets_remove(project_path: Path, params: Dict) -> Dict:
    """Remove a named secret from remote vault."""
    secret_name = params.get("secret_name")
    if not secret_name:
        return {"error": "secret_name is required for secrets_remove"}

    remote_name = params.get("remote")
    client = _get_client(remote_name, project_path)

    key_err = await _verify_remote_key(client, remote_name)
    if key_err:
        return key_err

    resp = await client.delete(f"/secrets/{secret_name}")
    if not resp["success"]:
        return {"error": f"Failed to remove secret: {resp['error']}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)

    return {
        "deleted": body.get("deleted", secret_name),
        "message": f"Removed secret '{secret_name}' from remote",
    }


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
    "secrets_push": _secrets_push,
    "secrets_list": _secrets_list,
    "secrets_remove": _secrets_remove,
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
