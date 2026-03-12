"""Snapshot checkout — cache and mutable execution spaces.

Three-layer model:
  1. CAS (objects/) — append-only content store
  2. Snapshot cache (cache/snapshots/{hash}/) — read-only materialized snapshots
  3. Execution spaces (executions/{thread_id}/) — mutable checkouts

Each remote execution checks out a mutable copy from the snapshot cache.
After execution, changes are diffed against the base and folded back into HEAD.
"""

from __future__ import annotations

import logging
import shutil
import uuid
from pathlib import Path

from lillux.primitives import cas

from rye.cas.materializer import materialize_manifest, materialize_manifest_dict

logger = logging.getLogger(__name__)


def ensure_snapshot_cached(
    snapshot_hash: str,
    cas_root: Path,
    cache_root: Path,
) -> Path:
    """Ensure a materialized snapshot exists in the cache. Returns path.

    Materializes from CAS if not cached. Read-only after creation.
    Uses a staging directory + atomic rename for crash safety.
    """
    cached = cache_root / "snapshots" / snapshot_hash
    marker = cached / ".snapshot_complete"

    if cached.exists() and marker.exists():
        return cached

    # Materialize into staging dir with unique name, then atomic rename
    staging = cache_root / "snapshots" / f".staging-{snapshot_hash}-{uuid.uuid4().hex[:12]}"
    staging.mkdir(parents=True)

    snapshot = cas.get_object(snapshot_hash, cas_root)
    if snapshot is None:
        shutil.rmtree(staging, ignore_errors=True)
        raise FileNotFoundError(
            f"ProjectSnapshot {snapshot_hash} not found in CAS"
        )

    manifest_hash = snapshot["project_manifest_hash"]
    manifest = cas.get_object(manifest_hash, cas_root)
    if manifest is None:
        shutil.rmtree(staging, ignore_errors=True)
        raise FileNotFoundError(
            f"Manifest {manifest_hash} not found in CAS"
        )

    materialize_manifest_dict(manifest, staging, cas_root)

    # Mark complete, then try to rename into place
    (staging / ".snapshot_complete").write_text(snapshot_hash)
    try:
        staging.rename(cached)
    except OSError:
        # Another concurrent materialization beat us — clean up our staging
        shutil.rmtree(staging, ignore_errors=True)
        if cached.exists() and (cached / ".snapshot_complete").exists():
            return cached
        raise

    return cached


def create_execution_space(
    snapshot_hash: str,
    thread_id: str,
    cas_root: Path,
    cache_root: Path,
    exec_root: Path,
) -> Path:
    """Create a mutable execution space from a cached snapshot.

    Returns path to the execution space — a full, mutable project directory.
    """
    cached = ensure_snapshot_cached(snapshot_hash, cas_root, cache_root)
    exec_space = exec_root / thread_id
    shutil.copytree(cached, exec_space, dirs_exist_ok=True)
    return exec_space


def ensure_user_space_cached(
    user_manifest_hash: str,
    cas_root: Path,
    cache_root: Path,
) -> Path:
    """Ensure cached user space exists. Materialized once per hash."""
    cached = cache_root / "user" / user_manifest_hash
    marker = cached / ".user_space_complete"

    if cached.exists() and marker.exists():
        return cached

    # Materialize into staging dir with unique name
    staging = cache_root / "user" / f".staging-{user_manifest_hash}-{uuid.uuid4().hex[:12]}"
    staging.mkdir(parents=True)

    materialize_manifest(user_manifest_hash, staging, cas_root)

    (staging / ".user_space_complete").write_text(user_manifest_hash)
    try:
        staging.rename(cached)
    except OSError:
        # Another concurrent materialization beat us — clean up our staging
        shutil.rmtree(staging, ignore_errors=True)
        if cached.exists() and (cached / ".user_space_complete").exists():
            return cached
        raise

    return cached


def cleanup_execution_space(exec_space: Path) -> None:
    """Remove an execution space after use."""
    if exec_space.exists():
        shutil.rmtree(exec_space, ignore_errors=True)
