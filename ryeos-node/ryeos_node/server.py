"""CAS-native remote execution server.

Endpoints: /health, /public-key, /objects/has, /objects/put, /objects/get,
           /push, /push/user-space, /user-space,
           /execute,
           /threads, /threads/{thread_id},
           /gc (POST), /gc/stats (GET),
           /webhook-bindings (POST, GET), /webhook-bindings/{hook_id} (DELETE)
"""

from contextlib import asynccontextmanager

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
    require_publish_namespace,
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

from rye.cas.objects import (
    ExecutionSnapshot,
    ProjectSnapshot,
    RuntimeOutputsBundle,
    SourceManifest,
    SCHEMA_VERSION,
    get_history,
)
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
from rye.cas.gc import prune_cache, prune_executions, mark_reachable, sweep, run_gc
from rye.cas.gc import compact_project_history, load_retention_policy, emit_gc_event
from rye.cas.gc_types import RetentionPolicy, GCResult
from rye.cas.gc_epochs import register_epoch, complete_epoch
from rye.cas.gc_incremental import load_gc_state
from rye.cas.gc_lock import read_lock as read_gc_lock
from rye.constants import AI_DIR
from rye.actions.execute import ExecuteTool
from rye.utils.execution_context import ExecutionContext

# Configure ryeos_node package logging once, in the uvicorn process.
# init.py runs in a separate process so its basicConfig doesn't apply here;
# without this, all ryeos_node.* logs are silently dropped because uvicorn
# only configures handlers for the `uvicorn.*` loggers.
_pkg_logger = logging.getLogger("ryeos_node")
if not _pkg_logger.handlers:
    _handler = logging.StreamHandler()  # stderr by default
    _handler.setFormatter(
        logging.Formatter("%(asctime)s %(levelname)s %(name)s: %(message)s")
    )
    _pkg_logger.addHandler(_handler)
    _pkg_logger.setLevel(logging.INFO)
    _pkg_logger.propagate = False

logger = logging.getLogger(__name__)


# Strong references to fire-and-forget background tasks so the event loop's
# weak-ref tracking doesn't GC them mid-execution. See:
# https://docs.python.org/3/library/asyncio-task.html#asyncio.create_task
_background_tasks: set[asyncio.Task] = set()


def _spawn_background_task(task: asyncio.Task, label: str) -> None:
    """Track a fire-and-forget asyncio Task and log lifecycle anomalies."""
    _background_tasks.add(task)

    def _done(t: asyncio.Task) -> None:
        _background_tasks.discard(t)
        if t.cancelled():
            logger.warning("Background task cancelled: %s", label)

    task.add_done_callback(_done)


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

from ryeos_node import __version__ as _node_version

_gc_scheduler_task: Optional[asyncio.Task] = None


async def _periodic_gc_loop() -> None:
    """Run GC for all active users on a schedule."""
    settings = get_settings()
    interval = settings.gc_schedule_interval
    cas_base = Path(settings.cas_base_path)
    node_id = settings.rye_remote_name

    logger.info("Periodic GC scheduler started (interval=%ds)", interval)

    while True:
        await asyncio.sleep(interval)
        logger.debug("Periodic GC tick")
        try:
            if not cas_base.is_dir():
                continue
            for user_dir in cas_base.iterdir():
                if not user_dir.is_dir() or user_dir.name.startswith("."):
                    continue
                try:
                    policy = RetentionPolicy(
                        manual_pushes=settings.gc_max_manual_pushes,
                        daily_checkpoints=settings.gc_retention_days,
                        max_success_executions=settings.gc_max_executions,
                        max_failure_executions=settings.gc_max_executions,
                    )
                    ai_dir = os.environ.get("AI_DIR", ".ai")
                    user_root = user_dir
                    cas_root = user_dir / ai_dir / "state" / "objects"
                    if not cas_root.is_dir():
                        continue
                    result = await asyncio.to_thread(
                        run_gc,
                        user_root,
                        cas_root,
                        node_id=node_id,
                        policy=policy,
                    )
                    logger.info(
                        "Periodic GC for %s: freed %s in %dms",
                        user_dir.name,
                        _human_bytes(result.total_freed_bytes),
                        result.duration_ms,
                    )
                except Exception:
                    logger.warning(
                        "Periodic GC failed for %s", user_dir.name, exc_info=True
                    )
        except Exception:
            logger.warning("Periodic GC tick failed", exc_info=True)


@asynccontextmanager
async def _lifespan(app: FastAPI):
    global _gc_scheduler_task
    settings = get_settings()
    if settings.gc_auto_enabled and settings.gc_schedule_interval > 0:
        _gc_scheduler_task = asyncio.create_task(_periodic_gc_loop())
    yield
    if _gc_scheduler_task:
        _gc_scheduler_task.cancel()
        try:
            await _gc_scheduler_task
        except asyncio.CancelledError:
            pass


app = FastAPI(title="ryeos-node", version=_node_version, lifespan=_lifespan)

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
    item_id: str  # canonical ref, e.g. "directive:email/handle_inbound"
    project_path: str
    description: Optional[str] = None
    secret_envelope: Optional[dict] = None
    vault_keys: Optional[List[str]] = None


class AuthorizeKeyRequest(BaseModel):
    public_key: str  # ed25519:<base64>
    label: str = "operator"
    scopes: List[str] = ["*"]


class PublishRequest(BaseModel):
    kind: str
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


def _measure_usage(root: Path) -> int:
    """Sum file sizes under a directory tree."""
    total = 0
    try:
        for f in root.rglob("*"):
            if f.is_file():
                try:
                    total += f.stat().st_size
                except OSError:
                    pass
    except OSError:
        pass
    return total


