"""Core GC engine — 3-phase pipeline for CAS garbage collection.

Phase 1: Cache & execution pruning (immediate wins, no DAG work)
Phase 2: History compaction (rewrite ProjectSnapshot DAG)
Phase 3: Mark-and-sweep (delete unreachable CAS objects)
"""

from __future__ import annotations

import collections
import hashlib
import json
import logging
import os
import shutil
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from typing import Any, Dict, List, Optional, Set

from rye.cas.gc_epochs import (
    cleanup_stale_epochs,
    list_active_epochs,
    oldest_epoch_time,
)
from rye.cas.gc_lock import acquire, release, update_phase as update_lock_phase
from rye.cas.gc_types import (
    DEFAULT_RETENTION,
    CompactionResult,
    GCResult,
    GCState,
    PruneResult,
    RetentionPolicy,
    SweepResult,
)
from rye.cas.objects import get_history
from rye.cas.refs import read_ref, write_ref_atomic
from rye.constants import AI_DIR
from rye.primitives import cas

logger = logging.getLogger(__name__)

# ---------------------------------------------------------------------------
# Kind-aware reference extraction table
# ---------------------------------------------------------------------------

HASH_FIELDS: Dict[str, list] = {
    "project_snapshot": [
        "project_manifest_hash",
        "user_manifest_hash",
        ("parent_hashes", "list"),
    ],
    "source_manifest": [
        ("items", "dict_values"),
        ("files", "dict_values"),
    ],
    "item_source": [
        "content_blob_hash",
    ],
    "execution_snapshot": [
        "project_manifest_hash",
        "user_manifest_hash",
        "state_hash",
        ("node_receipts", "list"),
    ],
    "node_receipt": [
        "node_input_hash",
        "node_result_hash",
    ],
    "node_input": [
        "graph_hash",
        "config_snapshot_hash",
    ],
    "runtime_outputs_bundle": [
        "execution_snapshot_hash",
        ("files", "dict_values"),
    ],
    "artifact_index": [
        ("entries", "nested_dict_values"),
    ],
}


def _extract_refs(obj: dict) -> List[str]:
    """Extract child object/blob hashes from a CAS object.

    Dispatches on ``obj["kind"]`` for known types, falls back to
    conservative heuristic for unknown kinds.
    """
    kind = obj.get("kind", "")
    refs: List[str] = []

    if kind in HASH_FIELDS:
        for field_spec in HASH_FIELDS[kind]:
            if isinstance(field_spec, tuple):
                field_name, field_type = field_spec
                value = obj.get(field_name)
                if value is None:
                    continue
                if field_type == "list":
                    refs.extend(h for h in value if isinstance(h, str))
                elif field_type == "dict_values":
                    if isinstance(value, dict):
                        refs.extend(h for h in value.values() if isinstance(h, str))
                elif field_type == "nested_dict_values":
                    if isinstance(value, dict):
                        for inner in value.values():
                            if isinstance(inner, dict):
                                refs.extend(
                                    h for h in inner.values() if isinstance(h, str)
                                )
            else:
                h = obj.get(field_spec)
                if h and isinstance(h, str):
                    refs.append(h)
    else:
        refs.extend(_extract_unknown_refs(obj))

    return refs


def _extract_unknown_refs(obj: dict) -> List[str]:
    """Conservative fallback: follow ``*_hash`` / ``*_hashes`` fields."""
    refs: List[str] = []
    for key, value in obj.items():
        if key.endswith("_hash") and isinstance(value, str) and len(value) == 64:
            refs.append(value)
        elif key.endswith("_hashes") and isinstance(value, list):
            for h in value:
                if isinstance(h, str) and len(h) == 64:
                    refs.append(h)
    return refs


def _extract_all_hashes(data: dict) -> List[str]:
    """Extract all SHA256-like strings from a flat dict (for ``running/`` markers)."""
    hashes: List[str] = []
    for value in data.values():
        if isinstance(value, str) and len(value) == 64:
            hashes.append(value)
        elif isinstance(value, list):
            for item in value:
                if isinstance(item, str) and len(item) == 64:
                    hashes.append(item)
    return hashes


def _parse_timestamp(s: str) -> Optional[datetime]:
    """Parse an ISO timestamp, returning None on failure."""
    if not s:
        return None
    try:
        return datetime.fromisoformat(s)
    except (ValueError, TypeError):
        return None


