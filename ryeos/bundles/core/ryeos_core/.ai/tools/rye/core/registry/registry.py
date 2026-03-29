"""
Registry tool — publish, search, and pull items from a ryeos-node registry.

Identity model:
  item_id = "{namespace}/{category}/{name}" (canonical)
  - namespace: owner (no slashes), e.g., "leolilley"
  - category: folder path (may contain slashes), e.g., "core" or "rye/core/registry"
  - name: basename (no slashes), e.g., "bootstrap"

  Parsing: first segment = namespace, last segment = name, middle = category
  Example: "leolilley/rye/core/registry/registry"
           -> namespace="leolilley", category="rye/core/registry", name="registry"

Actions:
  login    - Create identity document and publish to registry node
  whoami   - Show current identity (local keypair fingerprint)
  search   - Search the registry index
  pull     - Download item from registry
  push     - Publish item to registry
  claim    - Claim a namespace
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/registry"
__tool_description__ = "Registry tool for publishing and pulling items"

import base64
import hashlib
import json
import logging
import os
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

from rye.constants import AI_DIR

try:
    from rye.utils.path_utils import ensure_directory
except ImportError:
    def ensure_directory(path: Path) -> Path:
        path = Path(path)
        path.mkdir(parents=True, exist_ok=True)
        return path

logger = logging.getLogger(__name__)

TOOL_METADATA = {
    "name": "registry",
    "description": "Registry operations: publish, pull, search, claim",
    "version": __version__,
    "protected": True,
}

ACTIONS = ["login", "whoami", "search", "pull", "push", "claim"]

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ACTIONS,
            "description": "Registry operation",
        },
        "item_type": {
            "type": "string",
            "description": "Item type (tool, directive, knowledge)",
        },
        "item_id": {
            "type": "string",
            "description": "Item ID (namespace/category/name)",
        },
        "version": {
            "type": "string",
            "description": "Version string",
        },
        "query": {
            "type": "string",
            "description": "Search query",
        },
        "namespace": {
            "type": "string",
            "description": "Namespace to claim or filter by",
        },
        "location": {
            "type": "string",
            "description": "Where to install: project or user (default: project)",
        },
        "limit": {
            "type": "integer",
            "description": "Max results (default: 20)",
        },
    },
    "required": ["action"],
}


# =============================================================================
# ITEM ID HELPERS
# =============================================================================


def parse_item_id(item_id: str) -> Tuple[str, str, str]:
    """Parse item_id into (namespace, category, name).

    Format: namespace/category/name where category may contain slashes.
    Minimum 3 segments required.
    """
    segments = item_id.split("/")
    if len(segments) < 3:
        raise ValueError(
            f"item_id must have at least 3 segments (namespace/category/name), got: {item_id}"
        )
    namespace = segments[0]
    name = segments[-1]
    category = "/".join(segments[1:-1])
    return namespace, category, name


def build_item_id(namespace: str, category: str, name: str) -> str:
    """Build item_id from components."""
    return f"{namespace}/{category}/{name}"


def build_item_id_from_path(
    file_path: Path,
    namespace: str,
    item_type: str,
    project_path: Optional[Path] = None,
) -> str:
    """Build item_id from a local file path."""
    from rye.utils.path_utils import extract_category_path

    name = file_path.stem
    category = extract_category_path(
        file_path, item_type, location="project", project_path=project_path
    )
    if not category:
        category = "uncategorized"
    return build_item_id(namespace, category, name)


_TYPE_DIRS = {
    "directive": "directives",
    "tool": "tools",
    "knowledge": "knowledge",
}


def _type_dir(item_type: str) -> str:
    """Get the .ai/ subdirectory name for an item type."""
    return _TYPE_DIRS.get(item_type, item_type)


def _find_local_item(
    item_type: str, local_item_id: str, project_path: Optional[str] = None
) -> Optional[Path]:
    """Resolve a local item file from item_type + local_item_id."""
    from rye.utils.extensions import get_item_extensions
    from rye.utils.path_utils import get_project_type_path

    if not project_path:
        return None

    base = get_project_type_path(Path(project_path), item_type)
    if not base.exists():
        return None

    extensions = get_item_extensions(item_type, Path(project_path))
    for ext in extensions:
        file_path = base / f"{local_item_id}{ext}"
        if file_path.is_file():
            return file_path

    return None


# =============================================================================
# CONFIG RESOLUTION
# =============================================================================


_REGISTRY_CONFIG_REL = "registry/registry.yaml"


def _resolve_registry_url(project_path=None) -> str:
    """Resolve registry node URL from 3-tier config."""
    from rye.cas.manifest import _load_config_3tier
    config = _load_config_3tier(_REGISTRY_CONFIG_REL, project_path)
    reg = config.get("registry", {})
    url = reg.get("url", "")
    if not url:
        raise ValueError(
            "Registry URL not configured. "
            "Set registry.url in .ai/config/registry/registry.yaml"
        )
    return url


# =============================================================================
# SIGNING KEYS
# =============================================================================


def _get_signing_key_dir() -> Path:
    """Get the signing key directory."""
    from rye.utils.path_utils import get_user_space
    return Path(get_user_space()) / "config" / "keys" / "signing"


def _get_signing_keys() -> Tuple[bytes, bytes]:
    """Load local Ed25519 keypair for signing requests."""
    from lillux.primitives.signing import load_keypair
    return load_keypair(_get_signing_key_dir())


def _get_fingerprint() -> str:
    """Get local key fingerprint."""
    from lillux.primitives.signing import compute_key_fingerprint
    _, pub = _get_signing_keys()
    return compute_key_fingerprint(pub)


# =============================================================================
# HTTP CLIENT
# =============================================================================


class RegistryHttpClient:
    """HTTP client for registry API calls."""

    def __init__(self, base_url: str):
        self.base_url = base_url.rstrip("/")
        self._http = None

    async def _get_http(self):
        if self._http is None:
            from rye.runtime.http_client import HttpClientPrimitive
            self._http = HttpClientPrimitive()
        return self._http

    async def get(self, path: str, headers: Optional[Dict] = None) -> Dict:
        http = await self._get_http()
        req_headers = {"Content-Type": "application/json"}
        if headers:
            req_headers.update(headers)
        config = {
            "method": "GET",
            "url": f"{self.base_url}{path}",
            "headers": req_headers,
            "timeout": 30,
        }
        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }

    async def post(self, path: str, body: Dict, headers: Optional[Dict] = None) -> Dict:
        http = await self._get_http()
        req_headers = {"Content-Type": "application/json"}
        if headers:
            req_headers.update(headers)
        config = {
            "method": "POST",
            "url": f"{self.base_url}{path}",
            "headers": req_headers,
            "body": body,
            "timeout": 30,
        }
        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }

    async def close(self):
        if self._http:
            await self._http.close()


# =============================================================================
# TOFU KEY PINNING
# =============================================================================


async def _fetch_registry_identity(
    client: RegistryHttpClient,
    registry_name: str = "registry",
) -> Tuple[Optional[Dict], Optional[Dict]]:
    """Fetch and verify registry node identity, TOFU pin the key.

    Returns (identity_doc, None) on success, (None, error_dict) on failure.
    """
    resp = await client.get("/public-key")
    if not resp["success"]:
        return None, {"error": f"Could not fetch registry identity: {resp.get('error')}"}

    body = resp["body"]
    if isinstance(body, str):
        body = json.loads(body)

    signing_key_str = body.get("signing_key", "")
    if not signing_key_str.startswith("ed25519:"):
        return None, {"error": "Registry has invalid signing_key format"}

    remote_pem = base64.b64decode(signing_key_str.removeprefix("ed25519:"))

    sig_block = body.get("_signature")
    if sig_block:
        from lillux.primitives.signing import verify_signature
        payload = json.dumps(
            {k: v for k, v in body.items() if k != "_signature"},
            sort_keys=True, separators=(",", ":"),
        )
        content_hash = hashlib.sha256(payload.encode()).hexdigest()
        if not verify_signature(content_hash, sig_block["sig"], remote_pem):
            return None, {"error": "Registry identity signature verification failed"}

    from lillux.primitives.signing import compute_key_fingerprint
    from rye.utils.trust_store import TrustStore
    from urllib.parse import urlparse

    remote_fp = compute_key_fingerprint(remote_pem)
    trust_store = TrustStore()
    host = urlparse(client.base_url).netloc
    owner = f"registry:{registry_name}:{host}"
    pinned_key = trust_store.get_remote_key(remote_name=owner)

    if pinned_key is None:
        trust_store.pin_remote_key(remote_pem, remote_name=owner)
        logger.info("Pinned registry server key (TOFU): %s", remote_fp)
        return body, None

    pinned_fp = compute_key_fingerprint(pinned_key)
    if remote_fp == pinned_fp:
        return body, None

    return None, {
        "error": (
            f"Registry key mismatch (pinned: {pinned_fp}, remote: {remote_fp}). "
            f"To re-pin, remove the old key via "
            f"'rye execute tool rye/core/keys action=remove fingerprint={pinned_fp}' "
            f"then retry."
        ),
    }


# =============================================================================
# AUTHENTICATED REQUEST HELPERS
# =============================================================================


async def _authed_get(
    client: RegistryHttpClient,
    path: str,
    audience: str,
    priv: bytes,
    pub: bytes,
) -> Dict:
    from request_signing import sign_request
    headers = sign_request("GET", path, None, audience, priv, pub)
    return await client.get(path, headers=headers)


async def _authed_post(
    client: RegistryHttpClient,
    path: str,
    body: Dict,
    audience: str,
    priv: bytes,
    pub: bytes,
) -> Dict:
    from request_signing import sign_request
    body_bytes = json.dumps(body).encode()
    headers = sign_request("POST", path, body_bytes, audience, priv, pub)
    return await client.post(path, body, headers=headers)


async def _get_client_and_audience(project_path=None) -> Tuple[RegistryHttpClient, str, Optional[Dict]]:
    """Create client, verify identity, return (client, audience, error_or_None)."""
    url = _resolve_registry_url(project_path)
    client = RegistryHttpClient(url)
    identity_doc, err = await _fetch_registry_identity(client)
    if err:
        return client, "", err
    audience = identity_doc["principal_id"]
    return client, audience, None


# =============================================================================
# ACTIONS
# =============================================================================


async def _login(params: Dict, project_path: str) -> Dict:
    """Create identity document and publish to registry node."""
    from lillux.primitives.signing import ensure_full_keypair, compute_key_fingerprint
    from sign_object import sign_object

    key_dir = _get_signing_key_dir()
    priv, pub, _box_key, box_pub = ensure_full_keypair(key_dir)
    fp = compute_key_fingerprint(pub)

    identity = {
        "kind": "identity/v1",
        "principal_id": f"fp:{fp}",
        "signing_key": f"ed25519:{base64.b64encode(pub).decode()}",
        "box_key": f"x25519:{base64.b64encode(box_pub).decode()}",
    }

    signed_identity = sign_object(identity, priv, pub)

    client, audience, err = await _get_client_and_audience(project_path)
    if err:
        return err

    result = await _authed_post(
        client, "/registry/identity",
        {"identity": signed_identity},
        audience, priv, pub,
    )

    if not result["success"]:
        body = result.get("body", {})
        detail = body.get("detail", result.get("error", "Unknown error")) if isinstance(body, dict) else result.get("error")
        return {"error": f"Identity registration failed: {detail}"}

    return {
        "status": "registered",
        "fingerprint": fp,
        "principal_id": f"fp:{fp}",
        "message": f"Identity published to registry",
    }


async def _whoami(params: Dict, project_path: str) -> Dict:
    """Show current identity (local keypair fingerprint)."""
    try:
        from lillux.primitives.signing import compute_key_fingerprint
        _, pub = _get_signing_keys()
        fp = compute_key_fingerprint(pub)
        return {
            "fingerprint": fp,
            "principal_id": f"fp:{fp}",
            "key_dir": str(_get_signing_key_dir()),
        }
    except FileNotFoundError:
        return {
            "error": "No signing key found",
            "hint": "Run 'registry login' to create one",
        }


async def _search(params: Dict, project_path: str) -> Dict:
    """Search the registry index."""
    query = params.get("query")
    if not query:
        return {"error": "Required: query", "usage": "search(query='bootstrap')"}

    client, audience, err = await _get_client_and_audience(project_path)
    if err:
        return err

    priv, pub = _get_signing_keys()

    search_params = f"?query={query}"
    item_type = params.get("item_type")
    namespace = params.get("namespace")
    limit = params.get("limit", 20)
    if item_type:
        search_params += f"&item_type={item_type}"
    if namespace:
        search_params += f"&namespace={namespace}"
    search_params += f"&limit={limit}"

    path = f"/registry/search{search_params}"
    result = await _authed_get(client, path, audience, priv, pub)

    if not result["success"]:
        return {"error": f"Search failed: {result.get('error')}"}

    body = result.get("body", {})
    if isinstance(body, str):
        body = json.loads(body)

    return {
        "status": "success",
        "query": query,
        "results": body.get("results", []),
        "total": body.get("total", 0),
    }


async def _pull(params: Dict, project_path: str) -> Dict:
    """Download item from registry.

    Gets version metadata from registry index, then uses CAS sync
    to pull the manifest and objects, verifies publisher signature.
    """
    item_type = params.get("item_type")
    item_id = params.get("item_id")
    version = params.get("version")
    location = params.get("location", "project")

    if not item_type or not item_id:
        return {
            "error": "Required: item_type and item_id",
            "usage": "pull(item_type='tool', item_id='alice/core/my_tool')",
        }

    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        return {"error": str(e)}

    if item_type not in ("directive", "tool", "knowledge"):
        return {"error": f"Invalid item_type: {item_type}"}

    client, audience, err = await _get_client_and_audience(project_path)
    if err:
        return err

    priv, pub = _get_signing_keys()

    # Get item metadata from registry
    if version:
        path = f"/registry/items/{item_type}/{item_id}/versions/{version}"
    else:
        path = f"/registry/items/{item_type}/{item_id}"

    result = await _authed_get(client, path, audience, priv, pub)

    if not result["success"]:
        return {
            "error": f"Item not found: {item_type}/{item_id}",
            "status_code": result.get("status_code"),
        }

    body = result.get("body", {})
    if isinstance(body, str):
        body = json.loads(body)

    # If we got the item (not a specific version), get the latest version info
    if not version:
        latest = body.get("latest_version")
        if not latest:
            return {"error": "No versions published for this item"}
        version_info = body.get("versions", {}).get(latest)
        if not version_info:
            return {"error": f"Latest version {latest} not found"}
        version = latest
    else:
        version_info = body

    manifest_hash = version_info.get("manifest_hash")
    if not manifest_hash:
        return {"error": "No manifest hash in version metadata"}

    # CAS sync: pull the manifest object
    from rye.cas.store import cas_root as get_cas_root
    from rye.cas.sync import import_objects

    root = get_cas_root(Path(project_path) if project_path else Path("."))

    # Fetch objects from registry node via CAS endpoints
    get_resp = await _authed_post(
        client, "/objects/get",
        {"hashes": [manifest_hash]},
        audience, priv, pub,
    )

    if not get_resp["success"]:
        return {"error": f"Failed to fetch manifest: {get_resp.get('error')}"}

    get_body = get_resp.get("body", {})
    if isinstance(get_body, str):
        get_body = json.loads(get_body)

    entries = get_body.get("entries", [])
    if entries:
        import_objects(entries, root)

    # Load manifest and pull remaining objects
    from lillux.primitives import cas
    manifest = cas.get_object(manifest_hash, root)
    if manifest is None:
        return {"error": "Failed to load manifest after pull"}

    # Pull all referenced objects
    all_hashes = []
    for h in manifest.get("items", {}).values():
        all_hashes.append(h)
    for h in manifest.get("files", {}).values():
        all_hashes.append(h)

    if all_hashes:
        # Check which we already have
        missing = [h for h in all_hashes if not cas.has(h, root)]
        if missing:
            get_resp2 = await _authed_post(
                client, "/objects/get",
                {"hashes": missing},
                audience, priv, pub,
            )
            if get_resp2["success"]:
                body2 = get_resp2.get("body", {})
                if isinstance(body2, str):
                    body2 = json.loads(body2)
                import_objects(body2.get("entries", []), root)

    # Determine destination
    if location == "user":
        from rye.utils.path_utils import get_user_space
        base_dir = Path(get_user_space())
    else:
        base_dir = Path(project_path) / AI_DIR if project_path else Path(AI_DIR)

    # Materialize items from manifest
    items_written = []
    for rel_path, item_hash in manifest.get("items", {}).items():
        item_obj = cas.get_object(item_hash, root)
        if item_obj is None:
            continue
        content_hash = item_obj.get("content_hash")
        if content_hash:
            blob = cas.get_blob(content_hash, root)
            if blob:
                dest = base_dir / rel_path
                ensure_directory(dest.parent)
                dest.write_bytes(blob)
                items_written.append(rel_path)

    return {
        "status": "pulled",
        "item_type": item_type,
        "item_id": item_id,
        "version": version,
        "manifest_hash": manifest_hash,
        "location": location,
        "items_written": items_written,
    }


async def _push(params: Dict, project_path: str) -> Dict:
    """Publish item to registry.

    Syncs CAS objects to the node, then registers the item in the index.
    """
    item_type = params.get("item_type")
    item_id = params.get("item_id")
    version = params.get("version")

    if not item_type or not item_id:
        return {
            "error": "Required: item_type, item_id",
            "usage": "push(item_type='tool', item_id='alice/core/my_tool')",
        }

    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        return {"error": str(e)}

    if item_type not in ("directive", "tool", "knowledge"):
        return {"error": f"Invalid item_type: {item_type}"}

    # Resolve local file
    local_item_id = f"{category}/{name}"
    path = _find_local_item(item_type, local_item_id, project_path)
    if not path:
        return {
            "error": f"Item not found: {local_item_id}",
            "hint": f"Expected at .ai/{_type_dir(item_type)}/{local_item_id}.*",
        }

    # Extract version from file if not provided
    if not version:
        content = path.read_text()
        # Try extracting __version__ for Python tools
        for line in content.splitlines():
            stripped = line.strip()
            if stripped.startswith("__version__"):
                try:
                    version = stripped.split("=", 1)[1].strip().strip("\"'")
                    break
                except (IndexError, ValueError):
                    pass

    if not version:
        return {
            "error": "Version required but not found",
            "hint": "Set __version__ in the file or pass version parameter",
        }

    # Build manifest for this item via CAS
    from rye.cas.manifest import build_manifest
    from rye.cas.store import cas_root as get_cas_root
    from rye.cas.sync import collect_object_hashes, export_objects

    root = get_cas_root(Path(project_path))
    manifest_hash, manifest = build_manifest(Path(project_path), "project")

    client, audience, err = await _get_client_and_audience(project_path)
    if err:
        return err

    priv, pub = _get_signing_keys()

    # Sync CAS objects to registry node
    all_hashes = list(set(collect_object_hashes(manifest, root) + [manifest_hash]))

    has_resp = await _authed_post(
        client, "/objects/has",
        {"hashes": all_hashes},
        audience, priv, pub,
    )

    if not has_resp["success"]:
        return {"error": f"Failed to check objects: {has_resp.get('error')}"}

    has_body = has_resp.get("body", {})
    if isinstance(has_body, str):
        has_body = json.loads(has_body)
    missing = has_body.get("missing", [])

    if missing:
        entries = export_objects(missing, root)
        put_resp = await _authed_post(
            client, "/objects/put",
            {"entries": [e if isinstance(e, dict) else e.to_dict() for e in entries]},
            audience, priv, pub,
        )
        if not put_resp["success"]:
            return {"error": f"Failed to upload objects: {put_resp.get('error')}"}

    # Register in registry index
    result = await _authed_post(
        client, "/registry/publish",
        {
            "item_type": item_type,
            "item_id": item_id,
            "version": version,
            "manifest_hash": manifest_hash,
        },
        audience, priv, pub,
    )

    if not result["success"]:
        body = result.get("body", {})
        detail = body.get("detail", result.get("error", "Unknown error")) if isinstance(body, dict) else result.get("error")
        return {"error": f"Publish failed: {detail}"}

    return {
        "status": "published",
        "item_type": item_type,
        "item_id": item_id,
        "version": version,
        "manifest_hash": manifest_hash,
        "objects_synced": len(missing),
    }


async def _claim(params: Dict, project_path: str) -> Dict:
    """Claim a namespace."""
    namespace = params.get("namespace")
    if not namespace:
        return {"error": "Required: namespace", "usage": "claim(namespace='myname')"}

    from lillux.primitives.signing import compute_key_fingerprint
    from sign_object import sign_object

    priv, pub = _get_signing_keys()
    fp = compute_key_fingerprint(pub)

    claim = {
        "kind": "namespace-claim/v1",
        "namespace": namespace,
        "owner": f"fp:{fp}",
        "created_at": _now_iso(),
    }
    signed_claim = sign_object(claim, priv, pub)

    client, audience, err = await _get_client_and_audience(project_path)
    if err:
        return err

    result = await _authed_post(
        client, "/registry/namespaces/claim",
        {"claim": signed_claim},
        audience, priv, pub,
    )

    if not result["success"]:
        body = result.get("body", {})
        detail = body.get("detail", result.get("error", "Unknown error")) if isinstance(body, dict) else result.get("error")
        return {"error": f"Claim failed: {detail}"}

    return {
        "status": "claimed",
        "namespace": namespace,
        "owner": f"fp:{fp}",
    }


def _now_iso() -> str:
    from datetime import datetime, timezone
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


# =============================================================================
# ENTRY POINT
# =============================================================================


_ACTION_MAP = {
    "login": _login,
    "whoami": _whoami,
    "search": _search,
    "pull": _pull,
    "push": _push,
    "claim": _claim,
}


async def execute(params: dict, project_path: str) -> dict:
    """Entry point for function runtime."""
    action = params.pop("action", None)
    if not action:
        return {"success": False, "error": "action required in params"}
    if action not in ACTIONS:
        return {"success": False, "error": f"Unknown action: {action}", "valid_actions": ACTIONS}

    try:
        result = await _ACTION_MAP[action](params, project_path)
    except ValueError as e:
        result = {"error": str(e)}
    except Exception as e:
        logger.exception("Registry action %s failed", action)
        result = {"error": f"{action} failed: {e}"}

    if "error" in result:
        result["success"] = False
    elif "success" not in result:
        result["success"] = True
    return result


# =============================================================================
# REMOTE SPACE PROVIDER
# =============================================================================


class RegistryProvider:
    """RegistrySpaceProvider implementation for the Rye Registry.

    Wraps the _search and _pull functions behind the RegistrySpaceProvider
    protocol. Discovered via bundle entry point.
    """

    @property
    def provider_id(self) -> str:
        return "registry"

    async def search(
        self,
        *,
        query: str,
        item_type: str,
        limit: int = 10,
    ) -> List[Dict[str, Any]]:
        """Search the registry, returning normalized result dicts."""
        result = await _search(
            {"query": query, "item_type": item_type, "limit": limit},
            project_path=".",
        )
        if result.get("error"):
            return []

        results: List[Dict[str, Any]] = []
        for item in result.get("results", []):
            results.append({
                "id": item.get("item_id", ""),
                "name": item.get("item_id", "").rsplit("/", 1)[-1],
                "description": "",
                "type": item.get("item_type", item_type),
                "source": "registry",
                "score": 0.5,
                "metadata": {
                    "version": item.get("latest_version", ""),
                    "namespace": item.get("namespace", ""),
                    "owner": item.get("owner", ""),
                },
            })
        return results

    async def pull(
        self,
        *,
        item_type: str,
        item_id: str,
        version: Optional[str] = None,
    ) -> Dict[str, Any]:
        """Pull item from registry."""
        return await _pull(
            {"item_type": item_type, "item_id": item_id, "version": version},
            project_path=".",
        )


def get_provider() -> RegistryProvider:
    """Return a RegistryProvider instance for remote space discovery."""
    return RegistryProvider()
