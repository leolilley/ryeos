"""Writer epoch management for safe concurrent GC sweep.

Writers register epochs before creating CAS objects and complete them after
ref advance succeeds. GC marks epoch roots as ephemeral, allowing sweep to
run safely even with active writers.
"""

from __future__ import annotations

import json
import logging
import time
import uuid
from datetime import datetime, timezone
from pathlib import Path
from typing import List, Optional

from rye.cas.gc_types import WriterEpoch

logger = logging.getLogger(__name__)


def register_epoch(
    user_root: Path,
    node_id: str,
    user_id: str,
    root_hashes: List[str],
) -> str:
    """Register an in-flight write operation. Call BEFORE creating CAS objects.

    Args:
        user_root: Per-user CAS root (e.g., /cas/<fingerprint>/)
        node_id: ID of the node performing the write
        user_id: User fingerprint
        root_hashes: Base snapshot hashes that will be extended

    Returns:
        epoch_id for use with complete_epoch()
    """
    epoch_id = str(uuid.uuid4())
    epoch = WriterEpoch(
        epoch_id=epoch_id,
        node_id=node_id,
        user_id=user_id,
        root_hashes=root_hashes,
        created_at=datetime.now(timezone.utc).isoformat(),
    )
    inflight_dir = user_root / "inflight"
    inflight_dir.mkdir(parents=True, exist_ok=True)
    epoch_file = inflight_dir / f"{epoch_id}.json"
    epoch_file.write_text(json.dumps(epoch.to_dict()), encoding="utf-8")
    logger.debug("Registered epoch %s with %d root hashes", epoch_id, len(root_hashes))
    return epoch_id


def complete_epoch(user_root: Path, epoch_id: str) -> None:
    """Complete an epoch after ref advance succeeds. Removes the marker."""
    epoch_file = user_root / "inflight" / f"{epoch_id}.json"
    epoch_file.unlink(missing_ok=True)
    logger.debug("Completed epoch %s", epoch_id)


def load_epoch(user_root: Path, epoch_id: str) -> Optional[WriterEpoch]:
    """Load a specific epoch by ID."""
    epoch_file = user_root / "inflight" / f"{epoch_id}.json"
    if not epoch_file.exists():
        return None
    try:
        data = json.loads(epoch_file.read_text(encoding="utf-8"))
        return WriterEpoch(**data)
    except (json.JSONDecodeError, TypeError):
        return None


def list_active_epochs(user_root: Path) -> List[WriterEpoch]:
    """List all active (non-stale) epoch markers."""
    inflight_dir = user_root / "inflight"
    if not inflight_dir.exists():
        return []
    epochs = []
    for epoch_file in inflight_dir.iterdir():
        if not epoch_file.is_file() or not epoch_file.suffix == ".json":
            continue
        try:
            data = json.loads(epoch_file.read_text(encoding="utf-8"))
            epochs.append(WriterEpoch(**data))
        except (json.JSONDecodeError, TypeError):
            continue
    return epochs


def cleanup_stale_epochs(user_root: Path, max_age_seconds: int = 1800) -> int:
    """Remove epoch markers older than max_age (crashed writers).

    Args:
        user_root: Per-user CAS root
        max_age_seconds: Maximum age before considering an epoch stale (default: 30 min)

    Returns:
        Number of stale epochs removed
    """
    inflight_dir = user_root / "inflight"
    if not inflight_dir.exists():
        return 0
    cutoff = time.time() - max_age_seconds
    removed = 0
    for epoch_file in inflight_dir.iterdir():
        if not epoch_file.is_file():
            continue
        try:
            if epoch_file.stat().st_mtime < cutoff:
                epoch_id = epoch_file.stem
                epoch_file.unlink(missing_ok=True)
                removed += 1
                logger.info("Removed stale epoch %s (age > %ds)", epoch_id, max_age_seconds)
        except OSError:
            continue
    return removed


def oldest_epoch_time(user_root: Path) -> Optional[float]:
    """Return the created_at timestamp of the oldest active epoch, or None if no epochs."""
    inflight_dir = user_root / "inflight"
    if not inflight_dir.exists():
        return None
    oldest: Optional[float] = None
    for epoch_file in inflight_dir.iterdir():
        if not epoch_file.is_file() or not epoch_file.suffix == ".json":
            continue
        try:
            data = json.loads(epoch_file.read_text(encoding="utf-8"))
            created = datetime.fromisoformat(data["created_at"]).timestamp()
            if oldest is None or created < oldest:
                oldest = created
        except (json.JSONDecodeError, KeyError, ValueError):
            # If we can't parse, use file mtime as conservative fallback
            try:
                mtime = epoch_file.stat().st_mtime
                if oldest is None or mtime < oldest:
                    oldest = mtime
            except OSError:
                continue
    return oldest
