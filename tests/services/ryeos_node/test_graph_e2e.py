"""E2E tests for graph execution via ryeos-node server.

Covers:
  - test_push_execute_pull_graph: Push → execute graph → pull snapshot + receipts
  - test_cached_node_execution: Graph with cache hit → receipt shows cache_hit=true

Uses the walker's `execute()` function directly with _dispatch_action mocked
to avoid needing real tools. Verifies CAS objects (snapshots, receipts, cache).
"""

import hashlib
import importlib.util
import json
import sys
import time
from pathlib import Path
from unittest.mock import AsyncMock, patch

import pytest

from lillux.primitives import cas
from rye.cas.objects import NodeReceipt, NodeResult
from rye.cas.store import cas_root, read_ref
from rye.constants import AI_DIR

# ---------------------------------------------------------------------------
# Load walker module from bundle (path contains .ai/)
# ---------------------------------------------------------------------------

from conftest import get_bundle_path
_WALKER_DIR = get_bundle_path("core", "tools/rye/core/runtimes/state-graph")

# Load walker once at module level so patch.object targets the same module
if str(_WALKER_DIR) not in sys.path:
    sys.path.insert(0, str(_WALKER_DIR))

_spec = importlib.util.spec_from_file_location("walker_e2e", _WALKER_DIR / "walker.py")
walker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(walker)


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def graph_project(tmp_path, _setup_user_space, monkeypatch):
    """Create a minimal project with .ai/objects for graph execution."""
    project = tmp_path / "project"
    project.mkdir()
    (project / AI_DIR / "objects").mkdir(parents=True)
    (project / AI_DIR / "agent" / "graphs").mkdir(parents=True)
    monkeypatch.delenv("RYE_SIGNING_KEY_DIR", raising=False)
    return project


# ---------------------------------------------------------------------------
# Graph configs (interpolation uses ${...} syntax, not {{ ... }})
# ---------------------------------------------------------------------------

def _simple_graph():
    """Two-node graph: greet → done (return)."""
    return {
        "_item_id": "test/simple",
        "permissions": ["rye.execute.tool.*"],
        "config": {
            "start": "greet",
            "nodes": {
                "greet": {
                    "action": {
                        "primary": "execute",
                        "item_type": "tool",
                        "item_id": "test/echo",
                        "params": {"message": "hello"},
                    },
                    "assign": {
                        "greeting": "${result.body.message}",
                    },
                    "next": "done",
                },
                "done": {
                    "type": "return",
                    "output": {
                        "greeting": "${state.greeting}",
                    },
                },
            },
        },
    }


def _cached_graph():
    """Graph with cache: true on the compute node."""
    return {
        "_item_id": "test/cached",
        "permissions": ["rye.execute.tool.*"],
        "config": {
            "start": "compute",
            "nodes": {
                "compute": {
                    "cache_result": True,
                    "action": {
                        "primary": "execute",
                        "item_type": "tool",
                        "item_id": "test/expensive",
                        "params": {"x": 42},
                    },
                    "assign": {
                        "result_val": "${result.body.value}",
                    },
                    "next": "done",
                },
                "done": {
                    "type": "return",
                    "output": {
                        "value": "${state.result_val}",
                    },
                },
            },
        },
    }


# ---------------------------------------------------------------------------
# Tests
# ---------------------------------------------------------------------------


