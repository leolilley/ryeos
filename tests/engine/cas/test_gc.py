"""Tests for rye.cas.gc — core GC engine (prune, compact, mark, sweep, pins)."""

import hashlib
import json
import os
import time
from datetime import datetime, timedelta, timezone
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.cas.gc import (
    HASH_FIELDS,
    _extract_all_hashes,
    _extract_refs,
    _extract_unknown_refs,
    compact_project_history,
    mark_reachable,
    pin_snapshot,
    prune_cache,
    prune_executions,
    run_gc,
    sweep,
    unpin_snapshot,
)
from rye.cas.gc_types import (
    DEFAULT_RETENTION,
    CompactionResult,
    PruneResult,
    RetentionPolicy,
)


# ---------------------------------------------------------------------------
# Mock CAS — in-memory + on-disk for sweep tests
# ---------------------------------------------------------------------------


class MockCAS:
    def __init__(self, cas_root: Path | None = None):
        self.objects: dict[str, dict] = {}
        self.blobs: dict[str, bytes] = {}
        self.cas_root = cas_root

    def store_object(self, data: dict, root: Path) -> str:
        canonical = json.dumps(data, sort_keys=True, separators=(",", ":"))
        h = hashlib.sha256(canonical.encode()).hexdigest()
        self.objects[h] = data
        shard = root / "objects" / h[:2] / h[2:4]
        shard.mkdir(parents=True, exist_ok=True)
        (shard / f"{h}.json").write_text(json.dumps(data))
        return h

    def get_object(self, hash_hex: str, root: Path) -> dict | None:
        return self.objects.get(hash_hex)

    def store_blob(self, data: bytes, root: Path) -> str:
        h = hashlib.sha256(data).hexdigest()
        self.blobs[h] = data
        shard = root / "blobs" / h[:2] / h[2:4]
        shard.mkdir(parents=True, exist_ok=True)
        (shard / h).write_bytes(data)
        return h

    def get_blob(self, hash_hex: str, root: Path) -> bytes | None:
        return self.blobs.get(hash_hex)

    def has_object(self, hash_hex: str, root: Path) -> bool:
        return hash_hex in self.objects

    def has_blob(self, hash_hex: str, root: Path) -> bool:
        return hash_hex in self.blobs


def _setup_cas_dirs(tmp_path: Path) -> tuple[Path, Path, MockCAS]:
    """Create user_root + cas_root dirs and return (user_root, cas_root, mock)."""
    user_root = tmp_path / "user"
    user_root.mkdir()
    cas_root = user_root / ".ai" / "objects"
    (cas_root / "objects").mkdir(parents=True)
    (cas_root / "blobs").mkdir(parents=True)
    mock = MockCAS(cas_root)
    return user_root, cas_root, mock


def _patch_cas(mock: MockCAS):
    """Return a context manager that patches rye.primitives.cas functions."""
    return patch.multiple(
        "rye.primitives.cas",
        store_object=mock.store_object,
        get_object=mock.get_object,
        store_blob=mock.store_blob,
        get_blob=mock.get_blob,
        has_object=mock.has_object,
        has_blob=mock.has_blob,
    )


# ===========================================================================
# _extract_refs / HASH_FIELDS tests
# ===========================================================================


