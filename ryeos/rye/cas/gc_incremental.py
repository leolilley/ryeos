"""GC state persistence and incremental mark.

Persists GC state between runs so subsequent collections can skip
a full mark pass.  Durable roots (project refs, user-space, pins,
retained executions) are cached; ephemeral roots (in-flight epochs)
are always walked fresh.
"""

from __future__ import annotations

import collections
import json
import logging
from pathlib import Path
from typing import Dict, List, Optional, Set, Tuple

from rye.cas.gc_types import GCState
from rye.primitives import cas

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# State persistence
# ---------------------------------------------------------------------------


def save_gc_state(
    user_root: Path,
    cas_root: Path | None,
    state: GCState,
) -> None:
    """Write GC state to ``user_root / gc_state.json``."""
    state_file = user_root / "gc_state.json"
    try:
        state_file.parent.mkdir(parents=True, exist_ok=True)
        state_file.write_text(
            json.dumps(state.to_dict()),
            encoding="utf-8",
        )
    except OSError:
        logger.warning("Failed to write GC state to %s", state_file, exc_info=True)


def load_gc_state(user_root: Path) -> Optional[GCState]:
    """Read GC state from ``user_root / gc_state.json``.

    Returns ``None`` if the file is missing or cannot be parsed.
    """
    state_file = user_root / "gc_state.json"
    try:
        data = json.loads(state_file.read_text(encoding="utf-8"))
    except (FileNotFoundError, OSError):
        return None
    except (json.JSONDecodeError, ValueError):
        logger.warning("Corrupt gc_state.json at %s — ignoring", state_file)
        return None

    try:
        return GCState(
            last_gc_at=data.get("last_gc_at", ""),
            last_full_gc_at=data.get("last_full_gc_at", ""),
            reachable_hashes_blob=data.get("reachable_hashes_blob", ""),
            reachable_count=data.get("reachable_count", 0),
            objects_at_last_gc=data.get("objects_at_last_gc", 0),
            generation=data.get("generation", 0),
            invalidated=data.get("invalidated", False),
        )
    except (TypeError, KeyError):
        logger.warning("Invalid gc_state.json structure at %s", state_file)
        return None


def invalidate_gc_state(user_root: Path) -> None:
    """Mark the persisted GC state as invalidated.

    The next incremental GC will fall back to a full mark pass.
    Does nothing if no state file exists.
    """
    state = load_gc_state(user_root)
    if state is None:
        return
    state.invalidated = True
    save_gc_state(user_root, cas_root=None, state=state)


# ---------------------------------------------------------------------------
# Durable root collection
# ---------------------------------------------------------------------------


def _collect_durable_root_hashes(user_root: Path) -> List[str]:
    """Collect all durable root hashes (no ephemeral epochs).

    Sources:
      1. Project refs — ``refs/projects/*/head``
      2. User-space ref — ``refs/user-space/head``
      3. Pin refs — ``refs/pins/**/head``
      4. Retained execution snapshots — ``executions/by-id/*.json``
    """
    roots: List[str] = []

    # 1. Project refs
    projects_dir = user_root / "refs" / "projects"
    if projects_dir.is_dir():
        for head_file in projects_dir.glob("*/head"):
            try:
                h = head_file.read_text(encoding="utf-8").strip()
                if h:
                    roots.append(h)
            except OSError:
                continue

    # 2. User-space ref
    user_space_head = user_root / "refs" / "user-space" / "head"
    try:
        h = user_space_head.read_text(encoding="utf-8").strip()
        if h:
            roots.append(h)
    except (FileNotFoundError, OSError):
        pass

    # 3. Pin refs
    pins_dir = user_root / "refs" / "pins"
    if pins_dir.is_dir():
        for head_file in pins_dir.rglob("head"):
            try:
                h = head_file.read_text(encoding="utf-8").strip()
                if h:
                    roots.append(h)
            except OSError:
                continue

    # 4. Retained execution snapshots
    exec_dir = user_root / "executions" / "by-id"
    if exec_dir.is_dir():
        for exec_file in exec_dir.iterdir():
            if not exec_file.is_file():
                continue
            try:
                data = json.loads(exec_file.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError, ValueError):
                continue
            for key in (
                "snapshot_hash",
                "execution_snapshot_hash",
                "runtime_outputs_bundle_hash",
            ):
                h = data.get(key)
                if h:
                    roots.append(h)

    return roots


