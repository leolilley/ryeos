"""Tests for rye.cas.gc_incremental, rye.cas.gc_lock, and rye.cas.gc_epochs."""

import hashlib
import json
import os
import time
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.cas.gc_epochs import (
    cleanup_stale_epochs,
    complete_epoch,
    list_active_epochs,
    load_epoch,
    oldest_epoch_time,
    register_epoch,
)
from rye.cas.gc_incremental import (
    _collect_durable_root_hashes,
    invalidate_gc_state,
    load_gc_state,
    mark_reachable_incremental,
    save_gc_state,
)
from rye.cas.gc_lock import acquire, is_locked, read_lock, release, update_phase
from rye.cas.gc_types import GCState


# ---------------------------------------------------------------------------
# Mock CAS for incremental mark tests
# ---------------------------------------------------------------------------


class MockCAS:
    def __init__(self):
        self.objects = {}
        self.blobs = {}

    def store_object(self, data, root):
        h = hashlib.sha256(
            json.dumps(data, sort_keys=True, separators=(",", ":")).encode()
        ).hexdigest()
        self.objects[h] = data
        return h

    def get_object(self, h, root):
        return self.objects.get(h)

    def store_blob(self, data, root):
        h = hashlib.sha256(data).hexdigest()
        self.blobs[h] = data
        return h

    def get_blob(self, h, root):
        return self.blobs.get(h)


# ===========================================================================
# GC State persistence (gc_incremental.py)
# ===========================================================================


class TestGCState:
    def test_save_and_load_round_trip(self, tmp_path):
        state = GCState(
            last_gc_at="2026-01-01T00:00:00+00:00",
            last_full_gc_at="2025-12-01T00:00:00+00:00",
            reachable_hashes_blob="abc123",
            reachable_count=42,
            objects_at_last_gc=100,
            generation=3,
            invalidated=False,
        )
        save_gc_state(tmp_path, cas_root=None, state=state)
        loaded = load_gc_state(tmp_path)

        assert loaded is not None
        assert loaded.last_gc_at == state.last_gc_at
        assert loaded.last_full_gc_at == state.last_full_gc_at
        assert loaded.reachable_hashes_blob == state.reachable_hashes_blob
        assert loaded.reachable_count == state.reachable_count
        assert loaded.objects_at_last_gc == state.objects_at_last_gc
        assert loaded.generation == state.generation
        assert loaded.invalidated == state.invalidated

    def test_load_missing_file_returns_none(self, tmp_path):
        assert load_gc_state(tmp_path) is None

    def test_load_corrupt_file_returns_none(self, tmp_path):
        (tmp_path / "gc_state.json").write_text("not valid json{{{", encoding="utf-8")
        assert load_gc_state(tmp_path) is None

    def test_invalidate_sets_flag(self, tmp_path):
        state = GCState(invalidated=False, generation=1)
        save_gc_state(tmp_path, cas_root=None, state=state)

        invalidate_gc_state(tmp_path)

        loaded = load_gc_state(tmp_path)
        assert loaded is not None
        assert loaded.invalidated is True

    def test_invalidate_no_state_is_noop(self, tmp_path):
        # Should not raise and should not create a file
        invalidate_gc_state(tmp_path)
        assert not (tmp_path / "gc_state.json").exists()


# ===========================================================================
# Durable root collection
# ===========================================================================


class TestCollectDurableRoots:
    def test_collects_project_heads(self, tmp_path):
        head = tmp_path / "refs" / "projects" / "proj1" / "head"
        head.parent.mkdir(parents=True)
        head.write_text("aaa111", encoding="utf-8")

        roots = _collect_durable_root_hashes(tmp_path)
        assert "aaa111" in roots

    def test_collects_user_space_head(self, tmp_path):
        head = tmp_path / "refs" / "user-space" / "head"
        head.parent.mkdir(parents=True)
        head.write_text("bbb222", encoding="utf-8")

        roots = _collect_durable_root_hashes(tmp_path)
        assert "bbb222" in roots

    def test_collects_pin_heads(self, tmp_path):
        head = tmp_path / "refs" / "pins" / "proj1" / "pin1" / "head"
        head.parent.mkdir(parents=True)
        head.write_text("ccc333", encoding="utf-8")

        roots = _collect_durable_root_hashes(tmp_path)
        assert "ccc333" in roots

    def test_collects_execution_hashes(self, tmp_path):
        exec_dir = tmp_path / "executions" / "by-id"
        exec_dir.mkdir(parents=True)
        exec_data = {
            "snapshot_hash": "snap1",
            "execution_snapshot_hash": "exec1",
            "runtime_outputs_bundle_hash": "bundle1",
        }
        (exec_dir / "thread1.json").write_text(
            json.dumps(exec_data), encoding="utf-8"
        )

        roots = _collect_durable_root_hashes(tmp_path)
        assert "snap1" in roots
        assert "exec1" in roots
        assert "bundle1" in roots

    def test_no_epochs_included(self, tmp_path):
        inflight_dir = tmp_path / "inflight"
        inflight_dir.mkdir(parents=True)
        epoch_data = {
            "epoch_id": "e1",
            "node_id": "n1",
            "user_id": "u1",
            "root_hashes": ["ephemeral_hash_1"],
            "created_at": "2026-01-01T00:00:00+00:00",
        }
        (inflight_dir / "epoch1.json").write_text(
            json.dumps(epoch_data), encoding="utf-8"
        )

        roots = _collect_durable_root_hashes(tmp_path)
        assert "ephemeral_hash_1" not in roots