class TestExtractRefs:
    def test_project_snapshot(self):
        obj = {
            "kind": "project_snapshot",
            "project_manifest_hash": "a" * 64,
            "user_manifest_hash": "b" * 64,
            "parent_hashes": ["c" * 64, "d" * 64],
        }
        refs = _extract_refs(obj)
        assert "a" * 64 in refs
        assert "b" * 64 in refs
        assert "c" * 64 in refs
        assert "d" * 64 in refs

    def test_source_manifest(self):
        obj = {
            "kind": "source_manifest",
            "items": {"tool/a": "e" * 64, "tool/b": "f" * 64},
            "files": {"lockfile": "g" * 64},
        }
        refs = _extract_refs(obj)
        assert set(refs) == {"e" * 64, "f" * 64, "g" * 64}

    def test_execution_snapshot(self):
        obj = {
            "kind": "execution_snapshot",
            "project_manifest_hash": "a" * 64,
            "user_manifest_hash": "b" * 64,
            "state_hash": "c" * 64,
            "node_receipts": ["d" * 64],
        }
        refs = _extract_refs(obj)
        assert len(refs) == 4

    def test_node_receipt(self):
        obj = {
            "kind": "node_receipt",
            "node_input_hash": "a" * 64,
            "node_result_hash": "b" * 64,
        }
        refs = _extract_refs(obj)
        assert set(refs) == {"a" * 64, "b" * 64}

    def test_artifact_index_nested(self):
        obj = {
            "kind": "artifact_index",
            "entries": {
                "call1": {"key1": "a" * 64, "key2": "b" * 64},
                "call2": {"key3": "c" * 64},
            },
        }
        refs = _extract_refs(obj)
        assert set(refs) == {"a" * 64, "b" * 64, "c" * 64}

    def test_unknown_kind_fallback(self):
        obj = {
            "kind": "future_object_type",
            "some_hash": "a" * 64,
            "some_hashes": ["b" * 64, "c" * 64],
            "not_a_hash": "short",
        }
        refs = _extract_refs(obj)
        assert "a" * 64 in refs
        assert "b" * 64 in refs
        assert "c" * 64 in refs

    def test_leaf_nodes_no_refs(self):
        for kind in ("state_snapshot", "node_result", "config_snapshot"):
            obj = {"kind": kind, "data": "something"}
            refs = _extract_refs(obj)
            assert refs == []


class TestExtractAllHashes:
    def test_flat_dict(self):
        data = {"hash1": "a" * 64, "hash2": "b" * 64, "name": "not-a-hash"}
        result = _extract_all_hashes(data)
        assert set(result) == {"a" * 64, "b" * 64}

    def test_list_values(self):
        data = {"hashes": ["a" * 64, "b" * 64]}
        result = _extract_all_hashes(data)
        assert set(result) == {"a" * 64, "b" * 64}

    def test_ignores_short_strings(self):
        data = {"short": "abc", "num": 42}
        result = _extract_all_hashes(data)
        assert result == []


# ===========================================================================
# prune_cache tests
# ===========================================================================


class TestPruneCache:
    def test_prune_old_snapshots(self, tmp_path):
        user_root = tmp_path / "user"
        snap_dir = user_root / "cache" / "snapshots" / "old_snap"
        snap_dir.mkdir(parents=True)
        (snap_dir / "file.txt").write_text("data")
        # Set old mtime
        old_time = time.time() - 86400 * 2
        os.utime(snap_dir, (old_time, old_time))

        result = prune_cache(user_root, max_age_hours=24)
        assert result.cache_entries_deleted == 1
        assert not snap_dir.exists()

    def test_keep_recent_snapshots(self, tmp_path):
        user_root = tmp_path / "user"
        snap_dir = user_root / "cache" / "snapshots" / "new_snap"
        snap_dir.mkdir(parents=True)
        (snap_dir / "file.txt").write_text("data")

        result = prune_cache(user_root, max_age_hours=24)
        assert result.cache_entries_deleted == 0
        assert snap_dir.exists()

    def test_emergency_deletes_all(self, tmp_path):
        user_root = tmp_path / "user"
        snap_dir = user_root / "cache" / "snapshots" / "new_snap"
        snap_dir.mkdir(parents=True)
        (snap_dir / "file.txt").write_text("data")

        result = prune_cache(user_root, emergency=True)
        assert result.cache_entries_deleted == 1
        assert not snap_dir.exists()

    def test_missing_cache_dir(self, tmp_path):
        user_root = tmp_path / "user"
        user_root.mkdir()
        result = prune_cache(user_root)
        assert result.cache_entries_deleted == 0


# ===========================================================================
# prune_executions tests
# ===========================================================================


