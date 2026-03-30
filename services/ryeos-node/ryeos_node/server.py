"""CAS-native remote execution server.

Endpoints: /health, /public-key, /objects/has, /objects/put, /objects/get,
           /push, /push/user-space, /user-space,
           /execute,
           /threads, /threads/{thread_id},
           /webhook-bindings (POST, GET), /webhook-bindings/{hook_id} (DELETE)
"""

import asyncio
import datetime
import hashlib
import json
import logging
import os
import random
import shutil
import threading
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

from fastapi import Depends, FastAPI, HTTPException, Request, status
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.middleware.gzip import GZipMiddleware

from ryeos_node.auth import (
    Principal,
    ResolvedExecution,
    _verify_signed_request,
    get_current_principal,
    require_capability,
    verify_hmac,
    verify_timestamp,
)
from ryeos_node.config import Settings, get_settings
from ryeos_node.refs import (
    advance_project_ref,
    resolve_project_ref,
    resolve_user_space_ref,
    advance_user_space_ref,
)
from ryeos_node.executions import (
    register_execution,
    complete_execution,
    list_executions,
    get_execution,
    store_conflict_record,
)
from ryeos_node.webhooks import (
    create_binding,
    list_bindings,
    resolve_binding,
    revoke_binding,
)
from ryeos_node.registry import (
    load_index,
    publish_item,
    search_items,
    get_item,
    get_version,
    claim_namespace,
    register_identity,
    lookup_identity,
)

from rye.primitives import cas
from rye.primitives.integrity import compute_integrity

from rye.cas.objects import ExecutionSnapshot, ProjectSnapshot, RuntimeOutputsBundle, SourceManifest, SCHEMA_VERSION, get_history
from rye.cas.sync import (
    handle_has_objects,
    handle_put_objects,
    handle_get_objects,
)
from rye.cas.materializer import get_system_version
from rye.cas.checkout import (
    cleanup_execution_space,
    create_execution_space,
    ensure_snapshot_cached,
    ensure_user_space_cached,
)
from rye.cas.manifest import build_manifest
from rye.cas.merge import three_way_merge
from rye.constants import AI_DIR
from rye.actions.execute import ExecuteTool

logger = logging.getLogger(__name__)


class _ExecutionCounter:
    """Thread-safe active execution counter."""
    def __init__(self):
        self._lock = threading.Lock()
        self._active = 0

    @property
    def active(self) -> int:
        with self._lock:
            return self._active

    def increment(self) -> int:
        with self._lock:
            self._active += 1
            return self._active

    def decrement(self) -> int:
        with self._lock:
            self._active = max(0, self._active - 1)
            return self._active

_exec_counter = _ExecutionCounter()

app = FastAPI(title="ryeos-node", version="0.1.0")

# m3: Gzip compression for responses
app.add_middleware(GZipMiddleware, minimum_size=1000)


# m1: Enforce batch size limits (reads actual body, not just Content-Length header)
@app.middleware("http")
async def enforce_request_size(request: Request, call_next):
    if request.url.path in ("/health", "/status"):
        return await call_next(request)
    settings = get_settings()
    limit = settings.max_request_bytes

    # Fast reject via Content-Length header if present
    content_length = request.headers.get("content-length")
    if content_length and int(content_length) > limit:
        return JSONResponse(
            status_code=413,
            content={"detail": f"Request body exceeds {limit} bytes"},
        )

    # Stream-based enforcement for requests without Content-Length
    if request.method in ("POST", "PUT", "PATCH"):
        body = await request.body()
        if len(body) > limit:
            return JSONResponse(
                status_code=413,
                content={"detail": f"Request body exceeds {limit} bytes"},
            )

    return await call_next(request)


# --- Request/Response models ---


class HasObjectsRequest(BaseModel):
    hashes: List[str]


class PutObjectsRequest(BaseModel):
    entries: List[Dict[str, str]]


class GetObjectsRequest(BaseModel):
    hashes: List[str]


class PushRequest(BaseModel):
    project_path: str
    project_manifest_hash: str
    system_version: str
    expected_snapshot_hash: Optional[str] = None  # None = first push


class PushUserSpaceRequest(BaseModel):
    user_manifest_hash: str
    expected_revision: Optional[int] = None  # None = first push


class CreateWebhookBindingRequest(BaseModel):
    item_type: str
    item_id: str
    project_path: str
    description: Optional[str] = None


class PublishRequest(BaseModel):
    item_type: str
    item_id: str
    version: str
    manifest_hash: str


class ClaimNamespaceRequest(BaseModel):
    claim: dict


class RegisterIdentityRequest(BaseModel):
    identity: dict


# --- Helpers ---


def _principal_cas_root(principal: Principal, settings: Settings) -> Path:
    return settings.user_cas_root(principal.fingerprint)


def _check_user_quota(principal: Principal, settings: Settings) -> None:
    """Reject if principal CAS exceeds storage quota."""
    root = _principal_cas_root(principal, settings)
    if not root.exists():
        return
    total = sum(f.stat().st_size for f in root.rglob("*") if f.is_file())
    if total > settings.max_user_storage_bytes:
        raise HTTPException(
            status.HTTP_507_INSUFFICIENT_STORAGE,
            f"User storage quota exceeded ({total} bytes > {settings.max_user_storage_bytes})",
        )


def _check_system_version(client_version: str) -> None:
    """Reject on major/minor mismatch."""
    server_version = get_system_version()
    if server_version == "unknown":
        return

    def _major_minor(v: str) -> str:
        parts = v.split(".")
        return ".".join(parts[:2]) if len(parts) >= 2 else v

    if _major_minor(client_version) != _major_minor(server_version):
        raise HTTPException(
            status.HTTP_409_CONFLICT,
            f"Version mismatch: client={client_version}, server={server_version}",
        )


