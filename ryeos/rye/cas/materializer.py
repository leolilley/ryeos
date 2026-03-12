"""Temp materializer — reconstitutes .ai/ filesystem from CAS.

Compatibility bridge so existing executor runs unmodified.
System space = installed ryeos package (unchanged, NOT materialized).
"""

from __future__ import annotations

import logging
import shutil
import tempfile
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

from lillux.primitives import cas

from rye.cas.store import materialize_item

logger = logging.getLogger(__name__)

_TMP_PREFIX = "rye-exec-"


@dataclass
class ExecutionPaths:
    """Paths produced by the materializer for a single execution."""

    project_path: Path  # /tmp/rye-exec-<id>/project/
    user_space: Path  # /tmp/rye-exec-<id>/user/
    cas_root: Path  # shared CAS (not copied)
    _base: Path = Path()  # temp root for cleanup


def materialize(
    project_manifest_hash: str,
    user_manifest_hash: str,
    cas_root_path: Path,
    tmp_base: Optional[Path] = None,
) -> ExecutionPaths:
    """Create temp dirs, write files from CAS, return paths.

    Reads source_manifest objects, iterates items (item_source → blob)
    and files (raw blob → write directly), preserving directory structure.
    """
    base = Path(tempfile.mkdtemp(prefix=_TMP_PREFIX, dir=tmp_base))
    project_dir = base / "project"
    user_dir = base / "user"
    project_dir.mkdir()
    user_dir.mkdir()

    try:
        _materialize_manifest(
            project_manifest_hash, project_dir, cas_root_path,
        )
        _materialize_manifest(
            user_manifest_hash, user_dir, cas_root_path,
        )
    except BaseException:
        shutil.rmtree(base, ignore_errors=True)
        raise

    return ExecutionPaths(
        project_path=project_dir,
        user_space=user_dir,
        cas_root=cas_root_path,
        _base=base,
    )


def cleanup(paths: ExecutionPaths) -> None:
    """Remove temp dirs created by materialize()."""
    if paths._base and paths._base.exists():
        shutil.rmtree(paths._base, ignore_errors=True)


def _safe_target(root: Path, rel_path: str) -> Path:
    """Validate and resolve a relative path against a root. Rejects escapes."""
    rel = Path(rel_path)
    if rel.is_absolute():
        raise ValueError(f"Absolute path in manifest: {rel_path}")
    target = (root / rel).resolve()
    if not target.is_relative_to(root.resolve()):
        raise ValueError(f"Path escapes target root: {rel_path}")
    return target


def materialize_manifest(
    manifest_hash: str,
    target_root: Path,
    cas_root_path: Path,
) -> None:
    """Materialize a single manifest (by hash) into target_root."""
    manifest = cas.get_object(manifest_hash, cas_root_path)
    if manifest is None:
        raise FileNotFoundError(
            f"Manifest object {manifest_hash} not found in CAS"
        )
    materialize_manifest_dict(manifest, target_root, cas_root_path)


def materialize_manifest_dict(
    manifest: dict,
    target_root: Path,
    cas_root_path: Path,
) -> None:
    """Materialize a manifest dict into target_root."""
    # items: .ai/ paths → item_source objects (unwrap blob via materialize_item)
    for rel_path, item_source_hash in manifest.get("items", {}).items():
        target_path = _safe_target(target_root, rel_path)
        materialize_item(item_source_hash, target_path, cas_root_path)

    # files: non-.ai/ paths → raw blobs (write directly)
    for rel_path, blob_hash in manifest.get("files", {}).items():
        target_path = _safe_target(target_root, rel_path)
        blob_data = cas.get_blob(blob_hash, cas_root_path)
        if blob_data is None:
            raise FileNotFoundError(
                f"Blob {blob_hash} for file {rel_path} not found in CAS"
            )
        target_path.parent.mkdir(parents=True, exist_ok=True)
        target_path.write_bytes(blob_data)


# Keep private alias for internal callers
_materialize_manifest = materialize_manifest


def get_system_version() -> str:
    """Get installed ryeos-engine version for system_version pinning."""
    try:
        from rye import __version__

        return __version__
    except Exception:
        return "unknown"