# ---------------------------------------------------------------------------
# Helpers
# ---------------------------------------------------------------------------


def _dir_size(path: Path) -> int:
    """Sum file sizes under *path*."""
    total = 0
    try:
        for f in path.rglob("*"):
            if f.is_file():
                try:
                    total += f.stat().st_size
                except OSError:
                    pass
    except OSError:
        pass
    return total


def _prune_empty_dirs(root: Path) -> None:
    """Remove empty directories bottom-up."""
    if not root.is_dir():
        return
    for dirpath, dirnames, filenames in os.walk(str(root), topdown=False):
        if not dirnames and not filenames:
            try:
                os.rmdir(dirpath)
            except OSError:
                pass


def _human_bytes(n: int) -> str:
    v = float(n)
    for unit in ("B", "KB", "MB", "GB"):
        if abs(v) < 1024:
            return f"{v:.1f} {unit}"
        v /= 1024
    return f"{v:.1f} TB"


# ---------------------------------------------------------------------------
# Phase 1: Cache & execution pruning
# ---------------------------------------------------------------------------


def prune_cache(
    user_root: Path,
    *,
    max_age_hours: int = 24,
    emergency: bool = False,
) -> PruneResult:
    """Delete stale materialized cache snapshots and user-space caches."""
    result = PruneResult()
    cutoff = time.time() - (max_age_hours * 3600)

    for subdir_name in ("snapshots", "user"):
        cache_dir = user_root / "cache" / subdir_name
        if not cache_dir.is_dir():
            continue
        try:
            entries = list(cache_dir.iterdir())
        except OSError:
            continue
        for entry in entries:
            try:
                if not emergency and entry.stat().st_mtime >= cutoff:
                    continue
            except OSError:
                continue
            size = _dir_size(entry)
            try:
                if entry.is_dir():
                    shutil.rmtree(entry)
                else:
                    entry.unlink()
            except OSError:
                logger.warning("Failed to remove cache entry %s", entry, exc_info=True)
                continue
            result.cache_entries_deleted += 1
            result.cache_bytes_freed += size

    result.total_freed = result.cache_bytes_freed
    return result


def prune_executions(
    user_root: Path,
    *,
    max_success: int = 10,
    max_failure: int = 10,
) -> PruneResult:
    """Delete excess execution metadata records per (project, graph)."""
    result = PruneResult()
    exec_dir = user_root / "executions" / "by-id"
    if not exec_dir.is_dir():
        return result

    # Group by (project_path, graph_id)
    # Execution records may or may not have .json extension
    groups: Dict[tuple, List[tuple]] = {}
    for exec_file in exec_dir.iterdir():
        if not exec_file.is_file():
            continue
        try:
            data = json.loads(exec_file.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, ValueError):
            continue
        # Execution records use item_ref, not graph_id
        graph_id = data.get("graph_id", "")
        if not graph_id:
            graph_id = data.get("item_ref", "")
        key = (data.get("project_path", ""), graph_id)
        # Records use "state" not "status", "created_at"/"completed_at" not "timestamp"
        timestamp = (
            data.get("timestamp", "")
            or data.get("completed_at", "")
            or data.get("created_at", "")
        )
        status = data.get("status", "") or data.get("state", "")
        groups.setdefault(key, []).append((timestamp, status, exec_file))

    for entries in groups.values():
        # Sort by timestamp descending
        entries.sort(key=lambda e: e[0], reverse=True)

        keep_statuses = {"queued", "running"}
        success_statuses = {"success", "completed"}
        failure_statuses = {"failed", "error"}
        success_count = 0
        failure_count = 0
        for timestamp, status, exec_file in entries:
            if status in keep_statuses:
                continue
            if status in success_statuses:
                success_count += 1
                if success_count <= max_success:
                    continue
            elif status in failure_statuses:
                failure_count += 1
                if failure_count <= max_failure:
                    continue
            else:
                # Unknown status — treat as failure bucket
                failure_count += 1
                if failure_count <= max_failure:
                    continue
            try:
                exec_file.unlink()
                result.executions_deleted += 1
            except OSError:
                pass

    return result


# ---------------------------------------------------------------------------
# Retention policy loading
# ---------------------------------------------------------------------------