def _validate_manifest_graph(
    manifest_hash: str,
    root: Path,
    *,
    expected_space: str,
    label: str,
) -> dict:
    """Validate a manifest and its full transitive object graph.

    Verifies:
    - Manifest exists and has correct kind/schema/space
    - All item references point to valid item_source objects with existing content blobs
    - All file references point to existing blobs

    Returns the validated manifest dict.
    Raises HTTPException(400) on any validation failure.
    """
    manifest = cas.get_object(manifest_hash, root)
    if manifest is None:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} object {manifest_hash} not found in CAS. Upload objects first.",
        )
    if manifest.get("kind") != "source_manifest":
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} {manifest_hash} has kind={manifest.get('kind')!r}, expected 'source_manifest'",
        )
    if manifest.get("schema") != SCHEMA_VERSION:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} {manifest_hash} has unsupported schema version {manifest.get('schema')!r}",
        )
    if manifest.get("space") != expected_space:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} {manifest_hash} has space={manifest.get('space')!r}, expected {expected_space!r}",
        )

    items = manifest.get("items", {})
    files = manifest.get("files", {})
    if not isinstance(items, dict):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} {manifest_hash} has invalid items (expected object)",
        )
    if not isinstance(files, dict):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"{label} {manifest_hash} has invalid files (expected object)",
        )

    # Dedupe: avoid re-checking the same hash multiple times
    validated_items: dict[str, dict] = {}
    validated_blobs: set[str] = set()

    for rel_path, item_hash in items.items():
        if item_hash not in validated_items:
            item_obj = cas.get_object(item_hash, root)
            if item_obj is None:
                raise HTTPException(
                    status.HTTP_400_BAD_REQUEST,
                    f"{label} item '{rel_path}' references missing object {item_hash}",
                )
            if item_obj.get("kind") != "item_source":
                raise HTTPException(
                    status.HTTP_400_BAD_REQUEST,
                    f"{label} item '{rel_path}' references object {item_hash} "
                    f"with kind={item_obj.get('kind')!r}, expected 'item_source'",
                )
            blob_hash = item_obj.get("content_blob_hash")
            if not isinstance(blob_hash, str) or not blob_hash:
                raise HTTPException(
                    status.HTTP_400_BAD_REQUEST,
                    f"item_source {item_hash} for '{rel_path}' is missing content_blob_hash",
                )
            validated_items[item_hash] = item_obj

        blob_hash = validated_items[item_hash]["content_blob_hash"]
        if blob_hash not in validated_blobs:
            if not cas.has_blob(blob_hash, root):
                raise HTTPException(
                    status.HTTP_400_BAD_REQUEST,
                    f"item_source {item_hash} for '{rel_path}' references missing blob {blob_hash}",
                )
            validated_blobs.add(blob_hash)

    for rel_path, blob_hash in files.items():
        if blob_hash not in validated_blobs:
            if not cas.has_blob(blob_hash, root):
                raise HTTPException(
                    status.HTTP_400_BAD_REQUEST,
                    f"{label} file '{rel_path}' references missing blob {blob_hash}",
                )
            validated_blobs.add(blob_hash)

    return manifest


def _copy_cas_objects(src_root: Path, dst_root: Path) -> List[str]:
    """Re-ingest CAS blobs and objects from src into dst via CAS primitives.

    Validates content integrity on each object (recomputed hash must match).
    Raises RuntimeError if any integrity violations are detected.
    Returns list of new hashes stored in dst.
    """
    new_hashes: List[str] = []
    errors: List[str] = []

    # Copy blobs
    blobs_dir = src_root / "blobs"
    if blobs_dir.is_dir():
        for dirpath, _, filenames in os.walk(blobs_dir):
            for filename in filenames:
                src_file = Path(dirpath) / filename
                raw = src_file.read_bytes()
                computed_hash = hashlib.sha256(raw).hexdigest()
                if computed_hash != filename:
                    errors.append(
                        f"Blob hash mismatch: file={filename}, computed={computed_hash}"
                    )
                    continue
                cas.store_blob(raw, dst_root)
                new_hashes.append(computed_hash)

    # Copy objects
    objects_dir = src_root / "objects"
    if objects_dir.is_dir():
        for dirpath, _, filenames in os.walk(objects_dir):
            for filename in filenames:
                if not filename.endswith(".json"):
                    continue
                src_file = Path(dirpath) / filename
                try:
                    obj = json.loads(src_file.read_bytes())
                except (json.JSONDecodeError, UnicodeDecodeError):
                    errors.append(f"Invalid CAS object file: {src_file}")
                    continue
                expected_hash = filename.removesuffix(".json")
                computed_hash = compute_integrity(obj)
                if computed_hash != expected_hash:
                    errors.append(
                        f"Object hash mismatch: file={expected_hash}, computed={computed_hash}"
                    )
                    continue
                cas.store_object(obj, dst_root)
                new_hashes.append(computed_hash)

    if errors:
        raise RuntimeError(
            f"CAS integrity violations ({len(errors)}):\n" + "\n".join(errors)
        )

    return new_hashes


# Allowlisted path prefixes for runtime outputs (relative to project root).
_RUNTIME_OUTPUT_PREFIXES = (
    f"{AI_DIR}/agent/",
    f"{AI_DIR}/knowledge/agent/",
    f"{AI_DIR}/objects/refs/",
)


