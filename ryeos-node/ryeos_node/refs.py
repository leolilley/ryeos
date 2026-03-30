"""Local filesystem project and user-space ref management.

Replaces Supabase project_refs and user_space_refs tables with
atomic ref files under each user's CAS root.
"""

from __future__ import annotations

import datetime
import hashlib
import json
import logging
import os
from pathlib import Path

from fastapi import HTTPException, status

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------

def _atomic_write(path: Path, data: bytes) -> None:
    """Write to temp file, os.replace to target, fsync parent."""
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(".tmp")
    tmp.write_bytes(data)
    os.replace(tmp, path)
    fd = os.open(path.parent, os.O_RDONLY)
    try:
        os.fsync(fd)
    finally:
        os.close(fd)


def _project_path_hash(project_path: str) -> str:
    return hashlib.sha256(project_path.encode()).hexdigest()


def _project_ref_dir(cas_base: str, user_fp: str, project_path: str) -> Path:
    return Path(cas_base) / user_fp / "refs" / "projects" / _project_path_hash(project_path)


def _user_space_ref_dir(cas_base: str, user_fp: str) -> Path:
    return Path(cas_base) / user_fp / "refs" / "user-space"


# ---------------------------------------------------------------------------
# Project refs
# ---------------------------------------------------------------------------

def resolve_project_ref(
    cas_base: str, user_fp: str, project_path: str
) -> dict | None:
    ref_dir = _project_ref_dir(cas_base, user_fp, project_path)
    head_file = ref_dir / "head"
    meta_file = ref_dir / "meta.json"

    if not head_file.exists():
        return None

    snapshot_hash = head_file.read_text().strip()
    project_path_value = project_path
    if meta_file.exists():
        meta = json.loads(meta_file.read_text())
        project_path_value = meta.get("project_path", project_path)

    return {"snapshot_hash": snapshot_hash, "project_path": project_path_value}


def init_project_ref(
    cas_base: str, user_fp: str, project_path: str, snapshot_hash: str
) -> None:
    ref_dir = _project_ref_dir(cas_base, user_fp, project_path)
    head_file = ref_dir / "head"

    if head_file.exists():
        raise HTTPException(
            status_code=status.HTTP_409_CONFLICT,
            detail="Project ref already exists",
        )

    now = datetime.datetime.now(datetime.timezone.utc).isoformat()
    meta = {"project_path": project_path, "created_at": now}

    _atomic_write(head_file, snapshot_hash.encode())
    _atomic_write(ref_dir / "meta.json", json.dumps(meta).encode())


def advance_project_ref(
    cas_base: str,
    user_fp: str,
    project_path: str,
    new_snapshot_hash: str,
    expected_snapshot_hash: str | None,
) -> bool:
    ref_dir = _project_ref_dir(cas_base, user_fp, project_path)
    head_file = ref_dir / "head"

    current: str | None = None
    if head_file.exists():
        current = head_file.read_text().strip()

    if expected_snapshot_hash is None:
        if current is not None:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail="Project ref already exists; expected_snapshot_hash required",
            )
        # init case
        init_project_ref(cas_base, user_fp, project_path, new_snapshot_hash)
        return True

    if current != expected_snapshot_hash:
        return False

    _atomic_write(head_file, new_snapshot_hash.encode())
    return True


# ---------------------------------------------------------------------------
# User-space refs
# ---------------------------------------------------------------------------

def resolve_user_space_ref(cas_base: str, user_fp: str) -> dict | None:
    ref_dir = _user_space_ref_dir(cas_base, user_fp)
    head_file = ref_dir / "head"
    meta_file = ref_dir / "meta.json"

    if not head_file.exists():
        return None

    user_manifest_hash = head_file.read_text().strip()

    revision = 1
    pushed_at: str | None = None
    if meta_file.exists():
        meta = json.loads(meta_file.read_text())
        revision = meta.get("revision", 1)
        pushed_at = meta.get("pushed_at")

    return {
        "user_manifest_hash": user_manifest_hash,
        "revision": revision,
        "pushed_at": pushed_at,
    }


def advance_user_space_ref(
    cas_base: str,
    user_fp: str,
    new_manifest_hash: str,
    expected_revision: int | None,
) -> dict:
    ref_dir = _user_space_ref_dir(cas_base, user_fp)
    head_file = ref_dir / "head"
    meta_file = ref_dir / "meta.json"

    current = resolve_user_space_ref(cas_base, user_fp)
    now = datetime.datetime.now(datetime.timezone.utc).isoformat()

    if expected_revision is None:
        if current is not None:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail="User space ref already exists; expected_revision required",
            )
        # init
        new_revision = 1
    else:
        if current is None:
            raise HTTPException(
                status_code=status.HTTP_404_NOT_FOUND,
                detail="User space ref not found",
            )
        if current["revision"] != expected_revision:
            raise HTTPException(
                status_code=status.HTTP_409_CONFLICT,
                detail=f"Revision mismatch: expected {expected_revision}, current {current['revision']}",
            )
        new_revision = expected_revision + 1

    meta = {"revision": new_revision, "pushed_at": now}

    _atomic_write(head_file, new_manifest_hash.encode())
    _atomic_write(meta_file, json.dumps(meta).encode())

    return {
        "user_manifest_hash": new_manifest_hash,
        "revision": new_revision,
        "pushed_at": now,
    }
