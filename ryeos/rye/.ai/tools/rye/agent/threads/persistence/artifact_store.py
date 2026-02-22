# rye:signed:2026-02-22T09:00:56Z:752852983a0ce0134c940c05b417ea044d5481f4b109d5454f64b8e186816cf9:FjM3CIuiBX_4W12k7QdLU_TBek31muWKlsSQbr0toppidtmrnf_Td2vCltCs3mVacuknjlZ-lFVh_rrxRrmvBw==:9fbfabe975fa5a7f
"""
persistence/artifact_store.py: Filesystem-backed artifact store

Stores full tool results that have been trimmed from the context window.
Blobs are keyed by (thread_id, call_id) with content-hash deduplication.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Artifact store for out-of-band tool result persistence"

import hashlib
import json
from datetime import datetime
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import AI_DIR


class ArtifactStore:
    """Filesystem-backed store for out-of-band tool result persistence."""

    def __init__(self, thread_id: str, project_path: Path):
        self.thread_id = thread_id
        self.project_path = Path(project_path)
        self.artifacts_dir = self.project_path / AI_DIR / "agent" / "threads" / thread_id / "artifacts"

    def store(self, call_id: str, tool_name: str, data: Any) -> str:
        """Write artifact to disk and return its content hash.

        Uses atomic write (tmp + rename) and sha256 of deterministically
        serialized data for the content hash.
        """
        serialized = json.dumps(data, sort_keys=True, default=str)
        content_hash = hashlib.sha256(serialized.encode()).hexdigest()

        self.artifacts_dir.mkdir(parents=True, exist_ok=True)

        artifact = {
            "call_id": call_id,
            "tool_name": tool_name,
            "content_hash": content_hash,
            "size_bytes": len(serialized.encode()),
            "stored_at": datetime.utcnow().isoformat(),
            "data": data,
        }

        target = self.artifacts_dir / f"{call_id}.json"
        tmp = self.artifacts_dir / f"{call_id}.json.tmp"

        with open(tmp, "w") as f:
            json.dump(artifact, f, indent=2, default=str)

        tmp.replace(target)
        return content_hash

    def retrieve(self, call_id: str) -> Optional[Dict]:
        """Read artifact by call_id. Returns the full dict or None."""
        path = self.artifacts_dir / f"{call_id}.json"
        if not path.exists():
            return None

        with open(path) as f:
            return json.load(f)

    def has_content(self, content_hash: str) -> Optional[str]:
        """Check if any artifact in this thread has the given hash.

        Returns the call_id if found, None otherwise.
        """
        if not self.artifacts_dir.exists():
            return None

        for path in self.artifacts_dir.glob("*.json"):
            with open(path) as f:
                artifact = json.load(f)
            if artifact.get("content_hash") == content_hash:
                return artifact["call_id"]

        return None


def get_artifact_store(thread_id: str, project_path: Path) -> ArtifactStore:
    """Create an ArtifactStore for the given thread."""
    return ArtifactStore(thread_id, project_path)