def _ingest_runtime_outputs(
    project_path: Path,
    dst_root: Path,
    thread_id: str,
    snapshot_hash: str,
) -> tuple[str, List[str]]:
    """Ingest runtime-produced files into CAS as blobs + RuntimeOutputsBundle.

    Walks allowlisted paths under the materialized project, stores each
    regular file as a CAS blob, builds a RuntimeOutputsBundle mapping
    {relative_path: blob_hash}, stores it as a CAS object.

    Returns (bundle_hash, all_new_hashes) where all_new_hashes includes
    the bundle object hash and all referenced blob hashes.

    Rejects symlinks and paths that escape the project root.
    """
    files: Dict[str, str] = {}
    new_hashes: List[str] = []

    resolved_root = project_path.resolve()

    for prefix in _RUNTIME_OUTPUT_PREFIXES:
        prefix_path = project_path / prefix
        if not prefix_path.is_dir():
            continue

        for dirpath, _, filenames in os.walk(prefix_path):
            for filename in filenames:
                file_path = Path(dirpath) / filename

                # Reject symlinks (prevents exfiltration of server files)
                if file_path.is_symlink():
                    logger.warning(
                        "Skipping symlink in runtime outputs: %s", file_path
                    )
                    continue

                # Validate path stays under project root
                try:
                    resolved = file_path.resolve()
                    if not resolved.is_relative_to(resolved_root):
                        logger.warning(
                            "Path escapes project root: %s", file_path
                        )
                        continue
                except (ValueError, OSError):
                    continue

                if not file_path.is_file():
                    continue

                rel_path = str(file_path.relative_to(project_path))

                try:
                    raw = file_path.read_bytes()
                    blob_hash = cas.store_blob(raw, dst_root)
                    files[rel_path] = blob_hash
                    new_hashes.append(blob_hash)
                except Exception:
                    logger.warning(
                        "Failed to ingest runtime output: %s",
                        rel_path, exc_info=True,
                    )

    if not files:
        return "", []

    bundle = RuntimeOutputsBundle(
        remote_thread_id=thread_id,
        execution_snapshot_hash=snapshot_hash,
        files=files,
    )
    bundle_hash = cas.store_object(bundle.to_dict(), dst_root)
    new_hashes.append(bundle_hash)

    logger.info(
        "Ingested %d runtime output files (%s) for %s",
        len(files), bundle_hash[:16], thread_id,
    )

    return bundle_hash, new_hashes


def _resolve_project_ref_or_404(
    settings: Settings,
    principal: Principal,
    project_path: str,
) -> Dict[str, Any]:
    """Look up project ref from local filesystem.

    Returns dict with snapshot_hash, project_path.
    Raises HTTPException(404) if not found.
    """
    ref = resolve_project_ref(settings.cas_base_path, principal.fingerprint, project_path)
    if not ref:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"No project ref '{project_path}' found on remote '{settings.rye_remote_name}'. "
            f"Push first: rye execute tool rye/core/remote/remote action=push",
        )
    return ref


def _find_execution_snapshot_hash(project_path: Path) -> Optional[str]:
    """Find the walker's real execution_snapshot hash from graph refs."""
    refs_dir = project_path / AI_DIR / "objects" / "refs" / "graphs"
    if not refs_dir.is_dir():
        return None
    refs = sorted(
        [f for f in refs_dir.iterdir() if f.suffix == ".json"],
        key=lambda f: f.stat().st_mtime,
        reverse=True,
    )
    for ref_file in refs:
        try:
            data = json.loads(ref_file.read_bytes())
            return data.get("hash")
        except Exception:
            logger.warning("Corrupted graph ref %s", ref_file, exc_info=True)
            continue
    return None


# --- Endpoints ---


@app.get("/health")
async def health():
    return {"status": "ok", "version": get_system_version()}


def _scan_capabilities() -> tuple[list, list]:
    """Scan system bundle tools for capability classification."""
    provides = []
    routes = []
    try:
        from rye.utils.path_utils import get_system_spaces
        from rye.constants import AI_DIR

        for bundle in get_system_spaces():
            tools_dir = bundle.root_path / AI_DIR / "tools"
            if not tools_dir.is_dir():
                continue
            for file_path in tools_dir.rglob("*"):
                if not file_path.is_file() or file_path.name.startswith("_"):
                    continue
                if file_path.suffix not in (".py", ".md", ".yaml", ".yml"):
                    continue
                rel = file_path.relative_to(tools_dir)
                tool_id = str(rel.with_suffix(""))
                cap = f"rye.execute.tool.{tool_id.replace('/', '.')}"
                try:
                    head = file_path.read_text(errors="replace")[:2048]
                    if "__execution__" in head:
                        for line in head.splitlines():
                            if line.strip().startswith("__execution__"):
                                val = line.split("=", 1)[1].strip().strip("\"'")
                                if val == "routed":
                                    routes.append(cap)
                                    break
                        else:
                            provides.append(cap)
                    else:
                        provides.append(cap)
                except Exception:
                    provides.append(cap)
    except Exception:
        logger.warning("Failed to scan tools for /status", exc_info=True)
    return provides, routes


@app.get("/status")
async def node_status(settings: Settings = Depends(get_settings)):
    """Node status for routing decisions. No auth required."""
    import base64
    from rye.primitives.signing import compute_key_fingerprint, ensure_full_keypair

    key_dir = Path(settings.signing_key_dir)
    _, pub, _, _ = ensure_full_keypair(key_dir)
    node_id = f"fp:{compute_key_fingerprint(pub)}"

    provides, routes = _scan_capabilities()

    response = {
        "node_id": node_id,
        "node_name": settings.rye_remote_name,
        "healthy": True,
        "active": _exec_counter.active,
        "max_concurrent": settings.max_concurrent,
        "capabilities": {
            "provides": provides,
            "routes": routes,
        },
    }

    hardware = settings.hardware_descriptors()
    if hardware:
        response["hardware"] = hardware

    return response


_identity_cache: dict | None = None


