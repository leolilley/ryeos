"""Content-Addressed Store (CAS) primitives.

Type-agnostic, kernel-level storage. Same layer as integrity.py and signing.py.

Storage layout under a given root:
    blobs/ab/cd/<sha256>          — raw bytes
    objects/ab/cd/<sha256>.json   — canonical JSON

All writes are atomic and idempotent — delegated to the lillux Rust binary.
"""

import json
import shutil
import subprocess
import sys
from pathlib import Path
from typing import Dict, List, Optional

_cached_binary: Optional[str] = None


def _lillux() -> str:
    """Find the lillux binary. Cached after first lookup."""
    global _cached_binary
    if _cached_binary is not None:
        return _cached_binary
    candidate = Path(sys.executable).parent / "lillux"
    if candidate.is_file():
        _cached_binary = str(candidate)
        return _cached_binary
    found = shutil.which("lillux")
    if found:
        _cached_binary = found
        return _cached_binary
    raise FileNotFoundError("lillux binary not found")


def _shard_path(root: Path, namespace: str, hash_hex: str, ext: str = "") -> Path:
    """Build 2-level sharded path: root/namespace/ab/cd/abcdef...{ext}"""
    return root / namespace / hash_hex[:2] / hash_hex[2:4] / f"{hash_hex}{ext}"


def store_blob(data: bytes, root: Path) -> str:
    """Store raw bytes, return sha256 hex digest. Skip if exists."""
    result = subprocess.run(
        [_lillux(), "cas", "store", "--root", str(root), "--blob"],
        input=data,
        capture_output=True,
    )
    result.check_returncode()
    return json.loads(result.stdout)["hash"]


def store_object(data: dict, root: Path) -> str:
    """Store a dict as canonical JSON, return its integrity hash. Skip if exists."""
    result = subprocess.run(
        [_lillux(), "cas", "store", "--root", str(root)],
        input=json.dumps(data).encode("utf-8"),
        capture_output=True,
    )
    result.check_returncode()
    return json.loads(result.stdout)["hash"]


def get_blob(hash_hex: str, root: Path) -> Optional[bytes]:
    """Read blob by hash. Returns None if not found."""
    result = subprocess.run(
        [_lillux(), "cas", "fetch", "--root", str(root), "--hash", hash_hex, "--blob"],
        capture_output=True,
    )
    if result.returncode != 0 or not result.stdout:
        return None
    return result.stdout


def get_object(hash_hex: str, root: Path) -> Optional[dict]:
    """Read object by hash. Returns None if not found."""
    result = subprocess.run(
        [_lillux(), "cas", "fetch", "--root", str(root), "--hash", hash_hex],
        capture_output=True,
    )
    if result.returncode != 0 or not result.stdout:
        return None
    try:
        return json.loads(result.stdout)
    except (json.JSONDecodeError, ValueError):
        return None


def has_blob(hash_hex: str, root: Path) -> bool:
    """Check if a hash exists as a blob (not an object)."""
    return _shard_path(root, "blobs", hash_hex).exists()


def has_object(hash_hex: str, root: Path) -> bool:
    """Check if a hash exists as an object (not a blob)."""
    return _shard_path(root, "objects", hash_hex, ext=".json").exists()


def has(hash_hex: str, root: Path) -> bool:
    """Check if a hash exists as either a blob or an object."""
    return has_blob(hash_hex, root) or has_object(hash_hex, root)


def has_many(hashes: List[str], root: Path) -> Dict[str, bool]:
    """Batch existence check."""
    return {h: has(h, root) for h in hashes}