# ---------------------------------------------------------------------------
# Incremental mark
# ---------------------------------------------------------------------------


def mark_reachable_incremental(
    user_root: Path,
    cas_root: Path,
    prev_state: GCState,
) -> Tuple[Set[str], bool]:
    """Incremental mark pass that reuses the previous durable reachable set.

    Returns:
        (full_protected_set, was_full_gc)

    If *was_full_gc* is ``True`` the caller should treat the result as
    a fresh full-GC baseline (i.e. persist all of ``full_protected_set``
    as the new durable cache).
    """
    # --- Fallback: invalidated state → full mark -------------------------
    if prev_state.invalidated:
        logger.info("GC state invalidated — performing full mark")
        from rye.cas.gc import mark_reachable

        return mark_reachable(user_root, cas_root), True

    # --- Load cached durable reachable set --------------------------------
    if not prev_state.reachable_hashes_blob:
        logger.info("No reachable-hashes blob recorded — falling back to full mark")
        from rye.cas.gc import mark_reachable

        return mark_reachable(user_root, cas_root), True

    cached_blob = cas.get_blob(prev_state.reachable_hashes_blob, cas_root)
    if cached_blob is None:
        logger.warning(
            "Reachable-hashes blob %s missing from CAS — full mark",
            prev_state.reachable_hashes_blob,
        )
        from rye.cas.gc import mark_reachable

        return mark_reachable(user_root, cas_root), True

    try:
        durable_reachable: Set[str] = set(json.loads(cached_blob))
    except (json.JSONDecodeError, TypeError, ValueError):
        logger.warning("Corrupt reachable-hashes blob — full mark")
        from rye.cas.gc import mark_reachable

        return mark_reachable(user_root, cas_root), True

    # --- Discover new durable roots --------------------------------------
    current_roots = _collect_durable_root_hashes(user_root)
    new_roots = [r for r in current_roots if r not in durable_reachable]

    if new_roots:
        from rye.cas.gc import _extract_refs

        queue: collections.deque[str] = collections.deque(new_roots)
        while queue:
            h = queue.popleft()
            if h in durable_reachable:
                continue
            durable_reachable.add(h)
            obj = cas.get_object(h, cas_root)
            if obj is not None:
                for child in _extract_refs(obj):
                    if child not in durable_reachable:
                        queue.append(child)
            # Also check if it's a blob (some roots reference blobs directly)
            # No children to follow for raw blobs.

    # --- Compute ephemeral reachable FRESH (never cached) -----------------
    ephemeral_reachable: Set[str] = set()
    inflight_dir = user_root / "inflight"
    if inflight_dir.is_dir():
        from rye.cas.gc import _extract_refs

        for epoch_file in inflight_dir.iterdir():
            if not epoch_file.is_file() or epoch_file.suffix != ".json":
                continue
            try:
                epoch_data = json.loads(epoch_file.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError, ValueError):
                continue
            epoch_roots = epoch_data.get("root_hashes", [])
            queue: collections.deque[str] = collections.deque()
            for root_hash in epoch_roots:
                if root_hash not in durable_reachable and root_hash not in ephemeral_reachable:
                    queue.append(root_hash)
            while queue:
                h = queue.popleft()
                if h in durable_reachable or h in ephemeral_reachable:
                    continue
                ephemeral_reachable.add(h)
                obj = cas.get_object(h, cas_root)
                if obj is not None:
                    for child in _extract_refs(obj):
                        if child not in durable_reachable and child not in ephemeral_reachable:
                            queue.append(child)

    return durable_reachable | ephemeral_reachable, False