def load_retention_policy(
    cas_root: Path,
    project_manifest_hash: str,
) -> RetentionPolicy:
    """Load per-project retention policy from ``.ai/config/gc.yaml``."""
    if not project_manifest_hash:
        return DEFAULT_RETENTION

    manifest = cas.get_object(project_manifest_hash, cas_root)
    if manifest is None:
        return DEFAULT_RETENTION

    gc_config_hash = manifest.get("files", {}).get(f"{AI_DIR}/config/gc.yaml")
    if not gc_config_hash:
        return DEFAULT_RETENTION

    gc_blob = cas.get_blob(gc_config_hash, cas_root)
    if gc_blob is None:
        return DEFAULT_RETENTION

    try:
        import yaml

        config = yaml.safe_load(gc_blob)
    except Exception:
        logger.warning("Failed to parse gc.yaml blob %s", gc_config_hash)
        return DEFAULT_RETENTION

    if not isinstance(config, dict):
        return DEFAULT_RETENTION

    retention = config.get("retention", {})
    if not isinstance(retention, dict):
        return DEFAULT_RETENTION

    exec_hist = retention.get("execution_history", {})
    if not isinstance(exec_hist, dict):
        exec_hist = {}

    pinned_entries = retention.get("pinned", [])
    pinned_hashes = []
    if isinstance(pinned_entries, list):
        for p in pinned_entries:
            if isinstance(p, dict) and "hash" in p:
                pinned_hashes.append(p["hash"])
            elif isinstance(p, str):
                pinned_hashes.append(p)

    return RetentionPolicy(
        manual_pushes=retention.get("manual_pushes", 3),
        daily_checkpoints=retention.get("daily_checkpoints", 7),
        weekly_checkpoints=retention.get("weekly_checkpoints", 0),
        max_success_executions=exec_hist.get("success", 10),
        max_failure_executions=exec_hist.get("failure", 10),
        pinned=pinned_hashes,
    )


# ---------------------------------------------------------------------------
# Phase 2: History compaction
# ---------------------------------------------------------------------------


