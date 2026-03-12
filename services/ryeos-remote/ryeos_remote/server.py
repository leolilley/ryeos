"""CAS-native remote execution server.

Endpoints: /health, /public-key, /objects/has, /objects/put, /objects/get,
           /push, /execute, /threads, /threads/{thread_id},
           /secrets (POST, GET), /secrets/{name} (DELETE)
"""

import asyncio
import datetime
import hashlib
import json
import logging
import os
import random
import shutil
import uuid
from pathlib import Path
from typing import Any, Dict, List, Optional

from fastapi import Depends, FastAPI, HTTPException, Request, status
from fastapi.responses import JSONResponse
from pydantic import BaseModel
from starlette.middleware.gzip import GZipMiddleware

from ryeos_remote.auth import User, get_current_user
from ryeos_remote.config import Settings, get_settings

from lillux.primitives import cas
from lillux.primitives.integrity import compute_integrity

from rye.cas.objects import ExecutionSnapshot, ProjectSnapshot, RuntimeOutputsBundle, SourceManifest
from rye.cas.sync import (
    handle_has_objects,
    handle_put_objects,
    handle_get_objects,
)
from rye.cas.materializer import (
    ExecutionPaths,
    cleanup,
    get_system_version,
    materialize,
)
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
    project_name: str
    project_manifest_hash: str
    user_manifest_hash: str
    system_version: str
    expected_snapshot_hash: Optional[str] = None  # None = first push


class ExecuteRequest(BaseModel):
    project_manifest_hash: Optional[str] = None
    user_manifest_hash: Optional[str] = None
    project_name: Optional[str] = None
    system_version: Optional[str] = None
    item_type: str
    item_id: str
    parameters: Dict[str, Any] = {}
    thread: str = "inline"


class SecretsUpsertRequest(BaseModel):
    secrets: List[Dict[str, str]]


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
    project_name: Optional[str] = None,
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
            "project_name": project_name,
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
    project_name: str,
    project_manifest_hash: str,
    user_manifest_hash: str,
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
        "project_name": project_name,
        "project_manifest_hash": project_manifest_hash,
        "user_manifest_hash": user_manifest_hash,
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
            .eq("project_name", project_name)
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
    project_name: str,
) -> Dict[str, Any]:
    """Look up project_refs to resolve manifest hashes and snapshot state.

    Returns dict with project_manifest_hash, user_manifest_hash, system_version,
    snapshot_hash, snapshot_revision.
    Raises HTTPException if not found.
    """
    sb = _get_supabase(settings)
    result = (
        sb.table("project_refs")
        .select("project_manifest_hash, user_manifest_hash, system_version, snapshot_hash, snapshot_revision")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .eq("project_name", project_name)
        .execute()
    )
    if not result.data:
        raise HTTPException(
            status.HTTP_404_NOT_FOUND,
            f"No project ref '{project_name}' found on remote '{settings.rye_remote_name}'. "
            f"Push first: rye execute tool rye/core/remote/remote action=push",
        )
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
    root = _user_cas_root(user, settings)
    return handle_has_objects(req.hashes, root)


