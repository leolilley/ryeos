"""CAS-native remote execution server.

Endpoints: /health, /public-key, /objects/has, /objects/put, /objects/get,
           /push, /push/user-space, /user-space,
           /execute, /search, /load, /sign,
           /threads, /threads/{thread_id},
           /secrets (POST, GET), /secrets/{name} (DELETE),
           /webhook-bindings (POST, GET), /webhook-bindings/{hook_id} (DELETE)
"""

import asyncio
import datetime
import hashlib
import json
import logging
import os
import random
import secrets
import shutil
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

from fastapi import Depends, FastAPI, HTTPException, Request, status
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.middleware.gzip import GZipMiddleware

from ryeos_remote.auth import (
    ResolvedExecution,
    User,
    check_replay,
    get_current_user,
    require_scope,
    verify_hmac,
    verify_timestamp,
)
from ryeos_remote.config import Settings, get_settings

RESERVED_ENV_NAMES = frozenset({
    "PATH", "PYTHONPATH", "HOME", "USER", "SHELL", "LANG", "TERM",
    "LC_ALL", "LC_CTYPE", "TMPDIR", "TMP", "TEMP",
    # Internal RYE vars set by the server — block individually, not by prefix
    "RYE_SIGNING_KEY_DIR", "RYE_KERNEL_PYTHON", "RYE_REMOTE_NAME",
})

RESERVED_ENV_PREFIXES = (
    "SUPABASE_", "MODAL_", "LD_", "SSL_", "AWS_",
    "GOOGLE_", "AZURE_", "GITHUB_", "CI_", "DOCKER_",
)


def _is_safe_secret_name(name: str) -> bool:
    """Check if a secret name is safe to inject into os.environ."""
    if not name or not name.isidentifier():
        return False
    upper = name.upper()
    if upper in RESERVED_ENV_NAMES:
        return False
    return not any(upper.startswith(p) for p in RESERVED_ENV_PREFIXES)

from lillux.primitives import cas
from lillux.primitives.integrity import compute_integrity

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
from rye.tools.execute import ExecuteTool

logger = logging.getLogger(__name__)

app = FastAPI(title="ryeos-remote", version="0.1.0")

# m3: Gzip compression for responses
app.add_middleware(GZipMiddleware, minimum_size=1000)


# m1: Enforce batch size limits (reads actual body, not just Content-Length header)
@app.middleware("http")
async def enforce_request_size(request: Request, call_next):
    if request.url.path == "/health":
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


class SecretsUpsertRequest(BaseModel):
    secrets: List[Dict[str, str]]


class CreateWebhookBindingRequest(BaseModel):
    item_type: str
    item_id: str
    project_path: str
    description: Optional[str] = None


# --- Helpers ---


def _user_cas_root(user: User, settings: Settings) -> Path:
    return settings.user_cas_root(user.id)


def _check_user_quota(user: User, settings: Settings) -> None:
    """Reject if user CAS exceeds storage quota."""
    root = _user_cas_root(user, settings)
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


def _inject_user_secrets(user: User, settings: Settings) -> list[tuple[str, str | None]]:
    """Fetch user secrets from Vault and inject into os.environ.

    Returns list of (key, previous_value_or_None) tuples for cleanup.
    Safe under max_inputs=1 — no concurrent requests share this process.
    """
    try:
        from supabase import create_client

        supabase = create_client(settings.supabase_url, settings.supabase_service_key)
        result = supabase.rpc(
            "get_user_secrets", {"p_user_id": user.id}
        ).execute()

        injected: list[tuple[str, str | None]] = []
        for row in result.data or []:
            name = row["name"]
            if not _is_safe_secret_name(name):
                logger.warning("Skipping unsafe secret name: %s for user %s", name, user.username)
                continue
            old_value = os.environ.get(name)
            os.environ[name] = row["decrypted_value"]
            injected.append((name, old_value))

        if injected:
            logger.info("Injected %d user secrets for %s", len(injected), user.username)
        return injected
    except Exception:
        logger.warning("Failed to fetch user secrets", exc_info=True)
        return []


def _get_supabase(settings: Settings):
    """Get Supabase client (service_role for server-side writes)."""
    from supabase import create_client
    return create_client(settings.supabase_url, settings.supabase_service_key)