def compact_project_history(
    user_root: Path,
    project_ref_dir: Path,
    cas_root: Path,
    *,
    policy: Optional[RetentionPolicy] = None,
    dry_run: bool = False,
) -> CompactionResult:
    """Rewrite the first-parent snapshot chain, retaining only checkpoints."""
    head_file = project_ref_dir / "head"
    head_hash = read_ref(head_file)
    if not head_hash:
        return CompactionResult(skipped=True, skip_reason="no_head")

    # Walk full first-parent chain (newest → oldest)
    chain = get_history(head_hash, cas_root, limit=50_000)
    if not chain:
        return CompactionResult(skipped=True, skip_reason="empty_chain")

    # Safety: chain too deep
    if len(chain) == 50_000:
        return CompactionResult(
            retained_count=len(chain),
            skipped=True,
            skip_reason="history_too_deep",
        )

    # Safety: incomplete chain (walk truncated by missing object)
    last = chain[-1]
    if last.get("parent_hashes"):
        return CompactionResult(
            retained_count=len(chain),
            skipped=True,
            skip_reason="incomplete_history_possible_corruption",
        )

    # Load retention policy from HEAD's manifest if not supplied
    if policy is None:
        head_obj = cas.get_object(head_hash, cas_root)
        if head_obj:
            policy = load_retention_policy(
                cas_root, head_obj.get("project_manifest_hash", "")
            )
        else:
            policy = DEFAULT_RETENTION

    # --- Classify retained snapshots ---
    retained: Set[str] = set()
    retained.add(head_hash)  # HEAD always kept

    # Pinned snapshots from refs/pins/<project>/
    pinned_hashes: Set[str] = set()
    pins_dir = user_root / "refs" / "pins" / project_ref_dir.name
    if pins_dir.is_dir():
        for pin_dir in pins_dir.iterdir():
            pin_head = pin_dir / "head"
            if pin_head.is_file():
                try:
                    pinned_hashes.add(pin_head.read_text(encoding="utf-8").strip())
                except OSError:
                    pass
    pinned_hashes.update(policy.pinned)
    chain_hashes = {s["_hash"] for s in chain}
    for ph in pinned_hashes:
        if ph in chain_hashes:
            retained.add(ph)

    # Last N manual pushes
    manual_pushes = [s for s in chain if s.get("source") == "push"]
    for snap in manual_pushes[: policy.manual_pushes]:
        retained.add(snap["_hash"])

    # 1 daily checkpoint per day for last N days
    now_utc = datetime.now(timezone.utc)
    daily_cutoff = now_utc - timedelta(days=policy.daily_checkpoints)
    seen_days: Set[Any] = set()
    for snap in chain:
        ts = _parse_timestamp(snap.get("timestamp", ""))
        if ts and ts >= daily_cutoff:
            day_key = ts.date()
            if day_key not in seen_days:
                seen_days.add(day_key)
                retained.add(snap["_hash"])

    # Weekly checkpoints beyond daily window
    if policy.weekly_checkpoints > 0:
        weekly_start = daily_cutoff
        weekly_cutoff = weekly_start - timedelta(weeks=policy.weekly_checkpoints)
        seen_weeks: Set[Any] = set()
        for snap in chain:
            ts = _parse_timestamp(snap.get("timestamp", ""))
            if ts and weekly_cutoff <= ts < weekly_start:
                week_key = ts.isocalendar()[:2]  # (year, week_number)
                if week_key not in seen_weeks:
                    seen_weeks.add(week_key)
                    retained.add(snap["_hash"])

    # Nothing to compact
    if len(retained) >= len(chain):
        return CompactionResult(retained_count=len(retained), discarded_count=0)

    if dry_run:
        return CompactionResult(
            retained_count=len(retained),
            discarded_count=len(chain) - len(retained),
            old_head=head_hash,
            dry_run=True,
        )

    # --- Rewrite chain oldest → newest ---
    # chain is newest→oldest; filter and preserve order, then reverse
    retained_chain = [s for s in chain if s["_hash"] in retained]  # newest→oldest

    prev_hash: Optional[str] = None  # oldest gets parent_hashes=[]
    old_to_new: Dict[str, str] = {}

    for snap in reversed(retained_chain):  # oldest → newest
        snap_hash = snap["_hash"]
        is_pinned = snap_hash in pinned_hashes

        new_snap = {k: v for k, v in snap.items() if k != "_hash"}
        new_snap["parent_hashes"] = [prev_hash] if prev_hash else []
        new_snap["metadata"] = {
            **new_snap.get("metadata", {}),
            "gc_compacted": True,
            "original_hash": snap_hash,
            "original_parent_hashes": snap.get("parent_hashes", []),
        }
        if is_pinned:
            new_snap["metadata"]["was_pinned"] = True

        new_hash = cas.store_object(new_snap, cas_root)
        old_to_new[snap_hash] = new_hash

        if is_pinned:
            try:
                pin_dirs = list(
                    (user_root / "refs" / "pins" / project_ref_dir.name).iterdir()
                )
                for pin_dir in pin_dirs:
                    pin_head = pin_dir / "head"
                    if pin_head.is_file():
                        try:
                            if (
                                pin_head.read_text(encoding="utf-8").strip()
                                == snap_hash
                            ):
                                write_ref_atomic(pin_head, new_hash)
                        except OSError:
                            pass
            except OSError:
                pass

        prev_hash = new_hash

    # prev_hash is now the rewritten HEAD (last written = newest)
    new_head = prev_hash
    if new_head is None:
        return CompactionResult(skipped=True, skip_reason="empty_retained_chain")

    # Compare-and-swap on HEAD ref
    current_head = read_ref(head_file)
    if current_head != head_hash:
        return CompactionResult(
            retained_count=len(retained),
            discarded_count=0,
            skipped=True,
            skip_reason="head_moved_during_compaction",
        )

    write_ref_atomic(head_file, new_head)

    # Invalidate incremental GC state — compaction changes the reachable set
    try:
        from rye.cas.gc_incremental import invalidate_gc_state

        invalidate_gc_state(user_root)
    except Exception:
        pass

    return CompactionResult(
        retained_count=len(retained),
        discarded_count=len(chain) - len(retained),
        new_head=new_head,
        old_head=head_hash,
    )


# ---------------------------------------------------------------------------
# Pin management
# ---------------------------------------------------------------------------


