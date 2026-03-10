"""Sync protocol — hash set reconciliation over CAS.

Three operations: has_objects, put_objects, get_objects.
Used by both client (push/pull) and server (endpoint handlers).
"""

from __future__ import annotations

import base64
import hashlib
import json
import logging
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List

from lillux.primitives import cas
from lillux.primitives.integrity import canonical_json, compute_integrity

logger = logging.getLogger(__name__)


# --- Protocol types ---


@dataclass
class ObjectEntry:
    """A CAS object in transit."""

    hash: str
    kind: str  # "blob" or "object"
    data: str  # base64-encoded bytes

    def to_dict(self) -> Dict[str, str]:
        return {"hash": self.hash, "kind": self.kind, "data": self.data}

    @classmethod
    def from_dict(cls, d: Dict[str, str]) -> ObjectEntry:
        return cls(hash=d["hash"], kind=d["kind"], data=d["data"])


# --- Server-side handlers ---


def handle_has_objects(hashes: List[str], root: Path) -> Dict[str, List[str]]:
    """Check which hashes exist in CAS. Returns {present, missing}."""
    results = cas.has_many(hashes, root)
    present = [h for h, exists in results.items() if exists]
    missing = [h for h, exists in results.items() if not exists]
    return {"present": present, "missing": missing}


def handle_put_objects(
    entries: List[Dict[str, str]], root: Path
) -> Dict[str, Any]:
    """Store objects in CAS. Verifies hash on each entry. Returns {stored, errors}."""
    stored: List[str] = []
    errors: List[Dict[str, str]] = []

    for entry in entries:
        obj = ObjectEntry.from_dict(entry)
        raw = base64.b64decode(obj.data)

        # Verify claimed hash
        if obj.kind == "blob":
            actual = hashlib.sha256(raw).hexdigest()
        else:
            actual = compute_integrity(json.loads(raw.decode("utf-8")))

        if actual != obj.hash:
            errors.append({
                "hash": obj.hash,
                "error": f"hash mismatch: claimed {obj.hash[:16]}… got {actual[:16]}…",
            })
            continue

        # Store
        if obj.kind == "blob":
            cas.store_blob(raw, root)
        else:
            cas.store_object(json.loads(raw.decode("utf-8")), root)
        stored.append(obj.hash)

    result: Dict[str, Any] = {"stored": stored}
    if errors:
        result["errors"] = errors
    return result


def handle_get_objects(
    hashes: List[str], root: Path
) -> Dict[str, Any]:
    """Retrieve objects from CAS. Returns {entries}."""
    entries: List[Dict[str, str]] = []

    for h in hashes:
        blob = cas.get_blob(h, root)
        if blob is not None:
            entries.append(ObjectEntry(
                hash=h, kind="blob",
                data=base64.b64encode(blob).decode("ascii"),
            ).to_dict())
            continue

        obj = cas.get_object(h, root)
        if obj is not None:
            raw = canonical_json(obj).encode("utf-8")
            entries.append(ObjectEntry(
                hash=h, kind="object",
                data=base64.b64encode(raw).decode("ascii"),
            ).to_dict())
            continue

    missing = [h for h in hashes if not any(e["hash"] == h for e in entries)]
    if missing:
        logger.warning("CAS get_objects: %d hashes not found: %s", len(missing), [h[:16] for h in missing])

    return {"entries": entries}


# --- Client-side helpers ---


def collect_object_hashes(manifest: dict, root: Path) -> List[str]:
    """Collect transitive object hashes from a manifest.

    Walks items (item_source → content_blob_hash) and files (blob hashes).
    Returns deduplicated list including the manifest's own objects.
    """
    seen: set[str] = set()

    # item_source object hashes + their content blobs
    for item_source_hash in manifest.get("items", {}).values():
        seen.add(item_source_hash)
        obj = cas.get_object(item_source_hash, root)
        if obj and "content_blob_hash" in obj:
            seen.add(obj["content_blob_hash"])

    # raw file blob hashes
    for blob_hash in manifest.get("files", {}).values():
        seen.add(blob_hash)

    return list(seen)


def export_objects(
    hashes: List[str], root: Path
) -> List[Dict[str, str]]:
    """Export CAS objects as base64-encoded entries for put_objects."""
    result = handle_get_objects(hashes, root)
    return result["entries"]


def import_objects(
    entries: List[Dict[str, str]], root: Path
) -> List[str]:
    """Import base64-encoded entries into local CAS. Raises on integrity errors."""
    result = handle_put_objects(entries, root)
    if result.get("errors"):
        failed = [e["hash"][:16] for e in result["errors"]]
        raise ValueError(f"CAS import failed for {len(failed)} objects: {failed}")
    return result["stored"]
