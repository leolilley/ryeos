"""CAS-native remote execution server.

Endpoints: /health, /public-key, /objects/has, /objects/put, /objects/get,
           /push, /execute, /threads, /threads/{thread_id}
"""

import datetime
import hashlib
import json
import logging
import os
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

from rye.cas.objects import ExecutionSnapshot
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
from rye.tools.execute import ExecuteTool

logger = logging.getLogger(__name__)

app = FastAPI(title="ryeos-remote", version="0.1.0")

# m3: Gzip compression for responses
app.add_middleware(GZipMiddleware, minimum_size=1000)


# m1: Enforce batch size limits (reads actual body, not just Content-Length header)
@app.middleware("http")
async def enforce_request_size(request: Request, call_next):
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


class ExecuteRequest(BaseModel):
    project_manifest_hash: Optional[str] = None
    user_manifest_hash: Optional[str] = None
    project_name: Optional[str] = None
    system_version: Optional[str] = None
    item_type: str
    item_id: str
    parameters: Dict[str, Any] = {}


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
            os.environ[name] = row["decrypted_secret"]
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
) -> None:
    """Update thread state to completed/error."""
    try:
        sb = _get_supabase(settings)
        update = {"state": state, "completed_at": datetime.datetime.now(datetime.timezone.utc).isoformat()}
        if snapshot_hash:
            update["snapshot_hash"] = snapshot_hash
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
) -> None:
    """Upsert project_refs row (last-writer-wins)."""
    try:
        sb = _get_supabase(settings)
        sb.table("project_refs").upsert({
            "user_id": user.id,
            "remote_name": settings.rye_remote_name,
            "project_name": project_name,
            "project_manifest_hash": project_manifest_hash,
            "user_manifest_hash": user_manifest_hash,
            "system_version": system_version,
            "pushed_at": datetime.datetime.now(datetime.timezone.utc).isoformat(),
        }).execute()
    except Exception:
        logger.warning("Failed to upsert project ref %s", project_name, exc_info=True)


def _resolve_project_ref(
    settings: Settings,
    user: User,
    project_name: str,
) -> Dict[str, str]:
    """Look up project_refs to resolve manifest hashes.

    Returns dict with project_manifest_hash, user_manifest_hash, system_version.
    Raises HTTPException if not found.
    """
    sb = _get_supabase(settings)
    result = (
        sb.table("project_refs")
        .select("project_manifest_hash, user_manifest_hash, system_version")
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
    refs_dir = project_path / ".ai" / "objects" / "refs" / "graphs"
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
    """Finalize a push — verify manifest objects exist, upsert project ref."""
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

    _upsert_project_ref(
        settings, user,
        project_name=req.project_name,
        project_manifest_hash=req.project_manifest_hash,
        user_manifest_hash=req.user_manifest_hash,
        system_version=req.system_version,
    )

    return {
        "status": "ok",
        "remote_name": settings.rye_remote_name,
        "project_name": req.project_name,
        "project_manifest_hash": req.project_manifest_hash,
        "user_manifest_hash": req.user_manifest_hash,
    }


@app.post("/execute")
async def execute(
    req: ExecuteRequest,
    user: User = Depends(get_current_user),
    settings: Settings = Depends(get_settings),
):
    # Resolve manifest hashes: either explicit or via project_name
    if req.project_manifest_hash and req.user_manifest_hash:
        project_manifest_hash = req.project_manifest_hash
        user_manifest_hash = req.user_manifest_hash
        project_name = req.project_name
        if req.system_version:
            _check_system_version(req.system_version)
    elif req.project_name:
        ref = _resolve_project_ref(settings, user, req.project_name)
        project_manifest_hash = ref["project_manifest_hash"]
        user_manifest_hash = ref["user_manifest_hash"]
        project_name = req.project_name
        _check_system_version(ref["system_version"])
    else:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "Provide either (project_manifest_hash + user_manifest_hash) or project_name",
        )

    root = _user_cas_root(user, settings)
    paths: ExecutionPaths | None = None
    thread_id = f"rye-remote-{uuid.uuid4().hex[:12]}"

    injected_keys: list[tuple[str, str | None]] = []

    # Register thread before execution
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

        # Set signing key dir for remote executor
        os.environ["RYE_SIGNING_KEY_DIR"] = settings.signing_key_dir

        # Inject user secrets into env for tool execution.
        # Safe under max_inputs=1 — no concurrent requests share this process.
        # Intentionally temporary: superseded by payload-based secrets
        # when the async Function.spawn() worker lands (Phase 1, Step 3).
        injected_keys = _inject_user_secrets(user, settings)

        # Wire ExecuteTool against materialized paths
        tool = ExecuteTool(
            user_space=str(paths.user_space),
            project_path=str(paths.project_path),
        )

        result = await tool.handle(
            item_type=req.item_type,
            item_id=req.item_id,
            project_path=str(paths.project_path),
            parameters=req.parameters,
        )

        # Ingest execution outputs into user CAS before cleanup
        project_cas = paths.project_path / ".ai" / "objects"
        new_hashes = _copy_cas_objects(project_cas, root)

        _check_user_quota(user, settings)

        # Use walker's real execution_snapshot if available
        snapshot_hash = _find_execution_snapshot_hash(paths.project_path)
        if not snapshot_hash:
            # Fallback: create synthetic snapshot for non-graph executions
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

        exec_status = result.get("status", "unknown")
        _complete_thread(
            settings, thread_id,
            state="completed" if exec_status == "success" else "error",
            snapshot_hash=snapshot_hash,
        )

        return {
            "status": exec_status,
            "thread_id": thread_id,
            "execution_snapshot_hash": snapshot_hash,
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
        # Restore previous env values or remove injected secrets
        for key, old_value in injected_keys:
            if old_value is None:
                os.environ.pop(key, None)
            else:
                os.environ[key] = old_value
        if paths:
            cleanup(paths)


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