@app.post("/objects/put")
async def objects_put(
    req: PutObjectsRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
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
    root = _user_cas_root(user, settings)
    return handle_get_objects(req.hashes, root)


@app.post("/push")
async def push(
    req: PushRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Finalize a push — verify manifests, create snapshot, advance HEAD."""
    root = _user_cas_root(user, settings)

    # Shallow check: verify both manifest hashes exist as CAS objects
    for label, h in [
        ("project_manifest", req.project_manifest_hash),
        ("user_manifest", req.user_manifest_hash),
    ]:
        obj = cas.get_object(h, root)
        if obj is None:
            raise HTTPException(
                status.HTTP_400_BAD_REQUEST,
                f"{label} object {h} not found in CAS. Upload objects first.",
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
        .eq("project_name", req.project_name)
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
                f"HEAD has moved: expected={req.expected_snapshot_hash}, "
                f"actual={current_head}. Pull and re-push.",
            )

    # Create ProjectSnapshot
    snapshot = ProjectSnapshot(
        project_manifest_hash=req.project_manifest_hash,
        user_manifest_hash=req.user_manifest_hash,
        parent_hashes=[current_head] if current_head else [],
        source="push",
        timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
    )
    snapshot_hash = cas.store_object(snapshot.to_dict(), root)

    # Advance HEAD
    if ref is not None:
        _upsert_project_ref(
            settings, user,
            project_name=req.project_name,
            project_manifest_hash=req.project_manifest_hash,
            user_manifest_hash=req.user_manifest_hash,
            system_version=req.system_version,
            snapshot_hash=snapshot_hash,
            expected_revision=current_rev,
        )
    else:
        _upsert_project_ref(
            settings, user,
            project_name=req.project_name,
            project_manifest_hash=req.project_manifest_hash,
            user_manifest_hash=req.user_manifest_hash,
            system_version=req.system_version,
            snapshot_hash=snapshot_hash,
        )

    return {
        "status": "ok",
        "remote_name": settings.rye_remote_name,
        "project_name": req.project_name,
        "project_manifest_hash": req.project_manifest_hash,
        "user_manifest_hash": req.user_manifest_hash,
        "snapshot_hash": snapshot_hash,
    }


@app.post("/execute")
async def execute(
    req: ExecuteRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    # Validate thread mode
    if req.item_type == "directive" and req.thread != "fork":
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Directives must use thread=fork on remote, got thread={req.thread!r}",
        )
    if req.item_type == "tool" and req.thread != "inline":
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Tools must use thread=inline on remote, got thread={req.thread!r}",
        )

    # Route: checkout-based (project_name only) vs tempdir (explicit hashes)
    if req.project_manifest_hash and req.user_manifest_hash:
        return await _execute_tempdir(req, user, settings)
    elif req.project_name:
        return await _execute_with_checkout(req, user, settings)
    else:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "Provide either (project_manifest_hash + user_manifest_hash) or project_name",
        )


async def _execute_tempdir(
    req: ExecuteRequest,
    user: User,
    settings: Settings,
):
    """Tempdir-based execution — existing flow for explicit manifest hashes."""
    project_manifest_hash = req.project_manifest_hash
    user_manifest_hash = req.user_manifest_hash
    project_name = req.project_name
    if req.system_version:
        _check_system_version(req.system_version)

    root = _user_cas_root(user, settings)
    paths: ExecutionPaths | None = None
    thread_id = f"rye-remote-{uuid.uuid4().hex[:12]}"
    injected_keys: list[tuple[str, str | None]] = []

    _register_thread(
        settings, user, thread_id,
        item_type=req.item_type,
        item_id=req.item_id,
        project_manifest_hash=project_manifest_hash,
        user_manifest_hash=user_manifest_hash,
        project_name=project_name,
    )

    try:
        paths = materialize(
            project_manifest_hash,
            user_manifest_hash,
            root,
        )

        os.environ["RYE_SIGNING_KEY_DIR"] = settings.signing_key_dir
        injected_keys = _inject_user_secrets(user, settings)

        tool = ExecuteTool(
            user_space=str(paths.user_space),
            project_path=str(paths.project_path),
        )

        result = await tool.handle(
            item_type=req.item_type,
            item_id=req.item_id,
            project_path=str(paths.project_path),
            parameters=req.parameters,
            thread=req.thread,
        )

        project_cas = paths.project_path / AI_DIR / "objects"
        new_hashes = _copy_cas_objects(project_cas, root)

        snapshot_hash = _find_execution_snapshot_hash(paths.project_path)
        if not snapshot_hash:
            snapshot = ExecutionSnapshot(
                graph_run_id=thread_id,
                graph_id=f"{req.item_type}/{req.item_id}",
                project_manifest_hash=project_manifest_hash,
                user_manifest_hash=user_manifest_hash,
                system_version=get_system_version(),
                step=1,
                status=result.get("status", "unknown"),
            )
            snapshot_hash = cas.store_object(snapshot.to_dict(), root)
            new_hashes.append(snapshot_hash)

        bundle_hash, output_hashes = _ingest_runtime_outputs(
            paths.project_path, root, thread_id, snapshot_hash,
        )
        new_hashes.extend(output_hashes)

        _check_user_quota(user, settings)

        exec_status = result.get("status", "unknown")
        _complete_thread(
            settings, thread_id,
            state="completed" if exec_status == "success" else "error",
            snapshot_hash=snapshot_hash,
            runtime_outputs_bundle_hash=bundle_hash or None,
        )

        return {
            "status": exec_status,
            "thread_id": thread_id,
            "execution_snapshot_hash": snapshot_hash,
            "runtime_outputs_bundle_hash": bundle_hash or None,
            "new_object_hashes": new_hashes,
            "result": result,
            "system_version": get_system_version(),
        }
    except FileNotFoundError as e:
        _complete_thread(settings, thread_id, state="error")
        raise HTTPException(status.HTTP_404_NOT_FOUND, str(e))
    except Exception:
        _complete_thread(settings, thread_id, state="error")
        raise
    finally:
        for key, old_value in injected_keys:
            if old_value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = old_value
        if paths:
            cleanup(paths)


# --- Checkout-based execution (Step D) ---

MAX_FOLD_BACK_RETRIES = 5
FOLD_BACK_BASE_JITTER_MS = 50


async def _execute_with_checkout(
    req: ExecuteRequest,
    user: User,
    settings: Settings,
):
    """Checkout-based execution — isolated mutable copy from snapshot cache."""
    ref = _resolve_project_ref(settings, user, req.project_name)
    base_snapshot_hash = ref["snapshot_hash"]
    if not base_snapshot_hash:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            f"Project '{req.project_name}' has no snapshot. Re-push to create one.",
        )
    if req.system_version:
        _check_system_version(req.system_version)
    else:
        _check_system_version(ref["system_version"])

    root = _user_cas_root(user, settings)
    cache = settings.cache_root(user.id)
    exec_root = settings.exec_root(user.id)
    thread_id = f"rye-remote-{uuid.uuid4().hex[:12]}"
    exec_space: Path | None = None
    injected_keys: list[tuple[str, str | None]] = []

    _register_thread(
        settings, user, thread_id,
        item_type=req.item_type,
        item_id=req.item_id,
        project_manifest_hash=ref["project_manifest_hash"],
        user_manifest_hash=ref["user_manifest_hash"],
        project_name=req.project_name,
    )

    try:
        # Checkout mutable copy from snapshot cache
        exec_space = create_execution_space(
            base_snapshot_hash, thread_id, root, cache, exec_root,
        )
        user_space = ensure_user_space_cached(
            ref["user_manifest_hash"], root, cache,
        )

        os.environ["RYE_SIGNING_KEY_DIR"] = settings.signing_key_dir
        injected_keys = _inject_user_secrets(user, settings)

        tool = ExecuteTool(
            user_space=str(user_space),
            project_path=str(exec_space),
        )

        result = await tool.handle(
            item_type=req.item_type,
            item_id=req.item_id,
            project_path=str(exec_space),
            parameters=req.parameters,
            thread=req.thread,
        )

        # Promote execution-local CAS into user CAS
        exec_cas = exec_space / AI_DIR / "objects"
        new_hashes = _copy_cas_objects(exec_cas, root)

        # Ingest runtime outputs (transcripts, knowledge, refs) into CAS
        exec_snapshot_hash = _find_execution_snapshot_hash(exec_space)
        if not exec_snapshot_hash:
            es = ExecutionSnapshot(
                graph_run_id=thread_id,
                graph_id=f"{req.item_type}/{req.item_id}",
                project_manifest_hash=ref["project_manifest_hash"],
                user_manifest_hash=ref["user_manifest_hash"],
                system_version=get_system_version(),
                step=1,
                status=result.get("status", "unknown"),
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
            user_manifest_hash=ref["user_manifest_hash"],
            parent_hashes=[base_snapshot_hash],
            source="execution",
            source_detail=f"{req.item_type}/{req.item_id}",
            timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
            metadata={"thread_id": thread_id},
        )
        proj_snapshot_hash = cas.store_object(proj_snapshot.to_dict(), root)
        new_hashes.append(proj_snapshot_hash)

        # Fold back into HEAD
        fold_result = await _fold_back(
            user, settings, req.project_name,
            base_snapshot_hash, proj_snapshot_hash,
            root, cache, thread_id,
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
    project_name: str,
    base_snapshot_hash: str,
    exec_snapshot_hash: str,
    cas_root: Path,
    cache_root: Path,
    thread_id: str,
) -> dict:
    """Merge execution snapshot into HEAD.

    Fast-forward if HEAD hasn't moved, otherwise three-way merge.
    Bounded retry loop with jitter for contention.
    """
    current_head = base_snapshot_hash  # fallback for retry_exhausted

    for attempt in range(MAX_FOLD_BACK_RETRIES):
        ref = _resolve_project_ref(settings, user, project_name)
        current_head = ref["snapshot_hash"]
        current_rev = ref["snapshot_revision"]

        if current_head == base_snapshot_hash:
            # Fast-forward — HEAD hasn't moved
            if _try_advance_head(
                settings, user, project_name,
                exec_snapshot_hash, current_rev,
            ):
                _update_snapshot_cache(
                    settings, user, project_name,
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
                    settings, user, project_name,
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
                user_manifest_hash=ref["user_manifest_hash"],
                parent_hashes=[current_head, exec_snapshot_hash],
                source="merge",
                timestamp=datetime.datetime.now(datetime.timezone.utc).isoformat(),
                metadata={"base": base_snapshot_hash, "thread_id": thread_id},
            )
            merge_snapshot_hash = cas.store_object(
                merge_snapshot.to_dict(), cas_root,
            )

            if _try_advance_head(
                settings, user, project_name,
                merge_snapshot_hash, current_rev,
            ):
                _update_snapshot_cache(
                    settings, user, project_name,
                    merge_snapshot_hash, cas_root, cache_root,
                )
                return {"snapshot_hash": merge_snapshot_hash, "merge_type": "merge"}

        # CAS update raced — back off with jitter and retry
        jitter = FOLD_BACK_BASE_JITTER_MS * (2 ** attempt) + random.randint(0, 50)
        await asyncio.sleep(jitter / 1000)

    # Exhausted retries
    return {
        "snapshot_hash": current_head,
        "merge_type": "retry_exhausted",
        "unmerged_snapshot": exec_snapshot_hash,
    }


def _try_advance_head(
    settings: Settings,
    user: User,
    project_name: str,
    new_snapshot_hash: str,
    expected_rev: int,
) -> bool:
    """Optimistic CAS on snapshot_revision. Returns True if update succeeded."""
    sb = _get_supabase(settings)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    result = (
        sb.table("project_refs")
        .update({
            "snapshot_hash": new_snapshot_hash,
            "snapshot_revision": expected_rev + 1,
            "head_updated_at": now,
        })
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .eq("project_name", project_name)
        .eq("snapshot_revision", expected_rev)
        .execute()
    )
    return bool(result.data)


def _update_snapshot_cache(
    settings: Settings,
    user: User,
    project_name: str,
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
            new_snapshot_hash[:16], project_name, exc_info=True,
        )


def _store_conflict_record(
    settings: Settings,
    user: User,
    project_name: str,
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
    project_name: Optional[str] = None,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """List user's remote executions on this remote."""
    sb = _get_supabase(settings)
    query = (
        sb.table("threads")
        .select("*")
        .eq("user_id", user.id)
        .eq("remote_name", settings.rye_remote_name)
        .order("created_at", desc=True)
        .limit(limit)
    )
    if project_name:
        query = query.eq("project_name", project_name)
    result = query.execute()
    return {"threads": result.data or [], "remote_name": settings.rye_remote_name}


@app.get("/threads/{thread_id}")
async def get_thread(
    thread_id: str,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Get status of a specific thread."""
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


# --- Secrets management ---


@app.post("/secrets")
async def upsert_secrets(
    req: SecretsUpsertRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    """Upsert user secrets into the vault for remote execution."""
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
    sb = _get_supabase(settings)
    result = sb.rpc(
        "delete_user_secret",
        {"p_user_id": user.id, "p_name": name},
    ).execute()
    deleted = result.data if result.data else False
    if not deleted:
        raise HTTPException(status.HTTP_404_NOT_FOUND, f"Secret '{name}' not found")
    return {"deleted": name}
