# rye:signed:2026-03-31T07:27:23Z:42c9e9a9ad1e0e6b243cd7e17231ce084cf97cc25faa896dbfa0006669354116:IlMEmKfN8k74s7_T-Lq_dRQIJeI2pKMBqCvobLLpliqdW_q39xfAJts7So1jbbHnVDKUJCjUdea7rwbjYKeICg:4b987fd4e40303ac
"""
persistence/artifact_store.py: CAS-backed artifact store

Stores full tool results as CAS blobs with content-hash deduplication.
Maintains an ArtifactIndex CAS object per thread mapping call_id → blob_hash.
"""

__version__ = "2.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "CAS-backed artifact store for out-of-band tool result persistence"

import hashlib
import json
import logging
from pathlib import Path
from typing import Any, Dict, Optional

from rye.primitives import cas
from rye.cas.objects import ArtifactIndex
from rye.cas.store import cas_root

logger = logging.getLogger(__name__)


class ArtifactStore:
    """CAS-backed store for out-of-band tool result persistence.

    Artifacts are stored as CAS blobs (content-addressed by SHA256).
    An ArtifactIndex object tracks call_id → {blob_hash, content_hash, tool_name}
    per thread, stored as a CAS object with a mutable ref pointer.
    """

    def __init__(self, thread_id: str, project_path: Path):
        self.thread_id = thread_id
        self.project_path = Path(project_path)
        self._root = cas_root(self.project_path)
        self._index: Optional[Dict[str, Dict[str, str]]] = None

    def _load_index(self) -> Dict[str, Dict[str, str]]:
        """Load artifact index from CAS via ref. Returns entries dict."""
        if self._index is not None:
            return self._index

        from rye.cas.store import read_ref
        ref_path = self._ref_path()
        index_hash = read_ref(ref_path)

        if not index_hash:
            self._index = {}
            return self._index

        obj = cas.get_object(index_hash, self._root)
        if obj is None:
            raise RuntimeError(f"Artifact index ref points to missing object: {index_hash}")
        if obj.get("kind") != "artifact_index":
            raise RuntimeError(f"Invalid artifact index kind: {obj.get('kind')}")
        if obj.get("thread_id") != self.thread_id:
            raise RuntimeError(
                f"Artifact index thread mismatch: expected {self.thread_id}, got {obj.get('thread_id')}"
            )

        self._index = obj.get("entries", {})
        return self._index

    def _save_index(self) -> None:
        """Store artifact index as CAS object and update ref."""
        from rye.cas.store import write_ref
        index = ArtifactIndex(
            thread_id=self.thread_id,
            entries=self._load_index(),
        )
        index_hash = cas.store_object(index.to_dict(), self._root)
        write_ref(self._ref_path(), index_hash)

    def _ref_path(self) -> Path:
        from rye.constants import AI_DIR
        return (
            self.project_path / AI_DIR / "objects" / "refs"
            / "artifacts" / f"{self.thread_id}.json"
        )

    def store(self, call_id: str, tool_name: str, data: Any) -> str:
        """Store artifact as CAS blob. Returns content hash.

        Serializes data deterministically, stores as blob, updates index.
        """
        serialized = json.dumps(data, sort_keys=True, default=str)
        content_hash = hashlib.sha256(serialized.encode()).hexdigest()

        blob_hash = cas.store_blob(serialized.encode(), self._root)

        entries = self._load_index()
        entries[call_id] = {
            "blob_hash": blob_hash,
            "content_hash": content_hash,
            "tool_name": tool_name,
        }
        self._save_index()

        return content_hash

    def retrieve(self, call_id: str) -> Optional[Dict]:
        """Read artifact by call_id. Returns parsed data or None."""
        entries = self._load_index()
        entry = entries.get(call_id)
        if not entry:
            return None

        blob_data = cas.get_blob(entry["blob_hash"], self._root)
        if blob_data is None:
            logger.warning("Artifact blob %s missing for call_id %s", entry["blob_hash"], call_id)
            return None

        data = json.loads(blob_data)
        return {
            "call_id": call_id,
            "tool_name": entry.get("tool_name", ""),
            "content_hash": entry["content_hash"],
            "data": data,
        }

    def has_content(self, content_hash: str) -> Optional[str]:
        """Check if any artifact in this thread has the given hash.

        Returns the call_id if found, None otherwise.
        """
        entries = self._load_index()
        for call_id, entry in entries.items():
            if entry.get("content_hash") == content_hash:
                return call_id
        return None


def get_artifact_store(thread_id: str, project_path: Path) -> ArtifactStore:
    """Create an ArtifactStore for the given thread."""
    return ArtifactStore(thread_id, project_path)