def pin_snapshot(
    user_root: Path,
    project_hash: str,
    snapshot_hash: str,
    label: str,
) -> str:
    """Create a durable pin ref for a snapshot. Returns pin_id."""
    pin_id = hashlib.sha256(f"{snapshot_hash}:{label}".encode()).hexdigest()[:16]
    pin_dir = user_root / "refs" / "pins" / project_hash / pin_id
    pin_dir.mkdir(parents=True, exist_ok=True)
    write_ref_atomic(pin_dir / "head", snapshot_hash)
    meta = {
        "label": label,
        "snapshot_hash": snapshot_hash,
        "pinned_at": datetime.now(timezone.utc).isoformat(),
    }
    (pin_dir / "meta.json").write_text(json.dumps(meta), encoding="utf-8")
    logger.info("Pinned snapshot %s as %s (%s)", snapshot_hash[:12], pin_id, label)
    return pin_id


def unpin_snapshot(
    user_root: Path,
    project_hash: str,
    pin_id: str,
) -> bool:
    """Remove a pin ref. Returns True if it existed."""
    pin_dir = user_root / "refs" / "pins" / project_hash / pin_id
    if not pin_dir.exists():
        return False
    shutil.rmtree(pin_dir, ignore_errors=True)
    try:
        from rye.cas.gc_incremental import invalidate_gc_state

        invalidate_gc_state(user_root)
    except Exception:
        pass
    logger.info("Unpinned %s/%s", project_hash[:12], pin_id)
    return True


# ---------------------------------------------------------------------------
# Phase 3: Mark-and-sweep
# ---------------------------------------------------------------------------


def _collect_roots_server(user_root: Path) -> List[str]:
    """Collect GC root hashes from server-side per-user layout."""
    roots: List[str] = []

    # Project HEAD refs
    projects_dir = user_root / "refs" / "projects"
    if projects_dir.is_dir():
        for head_file in projects_dir.glob("*/head"):
            try:
                h = head_file.read_text(encoding="utf-8").strip()
                if h:
                    roots.append(h)
            except OSError:
                continue

    # User-space HEAD
    user_head = user_root / "refs" / "user-space" / "head"
    try:
        h = user_head.read_text(encoding="utf-8").strip()
        if h:
            roots.append(h)
    except (FileNotFoundError, OSError):
        pass

    # Pin refs
    pins_dir = user_root / "refs" / "pins"
    if pins_dir.is_dir():
        for head_file in pins_dir.rglob("head"):
            try:
                h = head_file.read_text(encoding="utf-8").strip()
                if h:
                    roots.append(h)
            except OSError:
                continue

    # Retained execution snapshots (files may or may not have .json extension)
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

    # Running markers
    running_dir = user_root / "running"
    if running_dir.is_dir():
        for running_file in running_dir.iterdir():
            if not running_file.is_file():
                continue
            try:
                data = json.loads(running_file.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError, ValueError):
                continue
            roots.extend(_extract_all_hashes(data))

    # In-flight writer epoch roots (ephemeral)
    inflight_dir = user_root / "inflight"
    if inflight_dir.is_dir():
        for epoch_file in inflight_dir.iterdir():
            if not epoch_file.is_file() or epoch_file.suffix != ".json":
                continue
            try:
                epoch_data = json.loads(epoch_file.read_text(encoding="utf-8"))
            except (OSError, json.JSONDecodeError, ValueError):
                continue
            for h in epoch_data.get("root_hashes", []):
                if h:
                    roots.append(h)

    return roots


def _collect_roots_local(cas_root: Path) -> List[str]:
    """Collect GC root hashes from local project CAS layout.

    Local layout: cas_root/refs/ contains graphs/, remotes/, artifacts/
    with JSON files that hold snapshot/hash references.
    """
    roots: List[str] = []
    refs_dir = cas_root / "refs"
    if not refs_dir.is_dir():
        return roots

    for ref_file in refs_dir.rglob("*.json"):
        try:
            data = json.loads(ref_file.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError, ValueError):
            continue
        # Extract any hash-like values from the ref
        for key in (
            "hash",
            "snapshot_hash",
            "execution_snapshot_hash",
            "runtime_outputs_bundle_hash",
            "manifest_hash",
        ):
            h = data.get(key)
            if h and isinstance(h, str):
                roots.append(h)
        # Also check entries dict (artifact refs)
        entries = data.get("entries")
        if isinstance(entries, dict):
            for v in entries.values():
                if isinstance(v, dict):
                    for bh in v.values():
                        if isinstance(bh, str) and len(bh) == 64:
                            roots.append(bh)
                elif isinstance(v, str) and len(v) == 64:
                    roots.append(v)

    return roots