@app.get("/public-key")
async def public_key(settings: Settings = Depends(get_settings)):
    """Return the node's signed identity document (signing + box keys)."""
    global _identity_cache
    if _identity_cache is not None:
        return _identity_cache

    import base64
    from rye.primitives.signing import (
        compute_key_fingerprint,
        ensure_full_keypair,
    )

    key_dir = Path(settings.signing_key_dir)
    priv, pub, box_key, box_pub = ensure_full_keypair(key_dir)
    fingerprint = compute_key_fingerprint(pub)

    identity_doc = {
        "kind": "identity/v1",
        "principal_id": f"fp:{fingerprint}",
        "signing_key": f"ed25519:{base64.b64encode(pub).decode()}",
        "box_key": f"x25519:{base64.b64encode(box_pub).decode()}",
        "created_at": datetime.datetime.now(datetime.timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ"),
    }

    # Sign with the node's own key
    import hashlib as _hl
    from rye.primitives.signing import sign_hash
    payload = json.dumps(
        {k: v for k, v in identity_doc.items() if k != "_signature"},
        sort_keys=True, separators=(",", ":"),
    )
    content_hash = _hl.sha256(payload.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, priv)

    identity_doc["_signature"] = {
        "signer": f"fp:{fingerprint}",
        "sig": sig_b64,
        "signed_at": identity_doc["created_at"],
    }

    _identity_cache = identity_doc
    return _identity_cache


@app.post("/objects/has")
async def objects_has(
    req: HasObjectsRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    require_capability(principal, "rye.objects.*")
    root = _principal_cas_root(principal, settings)
    return handle_has_objects(req.hashes, root)


@app.post("/objects/put")
async def objects_put(
    req: PutObjectsRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    require_capability(principal, "rye.objects.*")
    _check_user_quota(principal, settings)
    root = _principal_cas_root(principal, settings)
    result = handle_put_objects(req.entries, root)
    if result.get("errors"):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, result)
    return result


@app.post("/objects/get")
async def objects_get(
    req: GetObjectsRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    require_capability(principal, "rye.objects.*")
    root = _principal_cas_root(principal, settings)
    return handle_get_objects(req.hashes, root)


@app.post("/push")
async def push(
    req: PushRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Finalize a push — validate manifest graph, create snapshot, advance HEAD."""
    require_capability(principal, "rye.push.*")
    root = _principal_cas_root(principal, settings)

    # Deep validation: verify manifest schema + full transitive object graph
    _validate_manifest_graph(
        req.project_manifest_hash,
        root,
        expected_space="project",
        label="project_manifest",
    )

    if req.system_version:
        _check_system_version(req.system_version)

    # Resolve current HEAD (if any)
    ref = resolve_project_ref(settings.cas_base_path, principal.fingerprint, req.project_path)
    current_head = ref["snapshot_hash"] if ref else None

    # Reject if client is behind (expected_snapshot_hash mismatch)
    if req.expected_snapshot_hash is not None or current_head is not None:
        if req.expected_snapshot_hash != current_head:
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                {
                    "error": "HEAD has moved",
                    "expected": req.expected_snapshot_hash,
                    "actual": current_head,
                },
            )

    # Resolve user space hash for snapshot (may be None if never pushed)
    user_ref = resolve_user_space_ref(settings.cas_base_path, principal.fingerprint)
    user_manifest_hash = user_ref["user_manifest_hash"] if user_ref else None

    # Create ProjectSnapshot
    snapshot = ProjectSnapshot(
        project_manifest_hash=req.project_manifest_hash,
        user_manifest_hash=user_manifest_hash,
        parent_hashes=[current_head] if current_head else [],
        source="push",
        timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
    )
    snapshot_hash = cas.store_object(snapshot.to_dict(), root)

    # Advance HEAD (compare-and-swap on snapshot hash)
    if not advance_project_ref(
        settings.cas_base_path, principal.fingerprint,
        req.project_path, snapshot_hash, current_head,
    ):
        raise HTTPException(
            status.HTTP_409_CONFLICT,
            "HEAD moved during push. Pull and retry.",
        )

    return {
        "status": "ok",
        "remote_name": settings.rye_remote_name,
        "project_path": req.project_path,
        "project_manifest_hash": req.project_manifest_hash,
        "snapshot_hash": snapshot_hash,
    }


@app.post("/push/user-space")
async def push_user_space(
    req: PushUserSpaceRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Push user space independently from projects."""
    require_capability(principal, "rye.push.*")
    root = _principal_cas_root(principal, settings)

    # Deep validation: verify manifest schema + full transitive object graph
    _validate_manifest_graph(
        req.user_manifest_hash,
        root,
        expected_space="user",
        label="user_manifest",
    )

    advance_user_space_ref(
        settings.cas_base_path, principal.fingerprint,
        req.user_manifest_hash, req.expected_revision,
    )

    return {
        "status": "ok",
        "user_manifest_hash": req.user_manifest_hash,
        "remote_name": settings.rye_remote_name,
    }


@app.get("/user-space")
async def get_user_space(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Get current user space ref."""
    require_capability(principal, "rye.push.*")
    ref = resolve_user_space_ref(settings.cas_base_path, principal.fingerprint)
    if not ref:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            "No user space pushed yet.",
        )
    return {
        "user_manifest_hash": ref["user_manifest_hash"],
        "revision": ref["revision"],
        "pushed_at": ref["pushed_at"],
        "remote_name": settings.rye_remote_name,
    }


# --- Webhook binding lookup ---


def _lookup_binding(hook_id: str, settings: Settings) -> dict:
    """Look up an active webhook binding. Returns generic 401 on not found/revoked."""
    binding = resolve_binding(settings.cas_base_path, hook_id, settings.rye_remote_name)
    if not binding:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    return binding


def _resolve_principal_from_binding(binding: dict) -> Principal:
    """Resolve the Principal who owns a webhook binding.

    The binding's user_id is the principal fingerprint.
    Webhook bindings carry their own capabilities (inherited from when created).
    """
    return Principal(
        fingerprint=binding["user_id"],
        capabilities=binding.get("capabilities", ["rye.execute.*"]),
        owner=binding.get("owner", ""),
    )


# --- Dual-auth resolve_execution ---


async def resolve_execution(
    request: Request,
    settings: Settings = Depends(get_settings),
) -> ResolvedExecution:
    """Determine auth mode from headers and return normalized ResolvedExecution.

    - X-Rye-Signature header → signed-request auth path (caller controls everything)
    - X-Webhook-Timestamp header → webhook HMAC path (binding controls what executes)
    - Both or neither → 401
    """
    raw_body = await request.body()
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid JSON body")
    if not isinstance(body, dict):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "JSON body must be an object")

    has_signed = bool(request.headers.get("x-rye-signature"))
    has_webhook = bool(request.headers.get("x-webhook-timestamp"))

    if has_signed == has_webhook:
        raise HTTPException(
            status.HTTP_401_UNAUTHORIZED,
            "Provide exactly one auth mode: signed request headers OR webhook headers",
        )

    if has_webhook:
        # Webhook path — HMAC auth, binding controls what executes
        from ryeos_node.replay import get_replay_guard

        timestamp = request.headers.get("x-webhook-timestamp", "")
        signature = request.headers.get("x-webhook-signature", "")
        delivery_id = request.headers.get("x-webhook-delivery-id", "")
        hook_id = body.get("hook_id")
        if not hook_id:
            raise HTTPException(status.HTTP_400_BAD_REQUEST, "Webhook request requires hook_id")

        binding = _lookup_binding(hook_id, settings)
        verify_timestamp(timestamp)
        verify_hmac(timestamp, raw_body, binding["hmac_secret"], signature)

        # Replay check for webhook deliveries
        if delivery_id:
            guard = get_replay_guard(settings.cas_base_path)
            if not guard.check_and_record(hook_id, delivery_id):
                raise HTTPException(status.HTTP_200_OK, "Already processed")
        else:
            raise HTTPException(status.HTTP_400_BAD_REQUEST, "X-Webhook-Delivery-Id header required")

        principal = _resolve_principal_from_binding(binding)
        thread = "fork" if binding["item_type"] == "directive" else "inline"

        parameters = body.get("parameters", {})
        if not isinstance(parameters, dict):
            raise HTTPException(status.HTTP_400_BAD_REQUEST, "parameters must be an object")

        return ResolvedExecution(
            principal=principal,
            item_type=binding["item_type"],
            item_id=binding["item_id"],
            project_path=binding["project_path"],
            parameters=parameters,
            thread=thread,
        )

    # Signed-request path — caller controls everything
    principal = _verify_signed_request(request, raw_body, settings)
    require_capability(principal, "rye.execute.*")

    item_type = body.get("item_type")
    item_id = body.get("item_id")
    project_path = body.get("project_path")
    parameters = body.get("parameters", {})
    thread = body.get("thread")

    if not item_type or not item_id:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "item_type and item_id are required",
        )
    if item_type not in ("tool", "directive"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"item_type must be 'tool' or 'directive', got {item_type!r}",
        )
    if not project_path:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "project_path is required",
        )
    if not isinstance(parameters, dict):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "parameters must be an object",
        )

    if not thread:
        thread = "fork" if item_type == "directive" else "inline"

    if item_type == "directive" and thread != "fork":
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Directives must use thread=fork on remote, got thread={thread!r}",
        )
    if item_type == "tool" and thread != "inline":
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Tools must use thread=inline on remote, got thread={thread!r}",
        )

    return ResolvedExecution(
        principal=principal,
        item_type=item_type,
        item_id=item_id,
        project_path=project_path,
        parameters=parameters,
        thread=thread,
        secret_envelope=body.get("secret_envelope"),
    )


@app.post("/execute")
async def execute(
    resolved: ResolvedExecution = Depends(resolve_execution),
    settings: Settings = Depends(get_settings),
):
    return await _execute_from_head(
        principal=resolved.principal,
        settings=settings,
        project_path=resolved.project_path,
        item_type=resolved.item_type,
        item_id=resolved.item_id,
        parameters=resolved.parameters,
        thread=resolved.thread,
        secret_envelope=resolved.secret_envelope,
    )


# --- Execution from HEAD ---

MAX_FOLD_BACK_RETRIES = 5
FOLD_BACK_BASE_JITTER_MS = 50


async def _execute_from_head(
    principal: Principal,
    settings: Settings,
    project_path: str,
    item_type: str,
    item_id: str,
    parameters: Dict[str, Any],
    thread: str,
    secret_envelope: dict | None = None,
):
    """Execute from project HEAD — isolated mutable checkout with fold-back."""
    ref = _resolve_project_ref_or_404(settings, principal, project_path)
    user_ref = resolve_user_space_ref(settings.cas_base_path, principal.fingerprint)
    user_manifest_hash = user_ref["user_manifest_hash"] if user_ref else None

    base_snapshot_hash = ref["snapshot_hash"]
    if not base_snapshot_hash:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Project '{project_path}' has no snapshot. Re-push to create one.",
        )

    # Capacity check
    if _exec_counter.active >= settings.max_concurrent:
        raise HTTPException(
            status.HTTP_503_SERVICE_UNAVAILABLE,
            f"At capacity ({settings.max_concurrent} concurrent executions)",
        )
    _exec_counter.increment()

    root = _principal_cas_root(principal, settings)
    cache = settings.cache_root(principal.fingerprint)
    exec_root = settings.exec_root(principal.fingerprint)
    thread_id = f"rye-remote-{uuid.uuid4().hex[:12]}"
    exec_space: Path | None = None

    # Derive project_manifest_hash from snapshot for registration
    base_snap_obj = cas.get_object(base_snapshot_hash, _principal_cas_root(principal, settings))
    base_manifest_hash = base_snap_obj["project_manifest_hash"] if base_snap_obj else None

    register_execution(
        settings.cas_base_path, principal.fingerprint, thread_id,
        item_type=item_type,
        item_id=item_id,
        project_manifest_hash=base_manifest_hash or "",
        user_manifest_hash=user_manifest_hash,
        project_path=project_path,
        remote_name=settings.rye_remote_name,
        system_version=get_system_version(),
    )

    try:
        try:
            # Checkout mutable copy from snapshot cache
            exec_space = create_execution_space(
                base_snapshot_hash, thread_id, root, cache, exec_root,
            )
            user_space = ensure_user_space_cached(
                user_manifest_hash, root, cache,
            ) if user_manifest_hash else None

            os.environ["RYE_SIGNING_KEY_DIR"] = settings.signing_key_dir

            # Decrypt sealed envelope and inject secrets into env for subprocess
            injected_secrets: dict = {}
            prior_env: dict = {}
            if secret_envelope:
                from rye.primitives.sealed_envelope import decrypt_and_inject
                injected_secrets = decrypt_and_inject(secret_envelope, settings.signing_key_dir)
                for key in injected_secrets:
                    if key in os.environ:
                        prior_env[key] = os.environ[key]
                os.environ.update(injected_secrets)

            try:
                # Set USER_SPACE so resolvers/walkers find pushed user-space items
                # (safe under max_inputs=1 — one request per container process)
                if user_space:
                    os.environ["USER_SPACE"] = str(user_space)
                else:
                    empty_user = exec_space / ".empty_user_space"
                    empty_user.mkdir(exist_ok=True)
                    os.environ["USER_SPACE"] = str(empty_user)

                tool = ExecuteTool(
                    user_space=str(user_space) if user_space else None,
                    project_path=str(exec_space),
                )

                result = await tool.handle(
                    item_type=item_type,
                    item_id=item_id,
                    project_path=str(exec_space),
                    parameters=parameters,
                    thread=thread,
                )
            finally:
                for key in injected_secrets:
                    if key in prior_env:
                        os.environ[key] = prior_env[key]
                    else:
                        os.environ.pop(key, None)

            # Promote execution-local CAS into user CAS
            exec_cas = exec_space / AI_DIR / "objects"
            new_hashes = _copy_cas_objects(exec_cas, root)

            # Ingest runtime outputs (transcripts, knowledge, refs) into CAS
            exec_snapshot_hash = _find_execution_snapshot_hash(exec_space)
            if not exec_snapshot_hash:
                error_msg = result.get("error")
                fallback_status = result.get("status") or (
                    "completed" if result.get("success") else "error"
                )
                es = ExecutionSnapshot(
                    graph_run_id=thread_id,
                    graph_id=f"{item_type}/{item_id}",
                    project_manifest_hash=base_manifest_hash or "",
                    user_manifest_hash=user_manifest_hash,
                    system_version=get_system_version(),
                    step=0,
                    status=fallback_status,
                    errors=[{"message": error_msg, "phase": "execution"}] if error_msg else [],
                )
                exec_snapshot_hash = cas.store_object(es.to_dict(), root)
                new_hashes.append(exec_snapshot_hash)

            bundle_hash, output_hashes = _ingest_runtime_outputs(
                exec_space, root, thread_id, exec_snapshot_hash,
            )
            new_hashes.extend(output_hashes)

            # Build new manifest from execution space, compare to base
            exec_manifest_hash, _ = build_manifest(
                exec_space, "project", project_path=exec_space,
            )
            # Promote manifest objects into user CAS
            exec_manifest_cas = exec_space / AI_DIR / "objects"
            new_hashes.extend(_copy_cas_objects(exec_manifest_cas, root))

            base_snapshot_obj = cas.get_object(base_snapshot_hash, root)
            base_manifest_hash = base_snapshot_obj["project_manifest_hash"]

            if exec_manifest_hash == base_manifest_hash:
                # No-op — manifest unchanged, skip fold-back
                exec_status = result.get("status", "unknown")
                complete_execution(
                    settings.cas_base_path, principal.fingerprint, thread_id,
                    state="completed" if exec_status == "success" else "error",
                    snapshot_hash=exec_snapshot_hash,
                    runtime_outputs_bundle_hash=bundle_hash or None,
                )
                return {
                    "status": exec_status,
                    "thread_id": thread_id,
                    "snapshot_hash": base_snapshot_hash,
                    "merge_type": "no-op",
                    "execution_snapshot_hash": exec_snapshot_hash,
                    "runtime_outputs_bundle_hash": bundle_hash or None,
                    "new_object_hashes": new_hashes,
                    "result": result,
                    "system_version": get_system_version(),
                }

            # Create execution ProjectSnapshot
            proj_snapshot = ProjectSnapshot(
                project_manifest_hash=exec_manifest_hash,
                user_manifest_hash=user_manifest_hash,
                parent_hashes=[base_snapshot_hash],
                source="execution",
                source_detail=f"{item_type}/{item_id}",
                timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                metadata={"thread_id": thread_id},
            )
            proj_snapshot_hash = cas.store_object(proj_snapshot.to_dict(), root)
            new_hashes.append(proj_snapshot_hash)

            # Fold back into HEAD
            fold_result = await _fold_back(
                principal, settings, project_path,
                base_snapshot_hash, proj_snapshot_hash,
                root, cache, thread_id,
                user_manifest_hash=user_manifest_hash,
            )

            _check_user_quota(principal, settings)

            exec_status = result.get("status", "unknown")
            complete_execution(
                settings.cas_base_path, principal.fingerprint, thread_id,
                state="completed" if exec_status == "success" else "error",
                snapshot_hash=fold_result["snapshot_hash"],
                runtime_outputs_bundle_hash=bundle_hash or None,
            )

            return {
                "status": exec_status,
                "thread_id": thread_id,
                "snapshot_hash": fold_result["snapshot_hash"],
                "merge_type": fold_result["merge_type"],
                "execution_snapshot_hash": exec_snapshot_hash,
                "runtime_outputs_bundle_hash": bundle_hash or None,
                "new_object_hashes": new_hashes,
                "result": result,
                "system_version": get_system_version(),
                **({k: fold_result[k] for k in ("conflicts", "unmerged_snapshot") if k in fold_result}),
            }
        except FileNotFoundError as e:
            complete_execution(settings.cas_base_path, principal.fingerprint, thread_id, state="error")
            raise HTTPException(status.HTTP_404_NOT_FOUND, str(e))
        except Exception:
            complete_execution(settings.cas_base_path, principal.fingerprint, thread_id, state="error")
            raise
        finally:
            os.environ.pop("USER_SPACE", None)
            if exec_space:
                cleanup_execution_space(exec_space)
    finally:
        _exec_counter.decrement()


def _load_manifest_from_snapshot(
    snapshot_hash: str, cas_root: Path,
) -> dict:
    """Load the SourceManifest dict referenced by a ProjectSnapshot."""
    snapshot = cas.get_object(snapshot_hash, cas_root)
    if snapshot is None:
        raise FileNotFoundError(f"Snapshot {snapshot_hash} not found in CAS")
    manifest = cas.get_object(snapshot["project_manifest_hash"], cas_root)
    if manifest is None:
        raise FileNotFoundError(
            f"Manifest {snapshot['project_manifest_hash']} not found in CAS"
        )
    return manifest


async def _fold_back(
    principal: Principal,
    settings: Settings,
    project_path: str,
    base_snapshot_hash: str,
    exec_snapshot_hash: str,
    cas_root: Path,
    cache_root: Path,
    thread_id: str,
    user_manifest_hash: Optional[str] = None,
) -> dict:
    """Merge execution snapshot into HEAD.

    Fast-forward if HEAD hasn't moved, otherwise three-way merge.
    Bounded retry loop with jitter for contention.
    """
    current_head = base_snapshot_hash  # fallback for retry_exhausted

    for attempt in range(MAX_FOLD_BACK_RETRIES):
        ref = resolve_project_ref(settings.cas_base_path, principal.fingerprint, project_path)
        current_head = ref["snapshot_hash"] if ref else base_snapshot_hash

        if current_head == base_snapshot_hash:
            # Fast-forward — HEAD hasn't moved
            if _try_advance_head(
                settings, principal, project_path,
                exec_snapshot_hash, current_head,
            ):
                _update_snapshot_cache(
                    settings, principal, project_path,
                    exec_snapshot_hash, cas_root, cache_root,
                )
                return {"snapshot_hash": exec_snapshot_hash, "merge_type": "fast-forward"}
        else:
            # HEAD moved — three-way merge
            base_manifest = _load_manifest_from_snapshot(base_snapshot_hash, cas_root)
            head_manifest = _load_manifest_from_snapshot(current_head, cas_root)
            exec_manifest = _load_manifest_from_snapshot(exec_snapshot_hash, cas_root)

            merge_result = three_way_merge(
                base_manifest, head_manifest, exec_manifest, cas_root,
            )

            if merge_result.has_conflicts:
                store_conflict_record(
                    settings.cas_base_path, principal.fingerprint,
                    thread_id=thread_id,
                    conflicts=merge_result.conflicts,
                    unmerged_snapshot=exec_snapshot_hash,
                )
                return {
                    "snapshot_hash": current_head,
                    "merge_type": "conflict",
                    "conflicts": merge_result.conflicts,
                    "unmerged_snapshot": exec_snapshot_hash,
                }

            # Build merged manifest
            merged_manifest = SourceManifest(
                space="project",
                items=merge_result.merged_items,
                files=merge_result.merged_files,
            )
            merged_manifest_hash = cas.store_object(
                merged_manifest.to_dict(), cas_root,
            )

            merge_snapshot = ProjectSnapshot(
                project_manifest_hash=merged_manifest_hash,
                user_manifest_hash=user_manifest_hash,
                parent_hashes=[current_head, exec_snapshot_hash],
                source="merge",
                timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                metadata={"base": base_snapshot_hash, "thread_id": thread_id},
            )
            merge_snapshot_hash = cas.store_object(
                merge_snapshot.to_dict(), cas_root,
            )

            if _try_advance_head(
                settings, principal, project_path,
                merge_snapshot_hash, current_head,
            ):
                _update_snapshot_cache(
                    settings, principal, project_path,
                    merge_snapshot_hash, cas_root, cache_root,
                )
                return {"snapshot_hash": merge_snapshot_hash, "merge_type": "merge"}

        # CAS update raced — back off with jitter and retry
        jitter = FOLD_BACK_BASE_JITTER_MS * (2 ** attempt) + random.randint(0, 50)
        await asyncio.sleep(jitter / 1000)

    # Exhausted retries — persist unmerged snapshot for later inspection
    store_conflict_record(
        settings.cas_base_path, principal.fingerprint,
        thread_id=thread_id,
        conflicts={"retry_exhausted": True, "attempts": MAX_FOLD_BACK_RETRIES},
        unmerged_snapshot=exec_snapshot_hash,
    )
    return {
        "snapshot_hash": current_head,
        "merge_type": "retry_exhausted",
        "unmerged_snapshot": exec_snapshot_hash,
    }


def _try_advance_head(
    settings: Settings,
    principal: Principal,
    project_path: str,
    new_snapshot_hash: str,
    expected_snapshot_hash: str,
) -> bool:
    """Compare-and-swap on snapshot hash. Returns True if update succeeded."""
    return advance_project_ref(
        settings.cas_base_path, principal.fingerprint,
        project_path, new_snapshot_hash, expected_snapshot_hash,
    )


def _update_snapshot_cache(
    settings: Settings,
    principal: Principal,
    project_path: str,
    new_snapshot_hash: str,
    cas_root: Path,
    cache_root: Path,
) -> None:
    """Update snapshot cache to reflect new HEAD."""
    try:
        ensure_snapshot_cached(new_snapshot_hash, cas_root, cache_root)
    except Exception:
        logger.warning(
            "Failed to cache snapshot %s for %s",
            new_snapshot_hash[:16], project_path, exc_info=True,
        )


@app.get("/threads")
async def list_threads(
    limit: int = 20,
    project_path: Optional[str] = None,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List principal's remote executions on this remote."""
    require_capability(principal, "rye.threads.*")
    threads = list_executions(
        settings.cas_base_path, principal.fingerprint,
        project_path=project_path, limit=limit,
    )
    return {"threads": threads, "remote_name": settings.rye_remote_name}


@app.get("/threads/{thread_id}")
async def get_thread(
    thread_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Get status of a specific thread."""
    require_capability(principal, "rye.threads.*")
    record = get_execution(settings.cas_base_path, principal.fingerprint, thread_id)
    if not record:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Thread {thread_id} not found")
    return record


# --- History ---


@app.get("/history")
async def history(
    project_path: str,
    limit: int = 50,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Walk first-parent snapshot chain from project HEAD."""
    require_capability(principal, "rye.threads.*")
    ref = _resolve_project_ref_or_404(settings, principal, project_path)
    root = _principal_cas_root(principal, settings)
    snapshots = get_history(ref["snapshot_hash"], root, limit=min(limit, 200))
    return {
        "project_path": project_path,
        "head": ref["snapshot_hash"],
        "snapshots": snapshots,
        "remote_name": settings.rye_remote_name,
    }


# --- Webhook binding management ---


@app.post("/webhook-bindings")
async def create_webhook_binding(
    req: CreateWebhookBindingRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Create a webhook binding. Returns hook_id and hmac_secret (shown once)."""
    require_capability(principal, "rye.webhook-bindings.*")

    if req.item_type not in ("tool", "directive"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"item_type must be 'tool' or 'directive', got {req.item_type!r}",
        )

    return create_binding(
        settings.cas_base_path, principal.fingerprint, settings.rye_remote_name,
        req.item_type, req.item_id, req.project_path, req.description,
    )


@app.get("/webhook-bindings")
async def list_webhook_bindings(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List principal's webhook bindings (hmac_secret excluded)."""
    require_capability(principal, "rye.webhook-bindings.*")
    bindings_list = list_bindings(
        settings.cas_base_path, principal.fingerprint, settings.rye_remote_name,
    )
    return {"bindings": bindings_list}


@app.delete("/webhook-bindings/{hook_id}")
async def revoke_webhook_binding(
    hook_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Revoke a webhook binding (soft delete via revoked_at)."""
    require_capability(principal, "rye.webhook-bindings.*")
    if not revoke_binding(
        settings.cas_base_path, hook_id, principal.fingerprint, settings.rye_remote_name,
    ):
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"Webhook binding '{hook_id}' not found or already revoked",
        )
    return {"revoked": hook_id}


# --- Registry endpoints ---


@app.post("/registry/publish")
async def registry_publish(
    body: PublishRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    require_capability(principal, "rye.registry.publish")

    result = publish_item(
        settings.cas_base_path,
        body.item_type,
        body.item_id,
        body.version,
        body.manifest_hash,
        f"fp:{principal.fingerprint}",
    )
    if not result.get("ok"):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, result.get("error", "Publish failed"))
    return result


@app.get("/registry/search")
async def registry_search(
    query: str | None = None,
    item_type: str | None = None,
    namespace: str | None = None,
    limit: int = 20,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    require_capability(principal, "rye.registry.search")

    results = search_items(settings.cas_base_path, query, item_type, namespace, limit)
    return {"results": results, "total": len(results)}


@app.get("/registry/items/{item_type}/{item_id:path}/versions/{version}")
async def registry_get_version(
    item_type: str,
    item_id: str,
    version: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    require_capability(principal, "rye.registry.search")

    ver = get_version(settings.cas_base_path, item_type, item_id, version)
    if ver is None:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Version not found: {item_type}/{item_id}@{version}")
    return ver


@app.get("/registry/items/{item_type}/{item_id:path}")
async def registry_get_item(
    item_type: str,
    item_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    require_capability(principal, "rye.registry.search")

    item = get_item(settings.cas_base_path, item_type, item_id)
    if item is None:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Item not found: {item_type}/{item_id}")
    return item


@app.post("/registry/namespaces/claim")
async def registry_claim_namespace(
    body: ClaimNamespaceRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    require_capability(principal, "rye.registry.publish")

    result = claim_namespace(settings.cas_base_path, body.claim)
    if not result.get("ok"):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, result.get("error", "Claim failed"))
    return result


@app.post("/registry/identity")
async def registry_register_identity(
    body: RegisterIdentityRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")

    result = register_identity(settings.cas_base_path, body.identity)
    if not result.get("ok"):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, result.get("error", "Registration failed"))
    return result


@app.get("/registry/identity/{fingerprint}")
async def registry_lookup_identity(
    fingerprint: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")

    doc = lookup_identity(settings.cas_base_path, fingerprint)
    if doc is None:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Identity not found: {fingerprint}")
    return doc