def _register_thread(
    settings: Settings,
    user: User,
    thread_id: str,
    item_type: str,
    item_id: str,
    project_manifest_hash: str,
    user_manifest_hash: str,
    project_path: Optional[str] = None,
) -> None:
    """Register a thread row in Supabase (state=running)."""
    try:
        sb = _get_supabase(settings)
        sb.table("threads").insert({
            "thread_id": thread_id,
            "user_id": user.id,
            "item_type": item_type,
            "item_id": item_id,
            "execution_mode": "remote",
            "remote_name": settings.rye_remote_name,
            "project_path": project_path,
            "project_manifest_hash": project_manifest_hash,
            "user_manifest_hash": user_manifest_hash,
            "system_version": get_system_version(),
            "state": "running",
        }).execute()
    except Exception:
        logger.warning("Failed to register thread %s", thread_id, exc_info=True)


def _complete_thread(
    settings: Settings,
    thread_id: str,
    state: str,
    snapshot_hash: Optional[str] = None,
    runtime_outputs_bundle_hash: Optional[str] = None,
) -> None:
    """Update thread state to completed/error."""
    try:
        sb = _get_supabase(settings)
        update = {"state": state, "completed_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
        if snapshot_hash:
            update["snapshot_hash"] = snapshot_hash
        if runtime_outputs_bundle_hash:
            update["runtime_outputs_bundle_hash"] = runtime_outputs_bundle_hash
        sb.table("threads").update(update).eq(
            "thread_id", thread_id
        ).execute()
    except Exception:
        logger.warning("Failed to complete thread %s", thread_id, exc_info=True)


def _upsert_project_ref(
    settings: Settings,
    user: User,
    project_path: str,
    project_manifest_hash: str,
    system_version: str,
    snapshot_hash: Optional[str] = None,
    expected_revision: Optional[int] = None,
) -> None:
    """Upsert project_refs row with optional optimistic lock on snapshot_revision.

    If expected_revision is provided, uses compare-and-swap on snapshot_revision
    to prevent concurrent HEAD advances from clobbering each other.
    Raises HTTPException(409) if the revision has moved.
    """
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    sb = _get_supabase(settings)
    row = {
        "user_id": user.id,
        "remote_name": settings.rye_remote_name,
        "project_path": project_path,
        "project_manifest_hash": project_manifest_hash,
        "system_version": system_version,
        "pushed_at": now,
    }
    if snapshot_hash is not None:
        row["snapshot_hash"] = snapshot_hash
        row["head_updated_at"] = now

    if expected_revision is not None:
        row["snapshot_revision"] = expected_revision + 1
        # Optimistic CAS: update only if revision matches
        result = (
            sb.table("project_refs")
            .update(row)
            .eq("user_id", user.id)
            .eq("remote_name", settings.rye_remote_name)
            .eq("project_path", project_path)
            .eq("snapshot_revision", expected_revision)
            .execute()
        )
        if not result.data:
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                "HEAD revision moved during push. Pull and retry.",
            )
    else:
        sb.table("project_refs").upsert(row).execute()


def _resolve_project_ref(
    settings: Settings,
    user: User,
    project_path: str,
) -> Dict[str, Any]:
    """Look up project_refs to resolve project manifest and snapshot state.

    Returns dict with project_manifest_hash, system_version,
    snapshot_hash, snapshot_revision.
    Raises HTTPException if not found.
    """
    sb = _get_supabase(settings)
    result = (
        sb.table("project_refs")
        .select("project_manifest_hash, system_version, snapshot_hash, snapshot_revision")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .eq("project_path", project_path)
        .execute()
    )
    if not result.data:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"No project ref '{project_path}' found on remote '{settings.rye_remote_name}'. "
            f"Push first: rye execute tool rye/core/remote/remote action=push",
        )
    return result.data[0]


def _resolve_user_space_ref(
    settings: Settings,
    user: User,
) -> Optional[Dict[str, Any]]:
    """Look up user_space_refs for user's current user space.

    Returns dict with user_manifest_hash, snapshot_revision, pushed_at.
    Returns None if no user space has been pushed.
    """
    sb = _get_supabase(settings)
    result = (
        sb.table("user_space_refs")
        .select("user_manifest_hash, snapshot_revision, pushed_at")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .execute()
    )
    if not result.data:
        return None
    return result.data[0]


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


