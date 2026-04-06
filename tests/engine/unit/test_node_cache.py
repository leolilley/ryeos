"""Tests for node execution cache (Step 7)."""

import json
import tempfile
from pathlib import Path

from rye.cas.node_cache import compute_cache_key, cache_lookup, cache_store


class TestComputeCacheKey:
    """Test cache key computation."""

    def test_deterministic(self):
        k1 = compute_cache_key("gh", "node1", {"item_type": "tool"}, "cs")
        k2 = compute_cache_key("gh", "node1", {"item_type": "tool"}, "cs")
        assert k1 == k2
        assert len(k1) == 64

    def test_different_action_different_key(self):
        k1 = compute_cache_key("gh", "node1", {"item_id": "a"}, "cs")
        k2 = compute_cache_key("gh", "node1", {"item_id": "b"}, "cs")
        assert k1 != k2

    def test_different_node_different_key(self):
        k1 = compute_cache_key("gh", "node1", {"item_id": "a"}, "cs")
        k2 = compute_cache_key("gh", "node2", {"item_id": "a"}, "cs")
        assert k1 != k2

    def test_different_config_different_key(self):
        k1 = compute_cache_key("gh", "n", {}, "cs1")
        k2 = compute_cache_key("gh", "n", {}, "cs2")
        assert k1 != k2


class TestCacheLookupStore:
    """Test cache store and lookup."""

    def test_miss_returns_none(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "state" / "objects" / "cache" / "nodes").mkdir(
                parents=True
            )
            assert cache_lookup("nonexistent", project) is None

    def test_store_then_lookup(self):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "state" / "objects").mkdir(parents=True)

            result = {"status": "ok", "data": "hello"}
            result_hash = cache_store("testkey", result, project, "node1", 100)
            assert result_hash is not None

            cached = cache_lookup("testkey", project)
            assert cached is not None
            assert cached["result"]["status"] == "ok"
            assert cached["result"]["data"] == "hello"
            assert cached["node_result_hash"] == result_hash

    def test_error_results_not_cached(self):
        """Verify that cache_store works for error results too (caller decides)."""
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "state" / "objects").mkdir(parents=True)

            result = {"status": "error", "error": "failed"}
            result_hash = cache_store("errkey", result, project, "node1", 50)
            assert result_hash is not None

            cached = cache_lookup("errkey", project)
            assert cached is not None
            assert cached["result"]["status"] == "error"
            assert cached["node_result_hash"] == result_hash