# ===========================================================================
# Incremental mark
# ===========================================================================


class TestMarkReachableIncremental:
    def _setup_mock_cas(self):
        mock = MockCAS()
        return mock

    def test_incremental_adds_new_roots(self, tmp_path):
        mock = self._setup_mock_cas()
        cas_root = tmp_path / "cas"
        cas_root.mkdir()

        # Create an object graph: child_obj <- root_obj
        child_obj = {"kind": "blob_ref", "data": "leaf"}
        child_hash = mock.store_object(child_obj, cas_root)
        root_obj = {"kind": "unknown", "child_hash": child_hash}
        root_hash = mock.store_object(root_obj, cas_root)

        # Cached durable set from previous GC: just one old hash
        old_hash = "old" + "0" * 60
        cached_set = [old_hash]
        cached_blob = json.dumps(cached_set).encode()
        blob_hash = mock.store_blob(cached_blob, cas_root)

        prev_state = GCState(
            reachable_hashes_blob=blob_hash,
            reachable_count=1,
            invalidated=False,
        )

        # Create a new project ref pointing to root_hash
        head = tmp_path / "refs" / "projects" / "proj1" / "head"
        head.parent.mkdir(parents=True)
        head.write_text(root_hash, encoding="utf-8")

        with patch("rye.cas.gc_incremental.cas.get_object", side_effect=mock.get_object):
            with patch("rye.cas.gc_incremental.cas.get_blob", side_effect=mock.get_blob):
                result, was_full = mark_reachable_incremental(tmp_path, cas_root, prev_state)

        assert not was_full
        assert old_hash in result
        assert root_hash in result
        assert child_hash in result

    def test_invalidated_falls_back_to_full(self, tmp_path):
        cas_root = tmp_path / "cas"
        cas_root.mkdir()
        prev_state = GCState(invalidated=True)

        full_set = {"full_hash_1", "full_hash_2"}
        with patch("rye.cas.gc.mark_reachable", return_value=full_set) as mock_full:
            result, was_full = mark_reachable_incremental(tmp_path, cas_root, prev_state)

        assert was_full is True
        assert result == full_set
        mock_full.assert_called_once_with(tmp_path, cas_root)

    def test_missing_blob_falls_back_to_full(self, tmp_path):
        cas_root = tmp_path / "cas"
        cas_root.mkdir()
        prev_state = GCState(
            reachable_hashes_blob="nonexistent_blob_hash",
            invalidated=False,
        )

        full_set = {"full_hash_3"}
        with patch("rye.cas.gc_incremental.cas.get_blob", return_value=None):
            with patch("rye.cas.gc.mark_reachable", return_value=full_set):
                result, was_full = mark_reachable_incremental(tmp_path, cas_root, prev_state)

        assert was_full is True
        assert result == full_set

    def test_ephemeral_computed_fresh(self, tmp_path):
        mock = self._setup_mock_cas()
        cas_root = tmp_path / "cas"
        cas_root.mkdir()

        # Create an ephemeral object
        ephemeral_obj = {"kind": "blob_ref", "data": "ephemeral"}
        ephemeral_hash = mock.store_object(ephemeral_obj, cas_root)

        # Write an inflight epoch referencing the ephemeral hash
        inflight_dir = tmp_path / "inflight"
        inflight_dir.mkdir(parents=True)
        epoch_data = {
            "epoch_id": "e1",
            "node_id": "n1",
            "user_id": "u1",
            "root_hashes": [ephemeral_hash],
            "created_at": "2026-01-01T00:00:00+00:00",
        }
        (inflight_dir / "e1.json").write_text(json.dumps(epoch_data), encoding="utf-8")

        # Empty cached durable set
        cached_blob = json.dumps([]).encode()
        blob_hash = mock.store_blob(cached_blob, cas_root)

        prev_state = GCState(
            reachable_hashes_blob=blob_hash,
            reachable_count=0,
            invalidated=False,
        )

        with patch("rye.cas.gc_incremental.cas.get_object", side_effect=mock.get_object):
            with patch("rye.cas.gc_incremental.cas.get_blob", side_effect=mock.get_blob):
                result, was_full = mark_reachable_incremental(tmp_path, cas_root, prev_state)

        assert not was_full
        # Ephemeral hash must be in the protected set
        assert ephemeral_hash in result