def mark_reachable(user_root: Path, cas_root: Path) -> Set[str]:
    """Walk all live refs and return the set of reachable object+blob hashes.

    Uses iterative BFS (deque) — safe for 100K+ objects.
    Supports both server-side (user_root != cas_root parent) and local
    (cas_root/refs/) CAS layouts.
    """
    reachable: Set[str] = set()
    to_visit: collections.deque[str] = collections.deque()

    # Collect roots from server layout
    to_visit.extend(_collect_roots_server(user_root))

    # Also collect roots from local layout (cas_root/refs/)
    to_visit.extend(_collect_roots_local(cas_root))

    # --- Iterative traversal ---
    while to_visit:
        obj_hash = to_visit.pop()
        if not obj_hash or obj_hash in reachable:
            continue
        reachable.add(obj_hash)

        obj = cas.get_object(obj_hash, cas_root)
        if obj is None:
            continue

        for child_hash in _extract_refs(obj):
            if child_hash and child_hash not in reachable:
                to_visit.append(child_hash)

    return reachable


def _walk_shards(root: Path):
    """Yield (path, is_dir) for immediate children using scandir (avoids rglob)."""
    try:
        for entry in os.scandir(root):
            try:
                yield Path(entry.path), entry.is_dir()
            except OSError:
                continue
    except OSError:
        return


def _walk_shard_files(root: Path):
    """Yield file paths under a shard directory tree using os.walk."""
    try:
        for dirpath, _dirnames, filenames in os.walk(root):
            for name in filenames:
                yield Path(dirpath, name)
    except OSError:
        return


def sweep(
    user_root: Path,
    cas_root: Path,
    reachable: Set[str],
    *,
    grace_seconds: int = 3600,
    dry_run: bool = False,
) -> SweepResult:
    """Delete CAS objects and blobs not in the reachable set.

    Epoch-aware: never deletes objects newer than the oldest active epoch.
    """
    running_dir = user_root / "running"
    if running_dir.is_dir():
        try:
            if any(running_dir.iterdir()):
                logger.info(
                    "Sweep running with active executions (protected by writer epochs)"
                )
        except OSError:
            pass

    gc_start = time.time()

    cleanup_stale_epochs(user_root)
    oldest_epoch = oldest_epoch_time(user_root)
    epoch_max_age = 1800
    if oldest_epoch is not None:
        effective_epoch = max(oldest_epoch, gc_start - epoch_max_age)
        grace_cutoff = min(gc_start - grace_seconds, effective_epoch)
    else:
        grace_cutoff = gc_start - grace_seconds

    deleted_objects = 0
    deleted_blobs = 0
    freed_bytes = 0

    # Sweep objects — walk shard dirs then files within
    objects_dir = cas_root / "objects"
    if objects_dir.is_dir():
        for shard_path, is_dir in _walk_shards(objects_dir):
            if not is_dir:
                continue
            for obj_path in _walk_shard_files(shard_path):
                if not obj_path.suffix == ".json":
                    continue
                obj_hash = obj_path.stem
                if obj_hash in reachable:
                    continue
                try:
                    st = obj_path.stat()
                except OSError:
                    continue
                if st.st_mtime >= grace_cutoff:
                    continue
                if not dry_run:
                    try:
                        obj_path.unlink()
                    except OSError:
                        continue
                freed_bytes += st.st_size
                deleted_objects += 1

    # Sweep blobs — walk shard dirs then files within
    blobs_dir = cas_root / "blobs"
    if blobs_dir.is_dir():
        for shard_path, is_dir in _walk_shards(blobs_dir):
            if not is_dir:
                continue
            for blob_path in _walk_shard_files(shard_path):
                if not blob_path.is_file():
                    continue
                blob_hash = blob_path.name
                if blob_hash in reachable:
                    continue
                try:
                    st = blob_path.stat()
                except OSError:
                    continue
                if st.st_mtime >= grace_cutoff:
                    continue
                if not dry_run:
                    try:
                        blob_path.unlink()
                    except OSError:
                        continue
                freed_bytes += st.st_size
                deleted_blobs += 1

    # Clean empty shard directories
    if not dry_run:
        _prune_empty_dirs(objects_dir)
        _prune_empty_dirs(blobs_dir)

    return SweepResult(
        deleted_objects=deleted_objects,
        deleted_blobs=deleted_blobs,
        freed_bytes=freed_bytes,
        dry_run=dry_run,
    )