def _get_last_gc_time(user_root: Path) -> float | None:
    """Return timestamp of last GC run, or None."""
    state = load_gc_state(user_root)
    if state and state.last_gc_at:
        try:
            return datetime.datetime.fromisoformat(state.last_gc_at).timestamp()
        except (ValueError, TypeError):
            pass
    return None


async def _check_user_quota(principal: Principal, settings: Settings) -> None:
    """Check quota and auto-GC if over limit. Raise 507 only if GC can't reclaim enough."""
    user_root = settings.user_root(principal.fingerprint)
    if not user_root.exists():
        return

    total = _measure_usage(user_root)
    quota = settings.max_user_storage_bytes
    if total <= quota:
        return

    if not settings.gc_auto_enabled:
        raise HTTPException(
            status.HTTP_507_INSUFFICIENT_STORAGE,
            f"User storage quota exceeded ({total} bytes > {quota})",
        )

    # Rate limit: don't auto-GC more than once per cooldown period
    import time as _time

    last_gc = _get_last_gc_time(user_root)
    if last_gc and (_time.time() - last_gc) < settings.gc_auto_cooldown:
        raise HTTPException(
            status.HTTP_507_INSUFFICIENT_STORAGE,
            f"User storage quota exceeded ({total} > {quota}), GC ran recently",
        )

    cas_root = _principal_cas_root(principal, settings)
    logger.info(
        "Auto-GC triggered for %s: %d bytes > %d quota",
        principal.fingerprint,
        total,
        quota,
    )

    # Phase 1: quick cache prune (always safe, fast)
    prune_cache(user_root, emergency=True)
    total = _measure_usage(user_root)
    if total <= quota:
        return

    # Full GC with aggressive retention
    try:
        await asyncio.to_thread(
            run_gc,
            user_root,
            cas_root,
            node_id=f"{settings.rye_remote_name}-auto-{os.getpid()}",
            aggressive=True,
            policy=RetentionPolicy(
                manual_pushes=1,
                daily_checkpoints=1,
                max_success_executions=5,
                max_failure_executions=5,
            ),
        )
    except Exception:
        logger.warning("Auto-GC failed for %s", principal.fingerprint, exc_info=True)

    total = _measure_usage(user_root)
    if total > quota:
        raise HTTPException(
            status.HTTP_507_INSUFFICIENT_STORAGE,
            f"User storage quota exceeded after GC ({total} > {quota})",
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


def _validation_error(detail: str, label: str) -> HTTPException:
    """Log and raise a manifest validation error."""
    logger.warning("Manifest validation failed (%s): %s", label, detail)
    return HTTPException(status.HTTP_400_BAD_REQUEST, detail)


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
        raise _validation_error(
            f"{label} object {manifest_hash} not found in CAS. Upload objects first.",
            label,
        )
    if manifest.get("kind") != "source_manifest":
        raise _validation_error(
            f"{label} {manifest_hash} has kind={manifest.get('kind')!r}, expected 'source_manifest'",
            label,
        )
    if manifest.get("schema") != SCHEMA_VERSION:
        raise _validation_error(
            f"{label} {manifest_hash} has unsupported schema version {manifest.get('schema')!r}",
            label,
        )
    if manifest.get("space") != expected_space:
        raise _validation_error(
            f"{label} {manifest_hash} has space={manifest.get('space')!r}, expected {expected_space!r}",
            label,
        )

    items = manifest.get("items", {})
    files = manifest.get("files", {})
    if not isinstance(items, dict):
        raise _validation_error(
            f"{label} {manifest_hash} has invalid items (expected object)",
            label,
        )
    if not isinstance(files, dict):
        raise _validation_error(
            f"{label} {manifest_hash} has invalid files (expected object)",
            label,
        )

    # Dedupe: avoid re-checking the same hash multiple times
    validated_items: dict[str, dict] = {}
    validated_blobs: set[str] = set()

    for rel_path, item_hash in items.items():
        if item_hash not in validated_items:
            item_obj = cas.get_object(item_hash, root)
            if item_obj is None:
                raise _validation_error(
                    f"{label} item '{rel_path}' references missing object {item_hash}",
                    label,
                )
            if item_obj.get("kind") != "item_source":
                raise _validation_error(
                    f"{label} item '{rel_path}' references object {item_hash} "
                    f"with kind={item_obj.get('kind')!r}, expected 'item_source'",
                    label,
                )
            blob_hash = item_obj.get("content_blob_hash")
            if not isinstance(blob_hash, str) or not blob_hash:
                raise _validation_error(
                    f"item_source {item_hash} for '{rel_path}' is missing content_blob_hash",
                    label,
                )
            validated_items[item_hash] = item_obj

        blob_hash = validated_items[item_hash]["content_blob_hash"]
        if blob_hash not in validated_blobs:
            if not cas.has_blob(blob_hash, root):
                raise _validation_error(
                    f"item_source {item_hash} for '{rel_path}' references missing blob {blob_hash}",
                    label,
                )
            validated_blobs.add(blob_hash)

    for rel_path, blob_hash in files.items():
        if blob_hash not in validated_blobs:
            if not cas.has_blob(blob_hash, root):
                raise _validation_error(
                    f"{label} file '{rel_path}' references missing blob {blob_hash}",
                    label,
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
    f"{AI_DIR}/state/",
    f"{AI_DIR}/knowledge/state/",
    f"{AI_DIR}/state/objects/refs/",
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
                    logger.warning("Skipping symlink in runtime outputs: %s", file_path)
                    continue

                # Validate path stays under project root
                try:
                    resolved = file_path.resolve()
                    if not resolved.is_relative_to(resolved_root):
                        logger.warning("Path escapes project root: %s", file_path)
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
                        rel_path,
                        exc_info=True,
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
        len(files),
        bundle_hash[:16],
        thread_id,
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
    ref = resolve_project_ref(
        settings.cas_base_path, principal.fingerprint, project_path
    )
    if not ref:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"No project ref '{project_path}' found on remote '{settings.rye_remote_name}'. "
            f"Push first: rye execute tool rye/core/remote/remote action=push",
        )
    return ref


def _find_execution_snapshot_hash(project_path: Path) -> Optional[str]:
    """Find the walker's real execution_snapshot hash from graph refs."""
    refs_dir = project_path / AI_DIR / "state" / "objects" / "refs" / "graphs"
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
    from ryeos_node import __version__ as node_version

    ryeos_version = "unknown"
    try:
        from importlib.metadata import version

        ryeos_version = version("ryeos")
    except Exception:
        pass

    return {
        "status": "ok",
        "version": node_version,
        "engine_version": get_system_version(),
        "ryeos_version": ryeos_version,
    }


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
        "created_at": datetime.datetime.now(datetime.timezone.utc).strftime(
            "%Y-%m-%dT%H:%M:%SZ"
        ),
    }

    # Sign with the node's own key
    import hashlib as _hl
    from rye.primitives.signing import sign_hash

    payload = json.dumps(
        {k: v for k, v in identity_doc.items() if k != "_signature"},
        sort_keys=True,
        separators=(",", ":"),
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
    root = _principal_cas_root(principal, settings)
    return handle_has_objects(req.hashes, root)


@app.post("/objects/put")
async def objects_put(
    req: PutObjectsRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    await _check_user_quota(principal, settings)
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
    root = _principal_cas_root(principal, settings)
    return handle_get_objects(req.hashes, root)


class DebugCasRequest(BaseModel):
    hash: str
    kind: str = "blob"  # "blob" or "object"


@app.post("/debug/cas")
async def debug_cas(
    req: DebugCasRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Diagnostic endpoint: check CAS blob/object existence and retrieval."""
    root = _principal_cas_root(principal, settings)

    result: Dict[str, Any] = {
        "hash": req.hash,
        "kind": req.kind,
        "cas_root": str(root),
    }

    if req.kind == "blob":
        # Check filesystem
        blob_path = root / "blobs" / req.hash[:2] / req.hash[2:4] / req.hash
        result["shard_path"] = str(blob_path)
        result["exists_on_disk"] = blob_path.exists()
        if blob_path.exists():
            try:
                stat = blob_path.stat()
                result["file_size"] = stat.st_size
                result["file_mode"] = oct(stat.st_mode)
            except Exception as e:
                result["stat_error"] = str(e)

        # Check via has_blob (Python path check)
        result["has_blob"] = cas.has_blob(req.hash, root)

        # Check via lillux binary
        import subprocess

        try:
            from rye.primitives.cas import _lillux

            lillux_bin = _lillux()
            result["lillux_binary"] = lillux_bin
            proc = subprocess.run(
                [
                    lillux_bin,
                    "cas",
                    "fetch",
                    "--root",
                    str(root),
                    "--hash",
                    req.hash,
                    "--blob",
                ],
                capture_output=True,
            )
            result["lillux_returncode"] = proc.returncode
            result["lillux_stdout_len"] = len(proc.stdout)
            result["lillux_stderr"] = proc.stderr.decode("utf-8", errors="replace")[
                :500
            ]
        except Exception as e:
            result["lillux_error"] = str(e)

        # Check via get_blob
        blob_data = cas.get_blob(req.hash, root)
        result["get_blob_result"] = (
            f"bytes(len={len(blob_data)})" if blob_data is not None else "None"
        )
    else:
        result["has_object"] = cas.has_object(req.hash, root)
        obj = cas.get_object(req.hash, root)
        result["get_object_result"] = (
            f"dict(keys={list(obj.keys())})" if obj is not None else "None"
        )

    return result


@app.post("/push")
async def push(
    req: PushRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Finalize a push — validate manifest graph, create snapshot, advance HEAD."""
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
    ref = resolve_project_ref(
        settings.cas_base_path, principal.fingerprint, req.project_path
    )
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

    # Writer epoch: register BEFORE creating CAS objects
    user_root = settings.user_root(principal.fingerprint)
    epoch_id = register_epoch(
        user_root,
        settings.rye_remote_name,
        principal.fingerprint,
        [current_head] if current_head else [],
    )

    try:
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
            settings.cas_base_path,
            principal.fingerprint,
            req.project_path,
            snapshot_hash,
            current_head,
        ):
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                "HEAD moved during push. Pull and retry.",
            )
    finally:
        complete_epoch(user_root, epoch_id)

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
    root = _principal_cas_root(principal, settings)

    # Deep validation: verify manifest schema + full transitive object graph
    _validate_manifest_graph(
        req.user_manifest_hash,
        root,
        expected_space="user",
        label="user_manifest",
    )

    advance_user_space_ref(
        settings.cas_base_path,
        principal.fingerprint,
        req.user_manifest_hash,
        req.expected_revision,
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
    """
    return Principal(
        fingerprint=binding["user_id"],
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
            raise HTTPException(
                status.HTTP_400_BAD_REQUEST, "Webhook request requires hook_id"
            )

        binding = _lookup_binding(hook_id, settings)
        verify_timestamp(timestamp)
        verify_hmac(timestamp, raw_body, binding["hmac_secret"], signature)

        # Replay check for webhook deliveries
        if delivery_id:
            guard = get_replay_guard(settings.cas_base_path)
            if not guard.check_and_record(hook_id, delivery_id):
                raise HTTPException(status.HTTP_200_OK, "Already processed")
        else:
            raise HTTPException(
                status.HTTP_400_BAD_REQUEST, "X-Webhook-Delivery-Id header required"
            )

        principal = _resolve_principal_from_binding(binding)

        parameters = body.get("parameters", {})
        if not isinstance(parameters, dict):
            raise HTTPException(
                status.HTTP_400_BAD_REQUEST, "parameters must be an object"
            )

        return ResolvedExecution(
            principal=principal,
            item_id=binding["item_id"],
            project_path=binding["project_path"],
            parameters=parameters,
            thread=body.get("thread") or "inline",
            secret_envelope=binding.get("secret_envelope"),
            vault_keys=binding.get("vault_keys") or None,
            fire_and_forget=bool(body.get("async", False)),
        )

    # Signed-request path — caller controls everything
    principal = _verify_signed_request(request, raw_body, settings)

    from rye.constants import ItemType

    raw_item_id = body.get("item_id", "")
    kind, bare_id = ItemType.parse_canonical_ref(raw_item_id)

    if not kind or not bare_id:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "item_id with canonical ref (e.g. tool:my/tool) is required",
        )

    project_path = body.get("project_path")
    parameters = body.get("parameters", {})
    thread = body.get("thread")

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

    vault_keys = body.get("vault_keys")
    if vault_keys is not None:
        if not isinstance(vault_keys, list) or not all(
            isinstance(k, str) for k in vault_keys
        ):
            raise HTTPException(
                status.HTTP_400_BAD_REQUEST, "vault_keys must be a list of strings"
            )
    validated_vault_keys = vault_keys or None

    return ResolvedExecution(
        principal=principal,
        item_id=raw_item_id,
        project_path=project_path,
        parameters=parameters,
        thread=thread,
        secret_envelope=body.get("secret_envelope"),
        vault_keys=validated_vault_keys,
        fire_and_forget=bool(body.get("async", False)),
    )


@app.post("/execute")
async def execute(
    resolved: ResolvedExecution = Depends(resolve_execution),
    settings: Settings = Depends(get_settings),
):
    if resolved.fire_and_forget:
        # Generate thread_id early so we can return it immediately
        item_slug = resolved.item_id.replace("/", "-")
        thread_id = f"rye-remote-{item_slug}-{int(__import__('time').time() * 1000)}"
        logger.info(
            "Accepted async execution %s (%s)", thread_id, resolved.item_id
        )

        async def _bg_execute():
            logger.info(
                "Starting async execution %s (%s)", thread_id, resolved.item_id
            )
            try:
                result = await _execute_from_head(
                    principal=resolved.principal,
                    settings=settings,
                    project_path=resolved.project_path,
                    item_id=resolved.item_id,
                    parameters=resolved.parameters,
                    thread=resolved.thread,
                    secret_envelope=resolved.secret_envelope,
                    vault_keys=resolved.vault_keys,
                    thread_id_override=thread_id,
                )
                status_val = (
                    result.get("status") if isinstance(result, dict) else "ok"
                )
                logger.info(
                    "Async execution finished for %s (%s): %s",
                    thread_id, resolved.item_id, status_val,
                )
            except Exception:
                logger.error(
                    "Async execution failed for %s (%s)",
                    thread_id, resolved.item_id, exc_info=True,
                )

        # Track the task so the event loop's weak-ref tracking doesn't GC it
        # before/during execution. Without this the task can vanish silently.
        task = asyncio.create_task(_bg_execute())
        _spawn_background_task(task, f"execute {thread_id}")
        return {
            "status": "accepted",
            "thread_id": thread_id,
            "async": True,
            "message": f"Execution started in background. Poll /threads/{thread_id} for status.",
        }

    return await _execute_from_head(
        principal=resolved.principal,
        settings=settings,
        project_path=resolved.project_path,
        item_id=resolved.item_id,
        parameters=resolved.parameters,
        thread=resolved.thread,
        secret_envelope=resolved.secret_envelope,
        vault_keys=resolved.vault_keys,
    )


# --- Execution from HEAD ---

MAX_FOLD_BACK_RETRIES = 5
FOLD_BACK_BASE_JITTER_MS = 50


async def _execute_from_head(
    principal: Principal,
    settings: Settings,
    project_path: str,
    item_id: str,
    parameters: Dict[str, Any],
    thread: str | None,
    secret_envelope: dict | None = None,
    vault_keys: list[str] | None = None,
    thread_id_override: str | None = None,
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
    thread_id = thread_id_override or f"rye-remote-{item_id.replace('/', '-')}-{int(__import__('time').time() * 1000)}"
    exec_space: Path | None = None

    # Derive project_manifest_hash from snapshot for registration
    base_snap_obj = cas.get_object(
        base_snapshot_hash, _principal_cas_root(principal, settings)
    )
    base_manifest_hash = (
        base_snap_obj["project_manifest_hash"] if base_snap_obj else None
    )

    register_execution(
        settings.cas_base_path,
        principal.fingerprint,
        thread_id,
        item_id=item_id,
        project_manifest_hash=base_manifest_hash or "",
        user_manifest_hash=user_manifest_hash,
        project_path=project_path,
        remote_name=settings.rye_remote_name,
        system_version=get_system_version(),
    )

    # Writer epoch: protect in-flight CAS objects from GC sweep
    user_root = settings.user_root(principal.fingerprint)
    epoch_id = register_epoch(
        user_root,
        settings.rye_remote_name,
        principal.fingerprint,
        [base_snapshot_hash],
    )

    try:
        try:
            # Checkout mutable copy from snapshot cache
            exec_space = create_execution_space(
                base_snapshot_hash,
                thread_id,
                root,
                cache,
                exec_root,
            )
            user_space = (
                ensure_user_space_cached(
                    user_manifest_hash,
                    root,
                    cache,
                )
                if user_manifest_hash
                else None
            )

            # Build per-execution env (never mutate process-global os.environ)
            # These are for subprocesses; in-process code uses ExecutionContext.
            resolved_user_space: Path = (
                user_space if user_space else exec_space / ".empty_user_space"
            )
            if not user_space:
                resolved_user_space.mkdir(exist_ok=True)

            exec_env: dict[str, str] = {
                "RYE_SIGNING_KEY_DIR": settings.signing_key_dir,
                "USER_SPACE": str(resolved_user_space),
            }

            # Resolve vault secrets into per-execution env
            if vault_keys:
                from ryeos_node.vault import resolve_vault_env

                vault_env = resolve_vault_env(
                    settings.cas_base_path,
                    principal.fingerprint,
                    vault_keys,
                    settings.signing_key_dir,
                )
                exec_env.update(vault_env)

            # Decrypt sealed envelope into per-execution env (overrides vault)
            if secret_envelope:
                from rye.primitives.sealed_envelope import decrypt_and_inject

                decrypted = decrypt_and_inject(
                    secret_envelope, settings.signing_key_dir
                )
                exec_env.update(decrypted)

            # Explicit execution context — no env-var fallbacks
            from rye.utils.path_utils import get_system_spaces

            exec_ctx = ExecutionContext(
                project_path=exec_space,
                user_space=resolved_user_space,
                signing_key_dir=Path(settings.signing_key_dir),
                system_spaces=tuple(get_system_spaces()),
            )

            tool = ExecuteTool(
                ctx=exec_ctx,
                extra_env=exec_env,
            )

            result = await tool.handle(
                item_id=item_id,
                project_path=str(exec_space),
                parameters=parameters,
                thread=thread,
            )

            # Callee-owned tools (e.g. thread_directive, walker) generate
            # their own thread ID.  Separate it from the node's execution
            # record ID so callers can query status on the right handle.
            callee_thread_id = result.get("thread_id")

            # Promote execution-local CAS into user CAS
            exec_cas = exec_space / AI_DIR / "state" / "objects"
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
                    graph_id=item_id,
                    project_manifest_hash=base_manifest_hash or "",
                    user_manifest_hash=user_manifest_hash,
                    system_version=get_system_version(),
                    step=0,
                    status=fallback_status,
                    errors=[{"message": error_msg, "phase": "execution"}]
                    if error_msg
                    else [],
                )
                exec_snapshot_hash = cas.store_object(es.to_dict(), root)
                new_hashes.append(exec_snapshot_hash)

            bundle_hash, output_hashes = _ingest_runtime_outputs(
                exec_space,
                root,
                thread_id,
                exec_snapshot_hash,
            )
            new_hashes.extend(output_hashes)

            # Build new manifest from execution space, compare to base
            exec_manifest_hash, _ = build_manifest(
                exec_space,
                "project",
                project_path=exec_space,
            )
            # Promote manifest objects into user CAS
            exec_manifest_cas = exec_space / AI_DIR / "state" / "objects"
            new_hashes.extend(_copy_cas_objects(exec_manifest_cas, root))

            base_snapshot_obj = cas.get_object(base_snapshot_hash, root)
            base_manifest_hash = base_snapshot_obj["project_manifest_hash"]

            if exec_manifest_hash == base_manifest_hash:
                # No-op — manifest unchanged, skip fold-back
                exec_status = result.get("status", "unknown")
                complete_execution(
                    settings.cas_base_path,
                    principal.fingerprint,
                    thread_id,
                    state="completed" if exec_status == "success" else "error",
                    snapshot_hash=exec_snapshot_hash,
                    execution_snapshot_hash=exec_snapshot_hash,
                    runtime_outputs_bundle_hash=bundle_hash or None,
                )
                return {
                    "status": exec_status,
                    "thread_id": thread_id,
                    "callee_thread_id": callee_thread_id,
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
                source_detail=item_id,
                timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                metadata={"thread_id": thread_id},
            )
            proj_snapshot_hash = cas.store_object(proj_snapshot.to_dict(), root)
            new_hashes.append(proj_snapshot_hash)

            # Fold back into HEAD
            fold_result = await _fold_back(
                principal,
                settings,
                project_path,
                base_snapshot_hash,
                proj_snapshot_hash,
                root,
                cache,
                thread_id,
                user_manifest_hash=user_manifest_hash,
            )

            await _check_user_quota(principal, settings)

            exec_status = result.get("status", "unknown")
            complete_execution(
                settings.cas_base_path,
                principal.fingerprint,
                thread_id,
                state="completed" if exec_status == "success" else "error",
                snapshot_hash=fold_result["snapshot_hash"],
                execution_snapshot_hash=exec_snapshot_hash,
                runtime_outputs_bundle_hash=bundle_hash or None,
            )

            return {
                "status": exec_status,
                "thread_id": thread_id,
                "callee_thread_id": callee_thread_id,
                "snapshot_hash": fold_result["snapshot_hash"],
                "merge_type": fold_result["merge_type"],
                "execution_snapshot_hash": exec_snapshot_hash,
                "runtime_outputs_bundle_hash": bundle_hash or None,
                "new_object_hashes": new_hashes,
                "result": result,
                "system_version": get_system_version(),
                **(
                    {
                        k: fold_result[k]
                        for k in ("conflicts", "unmerged_snapshot")
                        if k in fold_result
                    }
                ),
            }
        except FileNotFoundError as e:
            complete_execution(
                settings.cas_base_path,
                principal.fingerprint,
                thread_id,
                state="error",
                error_message=str(e),
                error_phase="materialization",
            )
            raise HTTPException(status.HTTP_404_NOT_FOUND, str(e))
        except Exception as e:
            logger.error(
                "Execution failed for thread %s (%s): %s",
                thread_id,
                item_id,
                e,
                exc_info=True,
            )
            complete_execution(
                settings.cas_base_path,
                principal.fingerprint,
                thread_id,
                state="error",
                error_message=str(e),
                error_phase="execution",
            )
            raise HTTPException(
                status.HTTP_500_INTERNAL_SERVER_ERROR,
                detail={
                    "error": str(e),
                    "thread_id": thread_id,
                    "item_id": item_id,
                    "phase": "execution",
                },
            )
        finally:
            if exec_space:
                cleanup_execution_space(exec_space)
    finally:
        complete_epoch(user_root, epoch_id)
        _exec_counter.decrement()


def _load_manifest_from_snapshot(
    snapshot_hash: str,
    cas_root: Path,
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
        ref = resolve_project_ref(
            settings.cas_base_path, principal.fingerprint, project_path
        )
        current_head = ref["snapshot_hash"] if ref else base_snapshot_hash

        if current_head == base_snapshot_hash:
            # Fast-forward — HEAD hasn't moved
            if _try_advance_head(
                settings,
                principal,
                project_path,
                exec_snapshot_hash,
                current_head,
            ):
                _update_snapshot_cache(
                    settings,
                    principal,
                    project_path,
                    exec_snapshot_hash,
                    cas_root,
                    cache_root,
                )
                return {
                    "snapshot_hash": exec_snapshot_hash,
                    "merge_type": "fast-forward",
                }
        else:
            # HEAD moved — three-way merge
            base_manifest = _load_manifest_from_snapshot(base_snapshot_hash, cas_root)
            head_manifest = _load_manifest_from_snapshot(current_head, cas_root)
            exec_manifest = _load_manifest_from_snapshot(exec_snapshot_hash, cas_root)

            merge_result = three_way_merge(
                base_manifest,
                head_manifest,
                exec_manifest,
                cas_root,
            )

            if merge_result.has_conflicts:
                store_conflict_record(
                    settings.cas_base_path,
                    principal.fingerprint,
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
                merged_manifest.to_dict(),
                cas_root,
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
                merge_snapshot.to_dict(),
                cas_root,
            )

            if _try_advance_head(
                settings,
                principal,
                project_path,
                merge_snapshot_hash,
                current_head,
            ):
                _update_snapshot_cache(
                    settings,
                    principal,
                    project_path,
                    merge_snapshot_hash,
                    cas_root,
                    cache_root,
                )
                return {"snapshot_hash": merge_snapshot_hash, "merge_type": "merge"}

        # CAS update raced — back off with jitter and retry
        jitter = FOLD_BACK_BASE_JITTER_MS * (2**attempt) + random.randint(0, 50)
        await asyncio.sleep(jitter / 1000)

    # Exhausted retries — persist unmerged snapshot for later inspection
    store_conflict_record(
        settings.cas_base_path,
        principal.fingerprint,
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
        settings.cas_base_path,
        principal.fingerprint,
        project_path,
        new_snapshot_hash,
        expected_snapshot_hash,
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
            new_snapshot_hash[:16],
            project_path,
            exc_info=True,
        )


@app.get("/threads")
async def list_threads(
    limit: int = 20,
    project_path: Optional[str] = None,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List principal's remote executions on this remote."""
    threads = list_executions(
        settings.cas_base_path,
        principal.fingerprint,
        project_path=project_path,
        limit=limit,
    )
    return {"threads": threads, "remote_name": settings.rye_remote_name}


@app.get("/threads/{thread_id}")
async def get_thread(
    thread_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Get status of a specific thread."""
    record = get_execution(settings.cas_base_path, principal.fingerprint, thread_id)
    if not record:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Thread {thread_id} not found")

    root = _principal_cas_root(principal, settings)

    # Prefer execution_snapshot_hash (always an ExecutionSnapshot with errors)
    # Fall back to snapshot_hash for older records
    exec_snap_hash = record.get("execution_snapshot_hash") or record.get(
        "snapshot_hash"
    )
    if exec_snap_hash:
        snapshot = cas.get_object(exec_snap_hash, root)
        if snapshot and snapshot.get("kind") == "execution_snapshot":
            errors = snapshot.get("errors")
            if errors:
                record["execution_errors"] = errors
            snap_status = snapshot.get("status")
            if snap_status:
                record["execution_status"] = snap_status

    bundle_hash = record.get("runtime_outputs_bundle_hash")
    if bundle_hash:
        bundle = cas.get_object(bundle_hash, root)
        if bundle:
            files = bundle.get("files")
            if files is not None:
                record["runtime_output_files"] = len(files)

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
    ref = _resolve_project_ref_or_404(settings, principal, project_path)
    root = _principal_cas_root(principal, settings)
    snapshots = get_history(ref["snapshot_hash"], root, limit=min(limit, 200))
    return {
        "project_path": project_path,
        "head": ref["snapshot_hash"],
        "snapshots": snapshots,
        "remote_name": settings.rye_remote_name,
    }


# --- Vault (per-principal secret store) ---


class VaultSetRequest(BaseModel):
    name: str
    envelope: dict


class VaultDeleteRequest(BaseModel):
    name: str


@app.post("/vault/set")
async def vault_set(
    req: VaultSetRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Store a sealed envelope as a named secret in the principal's vault."""
    from ryeos_node.vault import set_secret

    try:
        set_secret(
            settings.cas_base_path,
            principal.fingerprint,
            req.name,
            req.envelope,
            settings.signing_key_dir,
        )
    except ValueError as e:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, str(e))

    return {"stored": req.name}


@app.get("/vault/list")
async def vault_list(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List secret names in the principal's vault (no values)."""
    from ryeos_node.vault import list_secrets

    names = list_secrets(settings.cas_base_path, principal.fingerprint)
    return {"names": names}


@app.post("/vault/delete")
async def vault_delete(
    req: VaultDeleteRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Delete a named secret from the principal's vault."""
    from ryeos_node.vault import delete_secret

    try:
        deleted = delete_secret(settings.cas_base_path, principal.fingerprint, req.name)
    except ValueError as e:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, str(e))

    if not deleted:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Secret '{req.name}' not found")
    return {"deleted": req.name}


# --- GC endpoints ---


class GCRequest(BaseModel):
    dry_run: bool = False
    aggressive: bool = False
    retention_days: int = 7
    max_manual_pushes: int = 3


@app.post("/gc")
async def gc_endpoint(
    req: GCRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Run GC for the authenticated user's CAS store."""
    user_root = settings.user_root(principal.fingerprint)
    cas_root = _principal_cas_root(principal, settings)

    if not user_root.exists():
        return {"status": "ok", "message": "No CAS data for this user"}

    policy = RetentionPolicy(
        manual_pushes=req.max_manual_pushes,
        daily_checkpoints=req.retention_days,
        max_success_executions=settings.gc_max_executions,
        max_failure_executions=settings.gc_max_executions,
    )

    result = await asyncio.to_thread(
        run_gc,
        user_root,
        cas_root,
        node_id=settings.rye_remote_name,
        dry_run=req.dry_run,
        aggressive=req.aggressive,
        policy=policy,
    )
    return result.to_dict()


def _human_bytes(n: int) -> str:
    v = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if abs(v) < 1024:
            return f"{v:.1f} {unit}"
        v /= 1024
    return f"{v:.1f} TB"


@app.get("/gc/stats")
async def gc_stats(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Return GC state and recent history for the authenticated user."""
    user_root = settings.user_root(principal.fingerprint)

    total_bytes = _measure_usage(user_root) if user_root.exists() else 0

    gc_state = load_gc_state(user_root)
    lock = read_gc_lock(user_root)

    gc_log = user_root / "logs" / "gc.jsonl"
    recent_events: list = []
    if gc_log.is_file():
        try:
            lines = gc_log.read_text(encoding="utf-8").strip().split("\n")
            for line in lines[-10:]:
                if line.strip():
                    try:
                        recent_events.append(json.loads(line))
                    except json.JSONDecodeError:
                        pass
        except OSError:
            pass

    inflight_count = 0
    inflight_dir = user_root / "inflight"
    if inflight_dir.is_dir():
        try:
            inflight_count = sum(1 for f in inflight_dir.iterdir() if f.is_file())
        except OSError:
            pass

    return {
        "usage_bytes": total_bytes,
        "usage_human": _human_bytes(total_bytes),
        "quota_bytes": settings.max_user_storage_bytes,
        "quota_percent": round((total_bytes / settings.max_user_storage_bytes) * 100, 1)
        if settings.max_user_storage_bytes
        else 0,
        "gc_state": gc_state.to_dict() if gc_state else None,
        "gc_lock": lock.to_dict() if lock else None,
        "recent_events": recent_events,
        "inflight_epochs": inflight_count,
    }


# --- Webhook binding management ---


@app.post("/webhook-bindings")
async def create_webhook_binding(
    req: CreateWebhookBindingRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Create a webhook binding. Returns hook_id and hmac_secret (shown once)."""
    from rye.constants import ItemType

    kind, bare_id = ItemType.parse_canonical_ref(req.item_id)
    if not kind or not bare_id:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "item_id must be a canonical ref (e.g. directive:email/handle_inbound)",
        )

    return create_binding(
        settings.cas_base_path,
        principal.fingerprint,
        settings.rye_remote_name,
        req.item_id,
        req.project_path,
        req.description,
        req.secret_envelope,
        owner=principal.owner,
        vault_keys=req.vault_keys,
    )


@app.get("/webhook-bindings")
async def list_webhook_bindings(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List principal's webhook bindings (hmac_secret excluded)."""
    bindings_list = list_bindings(
        settings.cas_base_path,
        principal.fingerprint,
        settings.rye_remote_name,
    )
    return {"bindings": bindings_list}


@app.delete("/webhook-bindings/{hook_id}")
async def revoke_webhook_binding(
    hook_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Revoke a webhook binding (soft delete via revoked_at)."""
    if not revoke_binding(
        settings.cas_base_path,
        hook_id,
        principal.fingerprint,
        settings.rye_remote_name,
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
    namespace = body.item_id.split("/")[0] if "/" in body.item_id else body.item_id
    require_publish_namespace(principal, namespace)
    result = publish_item(
        settings.cas_base_path,
        body.kind,
        body.item_id,
        body.version,
        body.manifest_hash,
        f"fp:{principal.fingerprint}",
    )
    if not result.get("ok"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST, result.get("error", "Publish failed")
        )
    return result


@app.get("/registry/search")
async def registry_search(
    query: str | None = None,
    kind: str | None = None,
    namespace: str | None = None,
    limit: int = 20,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    results = search_items(settings.cas_base_path, query, kind, namespace, limit)
    return {"results": results, "total": len(results)}


@app.get("/registry/items/{kind}/{item_id:path}/versions/{version}")
async def registry_get_version(
    kind: str,
    item_id: str,
    version: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    ver = get_version(settings.cas_base_path, kind, item_id, version)
    if ver is None:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"Version not found: {kind}/{item_id}@{version}",
        )
    return ver


@app.get("/registry/items/{kind}/{item_id:path}")
async def registry_get_item(
    kind: str,
    item_id: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    item = get_item(settings.cas_base_path, kind, item_id)
    if item is None:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND, f"Item not found: {kind}/{item_id}"
        )
    return item


@app.post("/registry/namespaces/claim")
async def registry_claim_namespace(
    body: ClaimNamespaceRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    if not settings.registry_enabled:
        raise HTTPException(status.HTTP_404_NOT_FOUND, "Registry not enabled")
    result = claim_namespace(settings.cas_base_path, body.claim)
    if not result.get("ok"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST, result.get("error", "Claim failed")
        )
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
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST, result.get("error", "Registration failed")
        )
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
        raise HTTPException(
            status.HTTP_404_NOT_FOUND, f"Identity not found: {fingerprint}"
        )
    return doc


# --- Admin: authorized key management ---


@app.post("/admin/authorized-keys")
async def admin_authorize_key(
    req: AuthorizeKeyRequest,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Authorize a new key on this node."""
    if not req.public_key.startswith("ed25519:"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "public_key must start with 'ed25519:'",
        )

    import base64
    import time as _time
    from rye.primitives.signing import (
        compute_key_fingerprint,
        load_keypair,
        sign_hash,
    )

    pub_pem = base64.b64decode(req.public_key[len("ed25519:") :])
    fp = compute_key_fingerprint(pub_pem)
    timestamp = _time.strftime("%Y-%m-%dT%H:%M:%SZ", _time.gmtime())

    caps_toml = ", ".join(f'"{c}"' for c in req.scopes)
    body = (
        f'fingerprint = "{fp}"\n'
        f'public_key = "{req.public_key}"\n'
        f'label = "{req.label}"\n'
        f"scopes = [{caps_toml}]\n"
        f'created_via = "api"\n'
        f'created_at = "{timestamp}"\n'
        f'created_by = "fp:{principal.fingerprint}"\n'
    )

    node_priv, node_pub = load_keypair(Path(settings.signing_key_dir))
    node_fp = compute_key_fingerprint(node_pub)
    content_hash = hashlib.sha256(body.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, node_priv)

    signed_content = (
        f"# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{node_fp}\n{body}"
    )

    auth_dir = settings.authorized_keys_dir()
    auth_dir.mkdir(parents=True, exist_ok=True)
    key_file = auth_dir / f"{fp}.toml"
    key_file.write_text(signed_content, encoding="utf-8")

    logger.info(
        "Authorized key fp:%s (label=%s) by fp:%s",
        fp,
        req.label,
        principal.fingerprint,
    )

    return {
        "fingerprint": f"fp:{fp}",
        "label": req.label,
        "scopes": req.scopes,
        "authorized_by": f"fp:{principal.fingerprint}",
    }


@app.get("/admin/authorized-keys")
async def admin_list_keys(
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """List authorized keys on this node."""
    auth_dir = settings.authorized_keys_dir()
    if not auth_dir.is_dir():
        return {"keys": []}

    keys = []
    for f in sorted(auth_dir.iterdir()):
        if f.suffix != ".toml":
            continue
        try:
            raw = f.read_text()
            lines = raw.split("\n", 1)
            body = lines[1] if len(lines) > 1 else ""
            try:
                import tomllib as _tomllib
            except ModuleNotFoundError:
                import tomli as _tomllib  # type: ignore[no-redef]
            data = _tomllib.loads(body)
            keys.append(
                {
                    "fingerprint": f"fp:{data.get('fingerprint', f.stem)}",
                    "label": data.get("label", data.get("owner", "")),
                    "scopes": data.get("scopes", []),
                    "created_via": data.get("created_via", "unknown"),
                    "created_at": data.get("created_at", ""),
                }
            )
        except Exception:
            continue

    return {"keys": keys}


@app.delete("/admin/authorized-keys/{fingerprint}")
async def admin_revoke_key(
    fingerprint: str,
    principal: Principal = Depends(get_current_principal),
    settings: Settings = Depends(get_settings),
):
    """Revoke an authorized key. Cannot revoke the last key with admin capability."""
    auth_dir = settings.authorized_keys_dir()
    key_file = auth_dir / f"{fingerprint}.toml"
    if not key_file.exists():
        raise HTTPException(
            status.HTTP_404_NOT_FOUND, f"Key fp:{fingerprint} not found"
        )

    # Prevent revoking the last admin key
    admin_count = 0
    for f in auth_dir.iterdir():
        if f.suffix != ".toml":
            continue
        try:
            raw = f.read_text()
            lines = raw.split("\n", 1)
            body = lines[1] if len(lines) > 1 else ""
            try:
                import tomllib as _tomllib2
            except ModuleNotFoundError:
                import tomli as _tomllib2  # type: ignore[no-redef]
            data = _tomllib2.loads(body)
            caps = data.get("scopes", [])
            if "*" in caps or "node.auth.manage" in caps:
                admin_count += 1
        except Exception:
            continue

    # Check if we're about to remove an admin key
    try:
        raw = key_file.read_text()
        lines = raw.split("\n", 1)
        body = lines[1] if len(lines) > 1 else ""
        try:
            import tomllib as _tomllib3
        except ModuleNotFoundError:
            import tomli as _tomllib3  # type: ignore[no-redef]
        data = _tomllib3.loads(body)
        caps = data.get("scopes", [])
        is_admin = "*" in caps or "node.auth.manage" in caps
    except Exception:
        is_admin = False

    if is_admin and admin_count <= 1:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "Cannot revoke the last admin key",
        )

    key_file.unlink()
    logger.info("Revoked key fp:%s by fp:%s", fingerprint, principal.fingerprint)

    return {"revoked": f"fp:{fingerprint}"}