@app.get("/public-key")
async def public_key(settings: Settings = Depends(get_settings)):
    from lillux.primitives.signing import ensure_keypair
    ensure_keypair(Path(settings.signing_key_dir))
    key_path = Path(settings.signing_key_dir) / "public_key.pem"
    return {"public_key_pem": key_path.read_text()}


@app.post("/objects/has")
async def objects_has(
    req: HasObjectsRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    require_scope(user, "remote:objects")
    root = _user_cas_root(user, settings)
    return handle_has_objects(req.hashes, root)


@app.post("/objects/put")
async def objects_put(
    req: PutObjectsRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    require_scope(user, "remote:objects")
    _check_user_quota(user, settings)
    root = _user_cas_root(user, settings)
    result = handle_put_objects(req.entries, root)
    if result.get("errors"):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, result)
    return result


@app.post("/objects/get")
async def objects_get(
    req: GetObjectsRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    require_scope(user, "remote:objects")
    root = _user_cas_root(user, settings)
    return handle_get_objects(req.hashes, root)


@app.post("/push")
async def push(
    req: PushRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Finalize a push — validate manifest graph, create snapshot, advance HEAD."""
    require_scope(user, "remote:push")
    root = _user_cas_root(user, settings)

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
    sb = _get_supabase(settings)
    ref_result = (
        sb.table("project_refs")
        .select("snapshot_hash, snapshot_revision")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .eq("project_path", req.project_path)
        .execute()
    )
    ref = ref_result.data[0] if ref_result.data else None
    current_head = ref["snapshot_hash"] if ref else None
    current_rev = ref["snapshot_revision"] if ref else 0

    # Reject if client is behind (expected_snapshot_hash mismatch)
    if req.expected_snapshot_hash is not None or current_head is not None:
        if req.expected_snapshot_hash != current_head:
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                {
                    "error": "HEAD has moved",
                    "expected": req.expected_snapshot_hash,
                    "actual": current_head,
                    "revision": current_rev,
                },
            )

    # Resolve user space hash for snapshot (may be None if never pushed)
    user_ref = _resolve_user_space_ref(settings, user)
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

    # Advance HEAD
    if ref is not None:
        _upsert_project_ref(
            settings, user,
            project_path=req.project_path,
            project_manifest_hash=req.project_manifest_hash,
            system_version=req.system_version,
            snapshot_hash=snapshot_hash,
            expected_revision=current_rev,
        )
    else:
        _upsert_project_ref(
            settings, user,
            project_path=req.project_path,
            project_manifest_hash=req.project_manifest_hash,
            system_version=req.system_version,
            snapshot_hash=snapshot_hash,
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
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Push user space independently from projects."""
    require_scope(user, "remote:push")
    root = _user_cas_root(user, settings)

    # Deep validation: verify manifest schema + full transitive object graph
    _validate_manifest_graph(
        req.user_manifest_hash,
        root,
        expected_space="user",
        label="user_manifest",
    )

    sb = _get_supabase(settings)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()

    if req.expected_revision is not None:
        # Optimistic CAS update
        result = (
            sb.table("user_space_refs")
            .update({
                "user_manifest_hash": req.user_manifest_hash,
                "snapshot_revision": req.expected_revision + 1,
                "pushed_at": now,
            })
            .eq("user_id", user.id)
            .eq("remote_name", settings.rye_remote_name)
            .eq("snapshot_revision", req.expected_revision)
            .execute()
        )
        if not result.data:
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                "User space revision moved. Fetch current state and retry.",
            )
    else:
        # First push — insert only (reject if row exists to prevent silent overwrite)
        existing = (
            sb.table("user_space_refs")
            .select("snapshot_revision")
            .eq("user_id", user.id)
            .eq("remote_name", settings.rye_remote_name)
            .execute()
        )
        if existing.data:
            raise HTTPException(
                status.HTTP_409_CONFLICT,
                f"User space already exists at revision {existing.data[0]['snapshot_revision']}. "
                "Provide expected_revision for optimistic update.",
            )
        sb.table("user_space_refs").insert({
            "user_id": user.id,
            "remote_name": settings.rye_remote_name,
            "user_manifest_hash": req.user_manifest_hash,
            "snapshot_revision": 1,
            "pushed_at": now,
        }).execute()

    return {
        "status": "ok",
        "user_manifest_hash": req.user_manifest_hash,
        "remote_name": settings.rye_remote_name,
    }


@app.get("/user-space")
async def get_user_space(
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Get current user space ref."""
    require_scope(user, "remote:push")
    ref = _resolve_user_space_ref(settings, user)
    if not ref:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            "No user space pushed yet.",
        )
    return {
        "user_manifest_hash": ref["user_manifest_hash"],
        "snapshot_revision": ref["snapshot_revision"],
        "pushed_at": ref["pushed_at"],
        "remote_name": settings.rye_remote_name,
    }


# --- Webhook binding lookup ---


def _lookup_binding(hook_id: str, settings: Settings) -> dict:
    """Look up an active webhook binding. Returns generic 401 on not found/revoked."""
    sb = _get_supabase(settings)
    result = (
        sb.table("webhook_bindings")
        .select("*")
        .eq("hook_id", hook_id)
        .eq("remote_name", settings.rye_remote_name)
        .execute()
    )
    if not result.data:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    binding = result.data[0]
    if binding.get("revoked_at"):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    return binding


def _resolve_user_from_binding(binding: dict, settings: Settings) -> User:
    """Resolve the User who owns a webhook binding."""
    sb = _get_supabase(settings)
    result = (
        sb.table("users")
        .select("id, username, email")
        .eq("id", binding["user_id"])
        .execute()
    )
    if not result.data:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    u = result.data[0]
    return User(id=u["id"], username=u["username"], email=u.get("email"))


# --- Dual-auth resolve_execution ---


async def resolve_execution(
    request: Request,
    settings: Settings = Depends(get_settings),
) -> ResolvedExecution:
    """Determine auth mode from headers and return normalized ResolvedExecution.

    - X-Webhook-Timestamp header → webhook HMAC path (binding controls what executes)
    - Authorization header → bearer API key path (caller controls everything)
    - Both or neither → 401
    """
    raw_body = await request.body()
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid JSON body")
    if not isinstance(body, dict):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "JSON body must be an object")

    has_bearer = bool(request.headers.get("authorization"))
    has_webhook = bool(request.headers.get("x-webhook-timestamp"))

    if has_bearer == has_webhook:
        raise HTTPException(
            status.HTTP_401_UNAUTHORIZED,
            "Provide exactly one auth mode: Authorization header OR webhook headers",
        )

    if has_webhook:
        # Webhook path — HMAC auth, binding controls what executes
        timestamp = request.headers.get("x-webhook-timestamp", "")
        signature = request.headers.get("x-webhook-signature", "")
        delivery_id = request.headers.get("x-webhook-delivery-id", "")
        hook_id = body.get("hook_id")
        if not hook_id:
            raise HTTPException(status.HTTP_400_BAD_REQUEST, "Webhook request requires hook_id")

        binding = _lookup_binding(hook_id, settings)
        verify_timestamp(timestamp)
        verify_hmac(timestamp, raw_body, binding["hmac_secret"], signature)
        check_replay(hook_id, delivery_id, settings)

        user = _resolve_user_from_binding(binding, settings)
        thread = "fork" if binding["item_type"] == "directive" else "inline"

        parameters = body.get("parameters", {})
        if not isinstance(parameters, dict):
            raise HTTPException(status.HTTP_400_BAD_REQUEST, "parameters must be an object")

        return ResolvedExecution(
            user=user,
            item_type=binding["item_type"],
            item_id=binding["item_id"],
            project_path=binding["project_path"],
            parameters=parameters,
            thread=thread,
        )

    # Bearer path — caller controls everything
    from fastapi.security import HTTPAuthorizationCredentials

    auth_header = request.headers.get("authorization", "")
    if not auth_header.lower().startswith("bearer "):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid authorization header")
    token = auth_header[7:]
    from ryeos_remote.auth import _resolve_api_key, API_KEY_PREFIX
    if not token.startswith(API_KEY_PREFIX):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid token — use an API key")
    user = await _resolve_api_key(token, settings)
    require_scope(user, "remote:execute")

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
        user=user,
        item_type=item_type,
        item_id=item_id,
        project_path=project_path,
        parameters=parameters,
        thread=thread,
    )


@app.post("/execute")
async def execute(
    resolved: ResolvedExecution = Depends(resolve_execution),
    settings: Settings = Depends(get_settings),
):
    return await _execute_from_head(
        user=resolved.user,
        settings=settings,
        project_path=resolved.project_path,
        item_type=resolved.item_type,
        item_id=resolved.item_id,
        parameters=resolved.parameters,
        thread=resolved.thread,
    )


# --- First-class tool endpoints (search/load/sign) ---


@app.post("/search")
async def search_items(
    request: Request,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Search for rye items. Wraps execute with item_type=tool, item_id=rye/search."""
    require_scope(user, "remote:execute")
    raw_body = await request.body()
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid JSON body")
    if not isinstance(body, dict):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "JSON body must be an object")

    project_path = body.pop("project_path", None)
    if not project_path:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "project_path is required")

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type="tool",
        item_id="rye/search",
        parameters=body,
        thread="inline",
    )