class TestPushExecutePullGraph:
    """Push → execute graph → pull snapshot + receipts."""

    @pytest.mark.asyncio
    async def test_graph_produces_snapshot_and_receipts(self, graph_project):
        """Execute a 2-node graph → verify execution_snapshot with node_receipts."""
        mock_result = {
            "status": "ok",
            "body": {"message": "hello world"},
        }

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch:
            mock_dispatch.return_value = mock_result
            result = await walker.execute(
                _simple_graph(), {}, str(graph_project),
            )

        assert result["success"] is True, f"Graph failed: {result.get('error')}"
        assert result["steps"] == 2  # greet + done

        # Verify execution_snapshot in CAS via graph ref
        run_id = result["graph_run_id"]
        ref_path = graph_project / AI_DIR / "objects" / "refs" / "graphs" / f"{run_id}.json"
        assert ref_path.exists()

        snapshot_hash = read_ref(ref_path)
        assert snapshot_hash is not None

        root = cas_root(graph_project)
        snapshot = cas.get_object(snapshot_hash, root)
        assert snapshot is not None
        assert snapshot["kind"] == "execution_snapshot"
        assert snapshot["graph_id"] == "test/simple"
        assert snapshot["status"] == "completed"
        assert snapshot["step"] == 2
        assert "system_version" in snapshot

        # Verify state_hash resolves
        state_obj = cas.get_object(snapshot["state_hash"], root)
        assert state_obj is not None
        assert state_obj["kind"] == "state_snapshot"
        assert state_obj["state"]["greeting"] == "hello world"

        # Verify node_receipts exist and are valid
        assert len(snapshot["node_receipts"]) >= 1
        for receipt_hash in snapshot["node_receipts"]:
            receipt = cas.get_object(receipt_hash, root)
            assert receipt is not None
            assert receipt["kind"] == "node_receipt"
            assert "node_result_hash" in receipt
            assert "elapsed_ms" in receipt
            assert "timestamp" in receipt

            # Dereference node_result_hash → must resolve in CAS (P0.2 regression guard)
            node_result = cas.get_object(receipt["node_result_hash"], root)
            assert node_result is not None, (
                f"node_result_hash {receipt['node_result_hash'][:16]} does not resolve in CAS"
            )
            assert node_result["kind"] == "node_result"

    @pytest.mark.asyncio
    async def test_graph_output_interpolated(self, graph_project):
        """Return node output is interpolated from state."""
        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch:
            mock_dispatch.return_value = {
                "status": "ok",
                "body": {"message": "greetings"},
            }
            result = await walker.execute(
                _simple_graph(), {}, str(graph_project),
            )

        assert result["success"] is True, f"Graph failed: {result.get('error')}"
        assert result["output"]["greeting"] == "greetings"


