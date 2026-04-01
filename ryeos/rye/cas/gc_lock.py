"""Distributed GC lock using mkdir (atomic on POSIX).

mkdir() is the safest simple mutual exclusion primitive on shared filesystems.
It fails atomically with FileExistsError if the directory already exists,
preventing the race conditions that affect temp-file-rename approaches.

Filesystem requirement: Must support reliable atomic mkdir semantics.
True for ext4, XFS, ZFS, and most cloud-managed shared filesystems (EFS, Filestore).
NFS v3 with weak caching may not be reliable — use an external coordinator instead.

Lock layout:
    /cas/<fingerprint>/.gc-lock/
        owner.json    — lock metadata (node_id, gc_run_id, TTL, phase)
"""

from __future__ import annotations

import json
import logging
import shutil
import uuid
from dataclasses import asdict
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Optional

from rye.cas.gc_types import DistributedGCLock

logger = logging.getLogger(__name__)

LOCK_DIR_NAME = ".gc-lock"


def _lock_dir(user_root: Path) -> Path:
    return user_root / LOCK_DIR_NAME


def _owner_file(user_root: Path) -> Path:
    return _lock_dir(user_root) / "owner.json"


def _parse_ts(s: str) -> datetime:
    return datetime.fromisoformat(s)


def acquire(
    user_root: Path,
    node_id: str,
    ttl_seconds: int = 300,
) -> Optional[DistributedGCLock]:
    """Acquire GC lock via mkdir. Returns lock on success, None if held by another node."""
    lock_dir = _lock_dir(user_root)
    generation = 0

    try:
        lock_dir.mkdir(parents=True, exist_ok=False)
    except FileExistsError:
        # Lock exists — check if expired or recoverable
        owner_file = _owner_file(user_root)
        if owner_file.exists():
            try:
                existing = json.loads(owner_file.read_text(encoding="utf-8"))
                expires = _parse_ts(existing["expires_at"])
                if datetime.now(timezone.utc) < expires:
                    if existing.get("node_id") == node_id:
                        # Same node re-acquiring — extend TTL
                        return _write_lock(user_root, node_id, existing.get("generation", 0), ttl_seconds)
                    logger.debug("GC lock held by %s until %s", existing["node_id"], existing["expires_at"])
                    return None
                # Expired — reclaim
                generation = existing.get("generation", 0) + 1
                logger.info("Reclaiming expired GC lock from %s (gen %d)", existing["node_id"], generation - 1)
            except (json.JSONDecodeError, KeyError):
                pass

        # Remove stale/corrupt lock and retry mkdir
        shutil.rmtree(lock_dir, ignore_errors=True)
        try:
            lock_dir.mkdir(parents=True, exist_ok=False)
        except FileExistsError:
            logger.debug("Lost GC lock race after stale removal")
            return None

    return _write_lock(user_root, node_id, generation, ttl_seconds)


def _write_lock(user_root: Path, node_id: str, generation: int, ttl_seconds: int) -> DistributedGCLock:
    now = datetime.now(timezone.utc)
    lock = DistributedGCLock(
        gc_run_id=str(uuid.uuid4()),
        node_id=node_id,
        started_at=now.isoformat(),
        expires_at=(now + timedelta(seconds=ttl_seconds)).isoformat(),
        generation=generation,
        phase="init",
    )
    owner_file = _owner_file(user_root)
    owner_file.write_text(json.dumps(asdict(lock)), encoding="utf-8")
    logger.info("Acquired GC lock: run=%s node=%s gen=%d", lock.gc_run_id, node_id, generation)
    return lock


def release(user_root: Path, node_id: str) -> bool:
    """Release the lock. Only succeeds if we still hold it."""
    lock_dir = _lock_dir(user_root)
    if not lock_dir.exists():
        return True

    owner_file = _owner_file(user_root)
    if owner_file.exists():
        try:
            existing = json.loads(owner_file.read_text(encoding="utf-8"))
            if existing.get("node_id") != node_id:
                logger.warning("Cannot release GC lock — held by %s, not %s", existing["node_id"], node_id)
                return False
        except (json.JSONDecodeError, KeyError):
            pass

    shutil.rmtree(lock_dir, ignore_errors=True)
    logger.info("Released GC lock for node %s", node_id)
    return True


def update_phase(
    user_root: Path,
    node_id: str,
    phase: str,
    extend_ttl: int = 300,
) -> bool:
    """Update lock phase and extend TTL (heartbeat)."""
    owner_file = _owner_file(user_root)
    if not owner_file.exists():
        return False

    try:
        existing = json.loads(owner_file.read_text(encoding="utf-8"))
        if existing.get("node_id") != node_id:
            return False

        existing["phase"] = phase
        existing["expires_at"] = (datetime.now(timezone.utc) + timedelta(seconds=extend_ttl)).isoformat()
        owner_file.write_text(json.dumps(existing), encoding="utf-8")
        logger.debug("GC lock phase: %s (TTL extended %ds)", phase, extend_ttl)
        return True
    except (json.JSONDecodeError, KeyError):
        return False


def read_lock(user_root: Path) -> Optional[DistributedGCLock]:
    """Read current lock state without acquiring. Returns None if no lock."""
    owner_file = _owner_file(user_root)
    if not owner_file.exists():
        return None
    try:
        data = json.loads(owner_file.read_text(encoding="utf-8"))
        return DistributedGCLock(**data)
    except (json.JSONDecodeError, TypeError):
        return None


def is_locked(user_root: Path) -> bool:
    """Check if a valid (non-expired) lock exists."""
    lock = read_lock(user_root)
    if lock is None:
        return False
    try:
        expires = _parse_ts(lock.expires_at)
        return datetime.now(timezone.utc) < expires
    except (ValueError, TypeError):
        return False
