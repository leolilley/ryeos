"""Content-Addressed Store (CAS) primitives.

Type-agnostic, kernel-level storage. Same layer as integrity.py and signing.py.

Storage layout under a given root:
    blobs/ab/cd/<sha256>          — raw bytes
    objects/ab/cd/<sha256>.json   — canonical JSON

All writes are atomic (tmp + rename) and idempotent (skip if exists).
"""

import hashlib
import json
import os
import tempfile
from pathlib import Path
from typing import Dict, List, Optional

from lillux.primitives.integrity import canonical_json, compute_integrity


def _shard_path(root: Path, namespace: str, hash_hex: str, ext: str = "") -> Path:
    """Build 2-level sharded path: root/namespace/ab/cd/abcdef...{ext}"""
    return root / namespace / hash_hex[:2] / hash_hex[2:4] / f"{hash_hex}{ext}"


def _atomic_write_bytes(target: Path, data: bytes) -> None:
    """Write bytes atomically via tmp file + rename."""
    target.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(dir=target.parent)
    closed = False
    try:
        os.write(fd, data)
        os.close(fd)
        closed = True
        os.rename(tmp_path, target)
    except BaseException:
        if not closed:
            os.close(fd)
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def store_blob(data: bytes, root: Path) -> str:
    """Store raw bytes, return sha256 hex digest. Skip if exists."""
    hash_hex = hashlib.sha256(data).hexdigest()
    target = _shard_path(root, "blobs", hash_hex)
    if target.exists():
        return hash_hex
    _atomic_write_bytes(target, data)
    return hash_hex


def store_object(data: dict, root: Path) -> str:
    """Store a dict as canonical JSON, return its integrity hash. Skip if exists."""
    hash_hex = compute_integrity(data)
    target = _shard_path(root, "objects", hash_hex, ext=".json")
    if target.exists():
        return hash_hex
    encoded = canonical_json(data).encode("utf-8")
    _atomic_write_bytes(target, encoded)
    return hash_hex


def get_blob(hash_hex: str, root: Path) -> Optional[bytes]:
    """Read blob by hash. Returns None if not found."""
    target = _shard_path(root, "blobs", hash_hex)
    if target.exists():
        return target.read_bytes()
    return None


def get_object(hash_hex: str, root: Path) -> Optional[dict]:
    """Read object by hash. Returns None if not found."""
    target = _shard_path(root, "objects", hash_hex, ext=".json")
    if target.exists():
        return json.loads(target.read_bytes())
    return None


def has(hash_hex: str, root: Path) -> bool:
    """Check if a hash exists as either a blob or an object."""
    return (
        _shard_path(root, "blobs", hash_hex).exists()
        or _shard_path(root, "objects", hash_hex, ext=".json").exists()
    )


def has_many(hashes: List[str], root: Path) -> Dict[str, bool]:
    """Batch existence check."""
    return {h: has(h, root) for h in hashes}
