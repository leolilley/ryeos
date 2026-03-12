"""Tests for rye.cas.checkout — snapshot caching and execution spaces."""

import threading
from pathlib import Path
from unittest.mock import patch

import pytest

from rye.cas.checkout import ensure_snapshot_cached


class TestConcurrentSnapshotCache:
    def test_concurrent_snapshot_cache_no_corruption(self, tmp_path):
        """4 threads calling ensure_snapshot_cached with the same hash must all succeed."""
        cas_root = tmp_path / "objects"
        cas_root.mkdir()
        cache_root = tmp_path / "cache"
        cache_root.mkdir()
        (cache_root / "snapshots").mkdir()

        snapshot_hash = "a" * 64
        manifest_hash = "b" * 64

        fake_snapshot = {"project_manifest_hash": manifest_hash}
        fake_manifest = {"items": {}, "files": {}}

        def mock_get_object(h, root):
            if h == snapshot_hash:
                return fake_snapshot
            if h == manifest_hash:
                return fake_manifest
            return None

        results = [None] * 4
        errors = [None] * 4
        barrier = threading.Barrier(4)

        def worker(idx):
            try:
                barrier.wait(timeout=5)
                results[idx] = ensure_snapshot_cached(snapshot_hash, cas_root, cache_root)
            except Exception as e:
                errors[idx] = e

        with patch("rye.cas.checkout.cas.get_object", side_effect=mock_get_object):
            with patch("rye.cas.checkout.materialize_manifest_dict"):
                threads = [threading.Thread(target=worker, args=(i,)) for i in range(4)]
                for t in threads:
                    t.start()
                for t in threads:
                    t.join(timeout=10)

        for i, err in enumerate(errors):
            assert err is None, f"Thread {i} raised: {err}"

        expected = cache_root / "snapshots" / snapshot_hash
        for i, result in enumerate(results):
            assert result == expected, f"Thread {i} returned {result}, expected {expected}"
        assert expected.exists()
        assert (expected / ".snapshot_complete").exists()