@app.post("/load")
async def load_item(
    request: Request,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Load/inspect a rye item. Wraps execute with item_type=tool, item_id=rye/load."""
    require_scope(user, "remote:execute")
    raw_body = await request.body()
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid JSON body")
    if not isinstance(body, dict):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "JSON body must be an object")

    project_path = body.pop("project_path", None)
    if not project_path:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "project_path is required")

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type="tool",
        item_id="rye/load",
        parameters=body,
        thread="inline",
    )


@app.post("/sign")
async def sign_item(
    request: Request,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Sign a rye item. Wraps execute with item_type=tool, item_id=rye/sign."""
    require_scope(user, "remote:execute")
    raw_body = await request.body()
    try:
        body = json.loads(raw_body)
    except (json.JSONDecodeError, UnicodeDecodeError):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "Invalid JSON body")
    if not isinstance(body, dict):
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "JSON body must be an object")

    project_path = body.pop("project_path", None)
    if not project_path:
        raise HTTPException(status.HTTP_400_BAD_REQUEST, "project_path is required")

    return await _execute_from_head(
        user=user,
        settings=settings,
        project_path=project_path,
        item_type="tool",
        item_id="rye/sign",
        parameters=body,
        thread="inline",
    )


# --- Execution from HEAD ---

MAX_FOLD_BACK_RETRIES = 5
FOLD_BACK_BASE_JITTER_MS = 50


