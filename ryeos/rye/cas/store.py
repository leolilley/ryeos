"""Rye-level CAS operations.

Knows about item types, spaces, manifests. Built on lillux CAS primitives.
"""

from __future__ import annotations

import hashlib
import json
import logging
import os
import tempfile
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from rye.primitives import cas
from rye.primitives.integrity import compute_integrity

from rye.cas.objects import ItemRef, ItemSource
from rye.constants import AI_DIR, ItemType, STATE_DIR, STATE_OBJECTS
from rye.utils.metadata_manager import MetadataManager, compute_content_hash
from rye.utils.path_utils import get_user_space

logger = logging.getLogger(__name__)


def cas_root(project_path: Path) -> Path:
    """Returns {project}/.ai/state/objects/"""
    return project_path / AI_DIR / STATE_DIR / STATE_OBJECTS


def user_cas_root() -> Path:
    """Returns {USER_SPACE}/.ai/state/objects/"""
    return get_user_space() / AI_DIR / STATE_DIR / STATE_OBJECTS


def ingest_item(
    item_type: str,
    file_path: Path,
    project_path: Path,
) -> ItemRef:
    """Read file → store as blob + create item_source object → return ItemRef.

    Works for both signed and unsigned .ai/ files.
    """
    root = cas_root(project_path)
    raw_bytes = file_path.read_bytes()
    content_text = raw_bytes.decode("utf-8")

    # Store raw content as blob
    blob_hash = cas.store_blob(raw_bytes, root)

    # Compute integrity (SHA256 of raw bytes)
    integrity = hashlib.sha256(raw_bytes).hexdigest()

    # Extract signature info (None for unsigned files)
    signature_info: Optional[Dict[str, str]] = None
    try:
        signature_info = MetadataManager.get_signature_info(
            item_type,
            content_text,
            file_path=file_path,
            project_path=project_path,
        )
    except ValueError:
        pass  # unsigned file — expected for PEM keys, etc.

    # Derive item_id from path relative to .ai/{type_dir}/
    type_dir_name = ItemType.TYPE_DIRS.get(item_type, item_type)
    item_id = file_path.stem  # fallback
    # Try to find the .ai/{type_dir}/ ancestor in the path
    parts = file_path.parts
    for i, part in enumerate(parts):
        if part == AI_DIR and i + 1 < len(parts) and parts[i + 1] == type_dir_name:
            # Everything after .ai/{type_dir}/ minus extension is the item_id
            relative = file_path.relative_to(Path(*parts[: i + 2]))
            item_id = relative.with_suffix("").as_posix()
            break

    # Build item_source object and store
    item_source = ItemSource(
        item_type=item_type,
        item_id=item_id,
        content_blob_hash=blob_hash,
        integrity=integrity,
        signature_info=signature_info,
    )
    object_hash = cas.store_object(item_source.to_dict(), root)

    return ItemRef(
        blob_hash=blob_hash,
        object_hash=object_hash,
        integrity=integrity,
        signature_info=signature_info,
    )


def ingest_directory(
    base_path: Path,
    project_path: Path,
) -> Dict[str, str]:
    """Walk .ai/ tree → ingest all items → return {relative_path: object_hash}.

    Skips .ai/state/ (runtime state).
    """
    ai_path = base_path / AI_DIR
    if not ai_path.is_dir():
        return {}

    skip_dirs = {STATE_DIR}
    results: Dict[str, str] = {}

    for dirpath, dirnames, filenames in os.walk(ai_path):
        rel_dir = Path(dirpath).relative_to(base_path)

        # Skip runtime directories
        dirnames[:] = [
            d for d in dirnames if not (rel_dir == Path(AI_DIR) and d in skip_dirs)
        ]

        for filename in filenames:
            file_path = Path(dirpath) / filename
            rel_path = str(file_path.relative_to(base_path))

            # Determine item type from path
            item_type = item_type_from_path(rel_path)
            if item_type is None:
                logger.debug("Skipping unrecognised path %s", rel_path)
                continue

            try:
                ref = ingest_item(item_type, file_path, project_path)
                results[rel_path] = ref.object_hash
            except Exception:
                logger.warning("Failed to ingest %s", rel_path, exc_info=True)

    return results


def materialize_item(
    object_hash: str,
    target_path: Path,
    root: Path,
) -> Path:
    """Read item_source object → extract blob → write to target_path."""
    obj = cas.get_object(object_hash, root)
    if obj is None:
        raise FileNotFoundError(
            f"Object {object_hash} not found in CAS (root={root}, target={target_path})"
        )

    blob_hash = obj["content_blob_hash"]
    blob_data = cas.get_blob(blob_hash, root)
    if blob_data is None:
        # Enhanced diagnostics: check if blob exists on disk but lillux can't read it
        blob_path = root / "blobs" / blob_hash[:2] / blob_hash[2:4] / blob_hash
        exists_on_disk = blob_path.exists()
        logger.error(
            "Blob %s not found via lillux. root=%s, exists_on_disk=%s, blob_path=%s, "
            "item_type=%s, item_id=%s, object_hash=%s",
            blob_hash,
            root,
            exists_on_disk,
            blob_path,
            obj.get("item_type"),
            obj.get("item_id"),
            object_hash,
        )
        if exists_on_disk:
            try:
                disk_size = blob_path.stat().st_size
                logger.error("Blob file exists on disk: size=%d bytes", disk_size)
            except Exception as e:
                logger.error("Could not stat blob file: %s", e)
        raise FileNotFoundError(
            f"Blob {blob_hash} not found in CAS "
            f"(root={root}, exists_on_disk={exists_on_disk}, "
            f"item_type={obj.get('item_type')}, item_id={obj.get('item_id')})"
        )

    target_path.parent.mkdir(parents=True, exist_ok=True)

    # Atomic write
    fd, tmp_path = tempfile.mkstemp(dir=target_path.parent)
    closed = False
    try:
        os.write(fd, blob_data)
        os.close(fd)
        closed = True
        os.rename(tmp_path, target_path)
    except BaseException:
        if not closed:
            os.close(fd)
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise

    return target_path


# --- Ref operations ---


def write_ref(ref_path: Path, hash_hex: str) -> None:
    """Atomically write a mutable ref pointer."""
    ref_path.parent.mkdir(parents=True, exist_ok=True)
    fd, tmp_path = tempfile.mkstemp(dir=ref_path.parent)
    closed = False
    try:
        os.write(fd, json.dumps({"hash": hash_hex}).encode("utf-8"))
        os.close(fd)
        closed = True
        os.rename(tmp_path, ref_path)
    except BaseException:
        if not closed:
            os.close(fd)
        try:
            os.unlink(tmp_path)
        except OSError:
            pass
        raise


def read_ref(ref_path: Path) -> Optional[str]:
    """Read a ref pointer. Returns hash or None."""
    if not ref_path.exists():
        return None
    data = json.loads(ref_path.read_bytes())
    return data.get("hash")


# --- Helpers ---


def item_type_from_path(rel_path: str) -> Optional[str]:
    """Derive item type from a .ai/-relative path. Returns None if unrecognised."""
    parts = rel_path.split("/")
    if len(parts) >= 2 and parts[0] == AI_DIR:
        type_dir = parts[1]
        for item_type, dir_name in ItemType.SIGNABLE_DIRS.items():
            if type_dir == dir_name:
                return item_type
    return None