# ---------------------------------------------------------------------------
# Observability
# ---------------------------------------------------------------------------


def emit_gc_event(
    user_root: Path,
    node_id: str,
    prune: PruneResult,
    compaction: Dict[str, CompactionResult],
    sweep_result: SweepResult,
    total_freed: int,
    duration_ms: int,
    max_log_entries: int = 500,
) -> None:
    """Write a structured GC event to the log file and stdout."""
    event: Dict[str, Any] = {
        "event": "gc_complete",
        "node_id": node_id,
        "user_root": str(user_root),
        "timestamp": datetime.now(timezone.utc).isoformat(),
        "duration_ms": duration_ms,
        "phase1": {
            "cache_entries_deleted": prune.cache_entries_deleted,
            "cache_bytes_freed": prune.cache_bytes_freed,
            "executions_deleted": prune.executions_deleted,
        },
        "phase2": {
            "projects_compacted": sum(
                1 for r in compaction.values() if r.discarded_count > 0
            ),
            "projects_skipped": sum(1 for r in compaction.values() if r.skipped),
            "total_snapshots_discarded": sum(
                r.discarded_count for r in compaction.values()
            ),
            "total_snapshots_retained": sum(
                r.retained_count for r in compaction.values()
            ),
        },
        "phase3": {
            "objects_swept": sweep_result.deleted_objects,
            "blobs_swept": sweep_result.deleted_blobs,
            "bytes_freed": sweep_result.freed_bytes,
            "skipped": sweep_result.skipped,
            "skip_reason": sweep_result.skip_reason,
        },
        "total_freed_bytes": total_freed,
        "total_freed_human": _human_bytes(total_freed),
    }

    gc_log = user_root / "logs" / "gc.jsonl"
    try:
        gc_log.parent.mkdir(parents=True, exist_ok=True)

        existing_lines: list[str] = []
        if gc_log.is_file():
            try:
                raw = gc_log.read_text(encoding="utf-8")
                all_lines = raw.strip().split("\n")
                if len(all_lines) > max_log_entries:
                    existing_lines = all_lines[-(max_log_entries // 2) :]
                else:
                    existing_lines = all_lines
            except OSError:
                pass

        with open(gc_log, "w", encoding="utf-8") as f:
            for line in existing_lines:
                f.write(line + "\n")
            f.write(json.dumps(event) + "\n")
    except OSError:
        logger.warning("Failed to write GC event to %s", gc_log, exc_info=True)

    logger.info("GC complete: freed %s in %dms", _human_bytes(total_freed), duration_ms)


# ---------------------------------------------------------------------------
# Orchestrator
# ---------------------------------------------------------------------------


def run_gc(
    user_root: Path,
    cas_root: Path,
    *,
    node_id: str = "local",
    dry_run: bool = False,
    aggressive: bool = False,
    policy: Optional[RetentionPolicy] = None,
    use_incremental: bool = True,
) -> GCResult:
    """Full 3-phase GC orchestrator with distributed coordination."""
    from rye.cas.gc_incremental import (
        load_gc_state,
        mark_reachable_incremental,
        save_gc_state,
    )

    start_time = time.time()

    # Phase 1: Cache & execution pruning (no lock needed)
    prune_result = prune_cache(user_root, emergency=aggressive)
    exec_prune = prune_executions(
        user_root,
        max_success=policy.max_success_executions if policy else 10,
        max_failure=policy.max_failure_executions if policy else 10,
    )
    prune_result += exec_prune

    # Acquire distributed lock for Phases 2+3
    lock = acquire(user_root, node_id)
    if lock is None:
        return GCResult(
            prune=prune_result,
            compaction={},
            sweep=SweepResult(skipped=True, skip_reason="gc_lock_held_by_another_node"),
            total_freed_bytes=prune_result.total_freed,
            duration_ms=int((time.time() - start_time) * 1000),
        )

    try:
        # Phase 2: History compaction
        update_lock_phase(user_root, node_id, "compact")
        compaction_results: Dict[str, CompactionResult] = {}

        projects_dir = user_root / "refs" / "projects"
        if projects_dir.is_dir():
            for project_dir in projects_dir.iterdir():
                if not project_dir.is_dir():
                    continue
                head_file = project_dir / "head"
                if not head_file.is_file():
                    continue

                # Determine policy for this project
                proj_policy = policy
                if proj_policy is None:
                    head_hash = read_ref(head_file)
                    if head_hash:
                        head_obj = cas.get_object(head_hash, cas_root)
                        if head_obj:
                            proj_policy = load_retention_policy(
                                cas_root,
                                head_obj.get("project_manifest_hash", ""),
                            )
                if proj_policy is None:
                    proj_policy = DEFAULT_RETENTION

                if aggressive:
                    proj_policy = RetentionPolicy(
                        manual_pushes=1,
                        daily_checkpoints=1,
                        weekly_checkpoints=0,
                    )

                result = compact_project_history(
                    user_root,
                    project_dir,
                    cas_root,
                    policy=proj_policy,
                    dry_run=dry_run,
                )
                compaction_results[project_dir.name] = result

        # Clean stale writer epochs
        cleanup_stale_epochs(user_root)

        # Phase 3: Mark
        update_lock_phase(user_root, node_id, "mark")

        any_compacted = any(r.discarded_count > 0 for r in compaction_results.values())
        prev_state = load_gc_state(user_root) if use_incremental else None

        # durable_reachable is persisted for incremental; reachable is the full sweep set
        durable_reachable: Optional[Set[str]] = None
        if prev_state and not prev_state.invalidated and not any_compacted:
            reachable, was_full = mark_reachable_incremental(
                user_root, cas_root, prev_state
            )
            # mark_reachable_incremental returns durable ∪ ephemeral;
            # for persistence we need durable only — re-collect it
            if not was_full:
                from rye.cas.gc_incremental import _collect_durable_root_hashes

                # The durable set is the cached blob + new durable roots (already computed)
                # Re-load from the blob and add new roots — but simpler to just
                # compute durable roots fresh and walk them (cheap vs full mark)
                cached_blob = cas.get_blob(prev_state.reachable_hashes_blob, cas_root)
                if cached_blob:
                    try:
                        durable_reachable = set(json.loads(cached_blob))
                        # Add any new durable roots
                        new_durable = _collect_durable_root_hashes(user_root)
                        new_to_visit: collections.deque[str] = collections.deque(
                            r for r in new_durable if r not in durable_reachable
                        )
                        while new_to_visit:
                            h = new_to_visit.popleft()
                            if h in durable_reachable:
                                continue
                            durable_reachable.add(h)
                            obj = cas.get_object(h, cas_root)
                            if obj is not None:
                                for ch in _extract_refs(obj):
                                    if ch not in durable_reachable:
                                        new_to_visit.append(ch)
                    except (json.JSONDecodeError, TypeError):
                        durable_reachable = None
        else:
            reachable = mark_reachable(user_root, cas_root)
            was_full = True
            durable_reachable = reachable  # full mark = all are durable

        # Phase 3: Sweep
        update_lock_phase(user_root, node_id, "sweep")
        sweep_result = sweep(user_root, cas_root, reachable, dry_run=dry_run)

        # Save GC state for incremental runs — persist DURABLE only
        if not dry_run and not sweep_result.skipped:
            save_set = durable_reachable if durable_reachable is not None else reachable
            reachable_blob = json.dumps(sorted(save_set)).encode("utf-8")
            blob_hash = cas.store_blob(reachable_blob, cas_root)
            save_gc_state(
                user_root,
                cas_root,
                GCState(
                    last_gc_at=datetime.now(timezone.utc).isoformat(),
                    last_full_gc_at=(
                        datetime.now(timezone.utc).isoformat()
                        if was_full
                        else (prev_state.last_full_gc_at if prev_state else "")
                    ),
                    reachable_hashes_blob=blob_hash,
                    reachable_count=len(reachable),
                    objects_at_last_gc=len(reachable),
                    generation=(prev_state.generation + 1) if prev_state else 1,
                ),
            )

        total_freed = prune_result.total_freed + sweep_result.freed_bytes
        duration_ms = int((time.time() - start_time) * 1000)

        emit_gc_event(
            user_root,
            node_id,
            prune_result,
            compaction_results,
            sweep_result,
            total_freed,
            duration_ms,
        )

        return GCResult(
            prune=prune_result,
            compaction=compaction_results,
            sweep=sweep_result,
            total_freed_bytes=total_freed,
            duration_ms=duration_ms,
        )
    finally:
        release(user_root, node_id)