class TestPruneExecutions:
    def _write_exec(self, exec_dir, thread_id, project, graph, stat, ts):
        exec_dir.mkdir(parents=True, exist_ok=True)
        (exec_dir / f"{thread_id}.json").write_text(json.dumps({
            "project_path": project,
            "graph_id": graph,
            "status": stat,
            "timestamp": ts,
        }))

    def test_keeps_running_and_queued(self, tmp_path):
        user_root = tmp_path / "user"
        ed = user_root / "executions" / "by-id"
        self._write_exec(ed, "t1", "proj", "g1", "running", "2026-01-01T00:00:00Z")
        self._write_exec(ed, "t2", "proj", "g1", "queued", "2026-01-01T00:01:00Z")

        result = prune_executions(user_root, max_success=1, max_failure=1)
        assert result.executions_deleted == 0

    def test_prunes_excess_success(self, tmp_path):
        user_root = tmp_path / "user"
        ed = user_root / "executions" / "by-id"
        for i in range(5):
            self._write_exec(ed, f"t{i}", "proj", "g1", "success", f"2026-01-01T00:{i:02d}:00Z")

        result = prune_executions(user_root, max_success=2, max_failure=2)
        assert result.executions_deleted == 3
        remaining = list(ed.iterdir())
        assert len(remaining) == 2

    def test_prunes_excess_failure(self, tmp_path):
        user_root = tmp_path / "user"
        ed = user_root / "executions" / "by-id"
        for i in range(4):
            self._write_exec(ed, f"f{i}", "proj", "g1", "failed", f"2026-01-01T00:{i:02d}:00Z")

        result = prune_executions(user_root, max_success=10, max_failure=2)
        assert result.executions_deleted == 2

    def test_missing_dir(self, tmp_path):
        result = prune_executions(tmp_path / "nonexistent")
        assert result.executions_deleted == 0


# ===========================================================================
# compact_project_history tests
# ===========================================================================