class TestCachedNodeExecution:
    """Graph with cache hit — receipt shows cache_hit=true."""

    @pytest.mark.asyncio
    async def test_second_run_hits_cache(self, graph_project):
        """Run graph twice with same inputs → second run shows cache_hit=true."""
        import time as _time

        mock_result = {
            "status": "ok",
            "body": {"value": 100},
        }

        # First run — cache miss, stores result
        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch:
            mock_dispatch.return_value = mock_result
            result1 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )

        assert result1["success"] is True, f"Run 1 failed: {result1.get('error')}"

        root = cas_root(graph_project)

        # Verify cache was stored
        cache_dir = graph_project / AI_DIR / "objects" / "cache" / "nodes"
        assert cache_dir.exists(), "Cache dir not created after first run"
        cache_files = list(cache_dir.iterdir())
        assert len(cache_files) >= 1, "No cache entries after first run"

        # Find receipts from first run
        ref1 = read_ref(
            graph_project / AI_DIR / "objects" / "refs" / "graphs"
            / f"{result1['graph_run_id']}.json"
        )
        snap1 = cas.get_object(ref1, root)
        receipts1 = snap1["node_receipts"]
        assert len(receipts1) >= 1
        receipt1 = cas.get_object(receipts1[0], root)
        assert receipt1["cache_hit"] is False

        # Verify the cache file is actually readable by cache_lookup directly
        from rye.cas.node_cache import cache_lookup, compute_cache_key
        from rye.cas.config_snapshot import compute_agent_config_snapshot
        import hashlib as _hashlib

        cfg = _cached_graph()["config"]
        graph_hash_val = _hashlib.sha256(
            json.dumps(cfg, sort_keys=True, separators=(",", ":"), default=str).encode()
        ).hexdigest()
        config_snap_hash, _ = compute_agent_config_snapshot(str(graph_project))

        # Reconstruct the action as the walker would after interpolation
        action = {
            "primary": "execute",
            "item_type": "tool",
            "item_id": "test/expensive",
            "params": {"x": 42},
        }
        cache_key = compute_cache_key(
            graph_hash=graph_hash_val,
            node_name="compute",
            interpolated_action=action,
            lockfile_hash=None,
            config_snapshot_hash=config_snap_hash,
        )
        cached = cache_lookup(cache_key, graph_project)
        assert cached is not None, f"cache_lookup returned None for key {cache_key[:16]}"

        # Wait 1s so auto-generated graph_run_id (uses int(time.time())) differs
        _time.sleep(1)

        # Second run — same config → cache hit (dispatch NOT called for compute)
        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch2:
            mock_dispatch2.return_value = mock_result
            result2 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )

        assert result2["success"] is True, f"Run 2 failed: {result2.get('error')}"
        # The compute node action should NOT have been dispatched — it was cached.
        # Infra hooks (after_step emitter) may still call _dispatch_action,
        # so filter to only calls matching the compute node's action item_id.
        compute_calls = [
            c for c in mock_dispatch2.call_args_list
            if c.args and isinstance(c.args[0], dict)
            and c.args[0].get("item_id") == "test/expensive"
        ]
        assert len(compute_calls) == 0, (
            f"Expected 0 dispatch calls for compute node (cached), got {len(compute_calls)}"
        )

        # Find receipts from second run
        ref2 = read_ref(
            graph_project / AI_DIR / "objects" / "refs" / "graphs"
            / f"{result2['graph_run_id']}.json"
        )
        snap2 = cas.get_object(ref2, root)
        receipts2 = snap2["node_receipts"]
        assert len(receipts2) >= 1
        receipt2 = cas.get_object(receipts2[0], root)
        assert receipt2["cache_hit"] is True

    @pytest.mark.asyncio
    async def test_cached_result_matches_original(self, graph_project):
        """Cached run produces same output as original."""
        import time as _time

        mock_result = {
            "status": "ok",
            "body": {"value": 42},
        }

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch:
            mock_dispatch.return_value = mock_result
            result1 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )

        assert result1["success"] is True, f"Run 1 failed: {result1.get('error')}"

        _time.sleep(1)  # ensure different graph_run_id

        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch2:
            mock_dispatch2.return_value = mock_result
            result2 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )

        assert result2["success"] is True, f"Run 2 failed: {result2.get('error')}"
        assert result1["output"] == result2["output"]
        assert result1["output"]["value"] == 42

    @pytest.mark.asyncio
    async def test_corrupted_cache_falls_back_to_recompute(self, graph_project):
        """Corrupted cache file → walker recomputes instead of failing."""
        mock_result = {
            "status": "ok",
            "body": {"value": 99},
        }

        # First run — populate cache
        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch:
            mock_dispatch.return_value = mock_result
            result1 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )
        assert result1["success"] is True

        # Corrupt all cache files
        cache_dir = graph_project / AI_DIR / "objects" / "cache" / "nodes"
        for cache_file in cache_dir.iterdir():
            cache_file.write_text("corrupted json {{{")

        import time as _time
        _time.sleep(1)

        # Second run — corrupted cache → should recompute, not fail
        with patch.object(walker, "_dispatch_action", new_callable=AsyncMock) as mock_dispatch2:
            mock_dispatch2.return_value = mock_result
            result2 = await walker.execute(
                _cached_graph(), {}, str(graph_project),
            )

        assert result2["success"] is True, f"Run 2 failed: {result2.get('error')}"
        # Dispatch SHOULD have been called since cache is corrupted
        compute_calls = [
            c for c in mock_dispatch2.call_args_list
            if c.args and isinstance(c.args[0], dict)
            and c.args[0].get("item_id") == "test/expensive"
        ]
        assert len(compute_calls) == 1, "Expected recompute after cache corruption"
        assert result2["output"]["value"] == 99