# ===========================================================================
# Distributed Lock (gc_lock.py)
# ===========================================================================


class TestDistributedLock:
    def test_acquire_and_release(self, tmp_path):
        lock = acquire(tmp_path, "node-A")
        assert lock is not None
        assert lock.node_id == "node-A"
        assert lock.phase == "init"

        ok = release(tmp_path, "node-A")
        assert ok is True
        assert not (tmp_path / ".gc-lock").exists()

    def test_acquire_twice_same_node_extends(self, tmp_path):
        lock1 = acquire(tmp_path, "node-A")
        assert lock1 is not None

        lock2 = acquire(tmp_path, "node-A")
        assert lock2 is not None
        # Re-acquire should succeed (same node extends TTL)
        assert lock2.node_id == "node-A"

    def test_acquire_blocked_by_other_node(self, tmp_path):
        lock_a = acquire(tmp_path, "node-A")
        assert lock_a is not None

        lock_b = acquire(tmp_path, "node-B")
        assert lock_b is None

    def test_expired_lock_reclaimable(self, tmp_path):
        # Acquire with ttl=0 so it expires immediately
        lock1 = acquire(tmp_path, "node-A", ttl_seconds=0)
        assert lock1 is not None

        # A different node should be able to reclaim
        lock2 = acquire(tmp_path, "node-B")
        assert lock2 is not None
        assert lock2.node_id == "node-B"
        assert lock2.generation >= 1

    def test_update_phase(self, tmp_path):
        lock = acquire(tmp_path, "node-A")
        assert lock is not None

        ok = update_phase(tmp_path, "node-A", "compact")
        assert ok is True

        current = read_lock(tmp_path)
        assert current is not None
        assert current.phase == "compact"

    def test_is_locked_true_when_held(self, tmp_path):
        acquire(tmp_path, "node-A")
        assert is_locked(tmp_path) is True

    def test_is_locked_false_when_expired(self, tmp_path):
        acquire(tmp_path, "node-A", ttl_seconds=0)
        time.sleep(0.01)
        assert is_locked(tmp_path) is False

    def test_release_wrong_node_fails(self, tmp_path):
        acquire(tmp_path, "node-A")
        ok = release(tmp_path, "node-B")
        assert ok is False
        # Lock should still exist
        assert is_locked(tmp_path) is True


# ===========================================================================
# Writer Epochs (gc_epochs.py)
# ===========================================================================


class TestWriterEpochs:
    def test_register_and_complete(self, tmp_path):
        epoch_id = register_epoch(tmp_path, "n1", "u1", ["h1", "h2"])
        assert epoch_id
        epoch_file = tmp_path / "inflight" / f"{epoch_id}.json"
        assert epoch_file.exists()

        complete_epoch(tmp_path, epoch_id)
        assert not epoch_file.exists()

    def test_list_active_epochs(self, tmp_path):
        ids = [
            register_epoch(tmp_path, "n1", "u1", [f"h{i}"])
            for i in range(3)
        ]
        active = list_active_epochs(tmp_path)
        assert len(active) == 3
        active_ids = {e.epoch_id for e in active}
        for eid in ids:
            assert eid in active_ids

    def test_load_epoch(self, tmp_path):
        epoch_id = register_epoch(tmp_path, "n1", "u1", ["h1"])
        loaded = load_epoch(tmp_path, epoch_id)
        assert loaded is not None
        assert loaded.epoch_id == epoch_id
        assert loaded.node_id == "n1"
        assert loaded.user_id == "u1"
        assert loaded.root_hashes == ["h1"]

    def test_cleanup_stale(self, tmp_path):
        epoch_id = register_epoch(tmp_path, "n1", "u1", ["h1"])
        epoch_file = tmp_path / "inflight" / f"{epoch_id}.json"

        # Set mtime to 1 hour ago
        old_time = time.time() - 3600
        os.utime(epoch_file, (old_time, old_time))

        removed = cleanup_stale_epochs(tmp_path, max_age_seconds=60)
        assert removed == 1
        assert not epoch_file.exists()

    def test_oldest_epoch_time(self, tmp_path):
        e1 = register_epoch(tmp_path, "n1", "u1", ["h1"])
        time.sleep(0.05)
        e2 = register_epoch(tmp_path, "n2", "u2", ["h2"])

        epoch1 = load_epoch(tmp_path, e1)
        epoch2 = load_epoch(tmp_path, e2)

        oldest = oldest_epoch_time(tmp_path)
        assert oldest is not None
        # Oldest should match the first epoch's created_at timestamp
        from datetime import datetime

        e1_ts = datetime.fromisoformat(epoch1.created_at).timestamp()
        assert oldest == pytest.approx(e1_ts, abs=1.0)

    def test_complete_missing_epoch_is_safe(self, tmp_path):
        # Should not raise
        complete_epoch(tmp_path, "nonexistent-epoch-id")