class TestCompactProjectHistory:
    def _build_chain(self, mock, cas_root, count, source="execution"):
        """Build a linear snapshot chain, return (hashes_newest_first, head_hash)."""
        hashes = []
        prev = None
        for i in range(count):
            ts = (datetime.now(timezone.utc) - timedelta(hours=count - i)).isoformat()
            snap = {
                "kind": "project_snapshot",
                "project_manifest_hash": "m" * 64,
                "user_manifest_hash": "",
                "parent_hashes": [prev] if prev else [],
                "source": source,
                "timestamp": ts,
                "metadata": {},
            }
            h = mock.store_object(snap, cas_root)
            hashes.append(h)
            prev = h
        hashes.reverse()  # newest first
        return hashes, hashes[0]

    def test_compaction_rewrites_chain(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        hashes, head = self._build_chain(mock, cas_root, 20)

        # Write HEAD ref
        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(head)

        policy = RetentionPolicy(manual_pushes=0, daily_checkpoints=1, weekly_checkpoints=0)

        with _patch_cas(mock):
            result = compact_project_history(user_root, proj_dir, cas_root, policy=policy)

        assert result.discarded_count > 0
        assert result.new_head is not None
        assert result.old_head == head

    def test_dry_run_no_changes(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)
        hashes, head = self._build_chain(mock, cas_root, 10)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(head)

        policy = RetentionPolicy(manual_pushes=0, daily_checkpoints=1)

        with _patch_cas(mock):
            result = compact_project_history(user_root, proj_dir, cas_root, policy=policy, dry_run=True)

        assert result.dry_run is True
        assert result.discarded_count > 0
        # HEAD should not have changed
        assert (proj_dir / "head").read_text() == head

    def test_empty_chain_skips(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text("nonexistent" + "0" * 54)

        with _patch_cas(mock):
            result = compact_project_history(user_root, proj_dir, cas_root)

        assert result.skipped is True

    def test_short_chain_no_compaction(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)
        # 2 manual push snapshots — both within daily window and both retained by push policy
        hashes, head = self._build_chain(mock, cas_root, 2, source="push")

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(head)

        policy = RetentionPolicy(manual_pushes=3, daily_checkpoints=7)

        with _patch_cas(mock):
            result = compact_project_history(user_root, proj_dir, cas_root, policy=policy)

        assert result.discarded_count == 0

    def test_pinned_snapshots_preserved(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)
        hashes, head = self._build_chain(mock, cas_root, 20)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(head)

        # Pin a snapshot in the middle
        pinned_hash = hashes[10]
        pin_dir = user_root / "refs" / "pins" / "proj1" / "testpin"
        pin_dir.mkdir(parents=True)
        (pin_dir / "head").write_text(pinned_hash)

        policy = RetentionPolicy(manual_pushes=0, daily_checkpoints=1)

        with _patch_cas(mock):
            result = compact_project_history(user_root, proj_dir, cas_root, policy=policy)

        # Pinned snapshot should be in the retained set (not discarded)
        assert result.retained_count >= 2  # At least HEAD + pin


# ===========================================================================
# mark_reachable tests
# ===========================================================================


class TestMarkReachable:
    def test_follows_project_head_refs(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        leaf = {"kind": "source_manifest", "items": {}, "files": {}}
        leaf_hash = mock.store_object(leaf, cas_root)

        snap = {
            "kind": "project_snapshot",
            "project_manifest_hash": leaf_hash,
            "user_manifest_hash": "",
            "parent_hashes": [],
        }
        snap_hash = mock.store_object(snap, cas_root)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(snap_hash)

        with _patch_cas(mock):
            reachable = mark_reachable(user_root, cas_root)

        assert snap_hash in reachable
        assert leaf_hash in reachable

    def test_follows_pin_refs(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        pinned_obj = {"kind": "project_snapshot", "project_manifest_hash": "", "parent_hashes": []}
        pinned_hash = mock.store_object(pinned_obj, cas_root)

        pin_dir = user_root / "refs" / "pins" / "proj1" / "pin1"
        pin_dir.mkdir(parents=True)
        (pin_dir / "head").write_text(pinned_hash)

        with _patch_cas(mock):
            reachable = mark_reachable(user_root, cas_root)

        assert pinned_hash in reachable

    def test_follows_execution_snapshot_hashes(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        exec_snap = {"kind": "execution_snapshot", "state_hash": "", "node_receipts": []}
        exec_hash = mock.store_object(exec_snap, cas_root)

        exec_dir = user_root / "executions" / "by-id"
        exec_dir.mkdir(parents=True)
        (exec_dir / "thread1.json").write_text(json.dumps({
            "execution_snapshot_hash": exec_hash,
        }))

        with _patch_cas(mock):
            reachable = mark_reachable(user_root, cas_root)

        assert exec_hash in reachable

    def test_follows_inflight_epoch_roots(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        obj = {"kind": "project_snapshot", "project_manifest_hash": "", "parent_hashes": []}
        obj_hash = mock.store_object(obj, cas_root)

        inflight_dir = user_root / "inflight"
        inflight_dir.mkdir(parents=True)
        (inflight_dir / "epoch1.json").write_text(json.dumps({
            "root_hashes": [obj_hash],
            "created_at": datetime.now(timezone.utc).isoformat(),
        }))

        with _patch_cas(mock):
            reachable = mark_reachable(user_root, cas_root)

        assert obj_hash in reachable

    def test_iterative_handles_deep_chain(self, tmp_path):
        """Verify no recursion error with a deep chain."""
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        # Build chain of 500 objects
        prev = None
        for _ in range(500):
            snap = {
                "kind": "project_snapshot",
                "project_manifest_hash": "",
                "parent_hashes": [prev] if prev else [],
            }
            prev = mock.store_object(snap, cas_root)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(prev)

        with _patch_cas(mock):
            reachable = mark_reachable(user_root, cas_root)

        assert len(reachable) == 500


# ===========================================================================
# sweep tests
# ===========================================================================


class TestSweep:
    def test_deletes_unreachable_objects(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        reachable_obj = {"kind": "keep"}
        reachable_hash = mock.store_object(reachable_obj, cas_root)

        unreachable_obj = {"kind": "garbage"}
        unreachable_hash = mock.store_object(unreachable_obj, cas_root)

        # Set old mtime on unreachable
        obj_path = cas_root / "objects" / unreachable_hash[:2] / unreachable_hash[2:4] / f"{unreachable_hash}.json"
        old_time = time.time() - 7200
        os.utime(obj_path, (old_time, old_time))

        result = sweep(user_root, cas_root, {reachable_hash}, grace_seconds=3600)
        assert result.deleted_objects == 1
        assert not obj_path.exists()

    def test_respects_grace_window(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        obj = {"kind": "new_object"}
        obj_hash = mock.store_object(obj, cas_root)
        # File is brand new — should not be deleted

        result = sweep(user_root, cas_root, set(), grace_seconds=3600)
        assert result.deleted_objects == 0

    def test_dry_run_counts_but_no_delete(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        obj = {"kind": "garbage"}
        obj_hash = mock.store_object(obj, cas_root)
        obj_path = cas_root / "objects" / obj_hash[:2] / obj_hash[2:4] / f"{obj_hash}.json"
        old_time = time.time() - 7200
        os.utime(obj_path, (old_time, old_time))

        result = sweep(user_root, cas_root, set(), grace_seconds=3600, dry_run=True)
        assert result.deleted_objects == 1
        assert result.dry_run is True
        assert obj_path.exists()  # File should still exist

    def test_epoch_aware_cutoff(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        obj = {"kind": "in_flight"}
        obj_hash = mock.store_object(obj, cas_root)
        obj_path = cas_root / "objects" / obj_hash[:2] / obj_hash[2:4] / f"{obj_hash}.json"
        # Set mtime to 2 hours ago
        old_time = time.time() - 7200
        os.utime(obj_path, (old_time, old_time))

        # Create an epoch that started 30 min ago — cutoff should be epoch start
        inflight_dir = user_root / "inflight"
        inflight_dir.mkdir(parents=True)
        epoch_start = datetime.now(timezone.utc) - timedelta(minutes=30)
        (inflight_dir / "epoch1.json").write_text(json.dumps({
            "created_at": epoch_start.isoformat(),
        }))

        # With grace_seconds=3600 (1h ago), the min(1h_ago, epoch_30min_ago)
        # = 1h ago, so the 2h-old object SHOULD be deleted
        result = sweep(user_root, cas_root, set(), grace_seconds=3600)
        assert result.deleted_objects == 1


# ===========================================================================
# pin tests
# ===========================================================================


class TestPinSnapshot:
    def test_creates_pin_ref(self, tmp_path):
        user_root = tmp_path / "user"
        user_root.mkdir()

        pin_id = pin_snapshot(user_root, "proj1", "a" * 64, "v1.0")
        assert pin_id
        pin_head = user_root / "refs" / "pins" / "proj1" / pin_id / "head"
        assert pin_head.read_text().strip() == "a" * 64
        meta = json.loads((pin_head.parent / "meta.json").read_text())
        assert meta["label"] == "v1.0"

    def test_unpin_removes_ref(self, tmp_path):
        user_root = tmp_path / "user"
        user_root.mkdir()

        pin_id = pin_snapshot(user_root, "proj1", "a" * 64, "v1.0")
        assert unpin_snapshot(user_root, "proj1", pin_id) is True
        assert not (user_root / "refs" / "pins" / "proj1" / pin_id).exists()

    def test_unpin_nonexistent(self, tmp_path):
        user_root = tmp_path / "user"
        user_root.mkdir()
        assert unpin_snapshot(user_root, "proj1", "nope") is False


# ===========================================================================
# run_gc orchestrator tests
# ===========================================================================


class TestRunGC:
    def test_full_gc_orchestrates_all_phases(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        # Create a simple project with 1 snapshot
        snap = {
            "kind": "project_snapshot",
            "project_manifest_hash": "",
            "user_manifest_hash": "",
            "parent_hashes": [],
            "source": "push",
            "timestamp": datetime.now(timezone.utc).isoformat(),
            "metadata": {},
        }
        snap_hash = mock.store_object(snap, cas_root)

        proj_dir = user_root / "refs" / "projects" / "proj1"
        proj_dir.mkdir(parents=True)
        (proj_dir / "head").write_text(snap_hash)

        # Create some garbage
        garbage = {"kind": "orphan"}
        garbage_hash = mock.store_object(garbage, cas_root)
        garbage_path = cas_root / "objects" / garbage_hash[:2] / garbage_hash[2:4] / f"{garbage_hash}.json"
        old_time = time.time() - 7200
        os.utime(garbage_path, (old_time, old_time))

        with _patch_cas(mock):
            result = run_gc(user_root, cas_root, use_incremental=False)

        assert result.total_freed_bytes >= 0
        assert result.sweep.deleted_objects >= 1
        assert result.error is None

    def test_lock_failure_returns_partial(self, tmp_path):
        user_root, cas_root, mock = _setup_cas_dirs(tmp_path)

        # Pre-acquire the lock so run_gc can't get it
        from rye.cas.gc_lock import acquire
        lock = acquire(user_root, "other-node", ttl_seconds=300)
        assert lock is not None

        with _patch_cas(mock):
            result = run_gc(user_root, cas_root)

        assert result.sweep.skipped is True
        assert result.sweep.skip_reason == "gc_lock_held_by_another_node"