async def _execute_from_head(
    user: User,
    settings: Settings,
    project_path: str,
    item_type: str,
    item_id: str,
    parameters: Dict[str, Any],
    thread: str,
):
    """Execute from project HEAD — isolated mutable checkout with fold-back."""
    ref = _resolve_project_ref(settings, user, project_path)
    user_ref = _resolve_user_space_ref(settings, user)
    user_manifest_hash = user_ref["user_manifest_hash"] if user_ref else None

    base_snapshot_hash = ref["snapshot_hash"]
    if not base_snapshot_hash:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Project '{project_path}' has no snapshot. Re-push to create one.",
        )
    _check_system_version(ref["system_version"])

    root = _user_cas_root(user, settings)
    cache = settings.cache_root(user.id)
    exec_root = settings.exec_root(user.id)
    thread_id = f"rye-remote-{uuid.uuid4().hex[:12]}"
    exec_space: Path | None = None
    injected_keys: list[tuple[str, str | None]] = []

    _register_thread(
        settings, user, thread_id,
        item_type=item_type,
        item_id=item_id,
        project_manifest_hash=ref["project_manifest_hash"],
        user_manifest_hash=user_manifest_hash,
        project_path=project_path,
    )

    try:
        # Checkout mutable copy from snapshot cache
        exec_space = create_execution_space(
            base_snapshot_hash, thread_id, root, cache, exec_root,
        )
        user_space = ensure_user_space_cached(
            user_manifest_hash, root, cache,
        ) if user_manifest_hash else None

        os.environ["RYE_SIGNING_KEY_DIR"] = settings.signing_key_dir

        # Set USER_SPACE so resolvers/walkers find pushed user-space items
        # (safe under max_inputs=1 — one request per container process)
        if user_space:
            os.environ["USER_SPACE"] = str(user_space)
        else:
            empty_user = exec_space / ".empty_user_space"
            empty_user.mkdir(exist_ok=True)
            os.environ["USER_SPACE"] = str(empty_user)

        injected_keys = _inject_user_secrets(user, settings)

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
                project_manifest_hash=ref["project_manifest_hash"],
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
            _complete_thread(
                settings, thread_id,
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
            user, settings, project_path,
            base_snapshot_hash, proj_snapshot_hash,
            root, cache, thread_id,
            user_manifest_hash=user_manifest_hash,
        )

        _check_user_quota(user, settings)

        exec_status = result.get("status", "unknown")
        _complete_thread(
            settings, thread_id,
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
        _complete_thread(settings, thread_id, state="error")
        raise HTTPException(status.HTTP_404_NOT_FOUND, str(e))
    except Exception:
        _complete_thread(settings, thread_id, state="error")
        raise
    finally:
        os.environ.pop("USER_SPACE", None)
        for key, old_value in injected_keys:
            if old_value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = old_value
        if exec_space:
            cleanup_execution_space(exec_space)


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
    user: User,
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
        ref = _resolve_project_ref(settings, user, project_path)
        current_head = ref["snapshot_hash"]
        current_rev = ref["snapshot_revision"]

        if current_head == base_snapshot_hash:
            # Fast-forward — HEAD hasn't moved
            exec_snap_obj = cas.get_object(exec_snapshot_hash, cas_root)
            exec_pm = exec_snap_obj["project_manifest_hash"] if exec_snap_obj else None
            if _try_advance_head(
                settings, user, project_path,
                exec_snapshot_hash, current_rev,
                project_manifest_hash=exec_pm,
            ):
                _update_snapshot_cache(
                    settings, user, project_path,
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
                _store_conflict_record(
                    settings, user, project_path,
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
                settings, user, project_path,
                merge_snapshot_hash, current_rev,
                project_manifest_hash=merged_manifest_hash,
            ):
                _update_snapshot_cache(
                    settings, user, project_path,
                    merge_snapshot_hash, cas_root, cache_root,
                )
                return {"snapshot_hash": merge_snapshot_hash, "merge_type": "merge"}

        # CAS update raced — back off with jitter and retry
        jitter = FOLD_BACK_BASE_JITTER_MS * (2 ** attempt) + random.randint(0, 50)
        await asyncio.sleep(jitter / 1000)

    # Exhausted retries — persist unmerged snapshot for later inspection
    _store_conflict_record(
        settings, user, project_path,
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
    user: User,
    project_path: str,
    new_snapshot_hash: str,
    expected_rev: int,
    project_manifest_hash: Optional[str] = None,
) -> bool:
    """Optimistic CAS on snapshot_revision. Returns True if update succeeded.

    Updates full project_refs metadata to prevent stale cached values
    after fold-back.
    """
    sb = _get_supabase(settings)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    update = {
        "snapshot_hash": new_snapshot_hash,
        "snapshot_revision": expected_rev + 1,
        "head_updated_at": now,
    }
    if project_manifest_hash is not None:
        update["project_manifest_hash"] = project_manifest_hash
        update["system_version"] = get_system_version()
    result = (
        sb.table("project_refs")
        .update(update)
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .eq("project_path", project_path)
        .eq("snapshot_revision", expected_rev)
        .execute()
    )
    return bool(result.data)


def _update_snapshot_cache(
    settings: Settings,
    user: User,
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


def _store_conflict_record(
    settings: Settings,
    user: User,
    project_path: str,
    thread_id: str,
    conflicts: dict,
    unmerged_snapshot: str,
) -> None:
    """Store merge conflict record in Supabase for later resolution."""
    try:
        sb = _get_supabase(settings)
        sb.table("threads").update({
            "merge_conflicts": conflicts,
            "unmerged_snapshot_hash": unmerged_snapshot,
        }).eq("thread_id", thread_id).execute()
    except Exception:
        logger.warning(
            "Failed to store conflict record for thread %s",
            thread_id, exc_info=True,
        )


@app.get("/threads")
async def list_threads(
    limit: int = 20,
    project_path: Optional[str] = None,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """List user's remote executions on this remote."""
    require_scope(user, "remote:threads")
    sb = _get_supabase(settings)
    query = (
        sb.table("threads")
        .select("*")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .order("created_at", desc=True)
        .limit(limit)
    )
    if project_path:
        query = query.eq("project_path", project_path)
    result = query.execute()
    return {"threads": result.data or [], "remote_name": settings.rye_remote_name}


@app.get("/threads/{thread_id}")
async def get_thread(
    thread_id: str,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Get status of a specific thread."""
    require_scope(user, "remote:threads")
    sb = _get_supabase(settings)
    result = (
        sb.table("threads")
        .select("*")
        .eq("thread_id", thread_id)
        .eq("user_id", user.id)
        .execute()
    )
    if not result.data:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Thread {thread_id} not found")
    return result.data[0]


# --- History ---


@app.get("/history")
async def history(
    project_path: str,
    limit: int = 50,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Walk first-parent snapshot chain from project HEAD."""
    require_scope(user, "remote:threads")
    ref = _resolve_project_ref(settings, user, project_path)
    root = _user_cas_root(user, settings)
    snapshots = get_history(ref["snapshot_hash"], root, limit=min(limit, 200))
    return {
        "project_path": project_path,
        "head": ref["snapshot_hash"],
        "snapshots": snapshots,
        "remote_name": settings.rye_remote_name,
    }


# --- Secrets management ---


@app.post("/secrets")
async def upsert_secrets(
    req: SecretsUpsertRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Upsert user secrets into the vault for remote execution."""
    require_scope(user, "remote:secrets")
    sb = _get_supabase(settings)
    stored = []
    for entry in req.secrets:
        name = entry.get("name", "")
        value = entry.get("value", "")
        if not name or not value:
            continue
        try:
            sb.rpc(
                "upsert_user_secret",
                {"p_user_id": user.id, "p_name": name, "p_value": value},
            ).execute()
            stored.append(name)
        except Exception:
            logger.warning("Failed to upsert secret %s", name, exc_info=True)
    return {"stored": stored, "count": len(stored)}


@app.get("/secrets")
async def list_secrets(
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """List user's secret names (values are never returned)."""
    require_scope(user, "remote:secrets")
    sb = _get_supabase(settings)
    result = (
        sb.table("user_secrets")
        .select("name, created_at, updated_at")
        .eq("user_id", user.id)
        .order("name")
        .execute()
    )
    return {"secrets": result.data or []}


@app.delete("/secrets/{name}")
async def delete_secret(
    name: str,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Delete a user secret by name."""
    require_scope(user, "remote:secrets")
    sb = _get_supabase(settings)
    result = sb.rpc(
        "delete_user_secret",
        {"p_user_id": user.id, "p_name": name},
    ).execute()
    deleted = result.data if result.data else False
    if not deleted:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Secret '{name}' not found")
    return {"deleted": name}


# --- Webhook binding management ---


@app.post("/webhook-bindings")
async def create_webhook_binding(
    req: CreateWebhookBindingRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Create a webhook binding. Returns hook_id and hmac_secret (shown once)."""
    require_scope(user, "remote:webhook-bindings")

    if req.item_type not in ("tool", "directive"):
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"item_type must be 'tool' or 'directive', got {req.item_type!r}",
        )

    hook_id = f"wh_{secrets.token_hex(16)}"
    hmac_secret = f"whsec_{secrets.token_hex(32)}"

    sb = _get_supabase(settings)
    sb.table("webhook_bindings").insert({
        "hook_id": hook_id,
        "user_id": user.id,
        "remote_name": settings.rye_remote_name,
        "item_type": req.item_type,
        "item_id": req.item_id,
        "project_path": req.project_path,
        "hmac_secret": hmac_secret,
        "description": req.description,
    }).execute()

    return {
        "hook_id": hook_id,
        "hmac_secret": hmac_secret,
        "item_type": req.item_type,
        "item_id": req.item_id,
        "project_path": req.project_path,
    }


@app.get("/webhook-bindings")
async def list_webhook_bindings(
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """List user's webhook bindings (hmac_secret excluded)."""
    require_scope(user, "remote:webhook-bindings")
    sb = _get_supabase(settings)
    result = (
        sb.table("webhook_bindings")
        .select("hook_id, item_type, item_id, project_path, description, created_at, revoked_at")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .order("created_at", desc=True)
        .execute()
    )
    return {"bindings": result.data or []}


@app.delete("/webhook-bindings/{hook_id}")
async def revoke_webhook_binding(
    hook_id: str,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Revoke a webhook binding (soft delete via revoked_at)."""
    require_scope(user, "remote:webhook-bindings")
    sb = _get_supabase(settings)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    result = (
        sb.table("webhook_bindings")
        .update({"revoked_at": now})
        .eq("hook_id", hook_id)
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .is_("revoked_at", "null")
        .execute()
    )
    if not result.data:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"Webhook binding '{hook_id}' not found or already revoked",
        )
    return {"revoked": hook_id}
