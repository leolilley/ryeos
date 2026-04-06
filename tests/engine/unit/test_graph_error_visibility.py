"""Tests for graph error visibility (completed_with_errors status).

Validates that when a graph uses on_error: continue and nodes fail,
the errors are tracked and surfaced through:
1. Walker: errors_suppressed list, completed_with_errors status
2. ExecutionSnapshot: errors field in CAS object
3. NodeReceipt: error field on failed nodes
4. ThreadRegistry: completed_with_errors as terminal status
5. processes/status: errors_suppressed count
6. processes/list: errors_suppressed in per-process summary
"""

import asyncio
import json
import sys
import tempfile
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

from conftest import get_bundle_path
from rye.cas.objects import ExecutionSnapshot, NodeReceipt


_WALKER_DIR = str(get_bundle_path("core", "tools/rye/core/runtimes/state-graph"))


# ---------------------------------------------------------------------------
# CAS object model tests
# ---------------------------------------------------------------------------


class TestNodeReceiptError:
    """NodeReceipt should include an error field when a node fails."""

    def test_no_error_by_default(self):
        receipt = NodeReceipt(
            node_input_hash="abc",
            node_result_hash="def",
            cache_hit=False,
            elapsed_ms=150,
            timestamp="2026-03-11T00:00:00Z",
        )
        d = receipt.to_dict()
        assert "error" not in d
        assert d["kind"] == "node_receipt"

    def test_error_included_when_set(self):
        receipt = NodeReceipt(
            node_input_hash="abc",
            node_result_hash="def",
            cache_hit=False,
            elapsed_ms=150,
            timestamp="2026-03-11T00:00:00Z",
            error="Lockfile integrity mismatch",
        )
        d = receipt.to_dict()
        assert d["error"] == "Lockfile integrity mismatch"

    def test_none_error_excluded(self):
        receipt = NodeReceipt(
            node_input_hash="abc",
            node_result_hash="def",
            cache_hit=False,
            elapsed_ms=150,
            timestamp="2026-03-11T00:00:00Z",
            error=None,
        )
        d = receipt.to_dict()
        assert "error" not in d


class TestExecutionSnapshotErrors:
    """ExecutionSnapshot should include errors list when nodes failed."""

    def test_no_errors_by_default(self):
        snap = ExecutionSnapshot(
            graph_run_id="run-1",
            graph_id="test",
            step=5,
            status="completed",
        )
        d = snap.to_dict()
        assert "errors" not in d

    def test_errors_included_when_present(self):
        errors = [
            {"step": 2, "node": "analyze_first", "error": "Lockfile mismatch"},
            {"step": 3, "node": "analyze_second", "error": "Lockfile mismatch"},
        ]
        snap = ExecutionSnapshot(
            graph_run_id="run-1",
            graph_id="test",
            step=5,
            status="completed_with_errors",
            errors=errors,
        )
        d = snap.to_dict()
        assert d["errors"] == errors
        assert d["status"] == "completed_with_errors"
        assert len(d["errors"]) == 2


# ---------------------------------------------------------------------------
# Walker _store_node_receipt tests
# ---------------------------------------------------------------------------


class TestStoreNodeReceiptWithError:
    """_store_node_receipt should pass error to NodeReceipt CAS object."""

    def test_receipt_without_error(self, _setup_user_space):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            sys.path.insert(0, _WALKER_DIR)
            try:
                from walker import _store_node_receipt

                h = _store_node_receipt(
                    str(project),
                    node_input_hash="inp",
                    node_result_hash="res",
                    cache_hit=False,
                    elapsed_ms=100,
                )
                assert h is not None

                from rye.primitives import cas
                from rye.cas.store import cas_root

                obj = cas.get_object(h, cas_root(project))
                assert obj["kind"] == "node_receipt"
                assert "error" not in obj
            finally:
                sys.path.remove(_WALKER_DIR)

    def test_receipt_with_error(self, _setup_user_space):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            sys.path.insert(0, _WALKER_DIR)
            try:
                from walker import _store_node_receipt

                h = _store_node_receipt(
                    str(project),
                    node_input_hash="inp",
                    node_result_hash="res",
                    cache_hit=False,
                    elapsed_ms=195,
                    error="Lockfile integrity mismatch for rye/agent/threads/thread_directive",
                )
                assert h is not None

                from rye.primitives import cas
                from rye.cas.store import cas_root

                obj = cas.get_object(h, cas_root(project))
                assert obj["kind"] == "node_receipt"
                assert (
                    obj["error"]
                    == "Lockfile integrity mismatch for rye/agent/threads/thread_directive"
                )
            finally:
                sys.path.remove(_WALKER_DIR)


# ---------------------------------------------------------------------------
# Walker _persist_state with errors tests
# ---------------------------------------------------------------------------


class TestPersistStateWithErrors:
    """_persist_state should include errors in ExecutionSnapshot."""

    @pytest.mark.asyncio
    async def test_persist_without_errors(self, _setup_user_space):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            sys.path.insert(0, _WALKER_DIR)
            try:
                from walker import _persist_state

                h = await _persist_state(
                    str(project),
                    "test-graph",
                    "run-1",
                    {"key": "value"},
                    "node1",
                    "completed",
                    5,
                )
                assert h is not None

                from rye.primitives import cas
                from rye.cas.store import cas_root

                snap = cas.get_object(h, cas_root(project))
                assert snap["status"] == "completed"
                assert "errors" not in snap
            finally:
                sys.path.remove(_WALKER_DIR)

    @pytest.mark.asyncio
    async def test_persist_with_errors(self, _setup_user_space):
        with tempfile.TemporaryDirectory() as tmpdir:
            project = Path(tmpdir)
            (project / ".ai" / "objects").mkdir(parents=True)

            sys.path.insert(0, _WALKER_DIR)
            try:
                from walker import _persist_state

                errors = [
                    {"step": 2, "node": "analyze_first", "error": "Lockfile mismatch"},
                    {"step": 3, "node": "analyze_second", "error": "Lockfile mismatch"},
                ]
                h = await _persist_state(
                    str(project),
                    "test-graph",
                    "run-1",
                    {"key": "value"},
                    "node1",
                    "completed_with_errors",
                    5,
                    errors=errors,
                )
                assert h is not None

                from rye.primitives import cas
                from rye.cas.store import cas_root

                snap = cas.get_object(h, cas_root(project))
                assert snap["status"] == "completed_with_errors"
                assert snap["errors"] == errors
                assert len(snap["errors"]) == 2
            finally:
                sys.path.remove(_WALKER_DIR)


# ---------------------------------------------------------------------------
# ThreadRegistry completed_with_errors tests
# ---------------------------------------------------------------------------


class TestThreadRegistryCompletedWithErrors:
    """ThreadRegistry should treat completed_with_errors as a terminal status."""

    def test_completed_with_errors_sets_completed_at(self, tmp_path):
        sys.path.insert(
            0, str(get_bundle_path("standard", "tools/rye/agent/threads/persistence"))
        )
        try:
            from thread_registry import ThreadRegistry

            registry = ThreadRegistry(tmp_path)
            registry.register("t-1", "test/graph", None)
            registry.update_status("t-1", "running")
            registry.update_status("t-1", "completed_with_errors")

            thread = registry.get_thread("t-1")
            assert thread["status"] == "completed_with_errors"
            assert thread["completed_at"] is not None
        finally:
            sys.path.pop(0)

    def test_completed_with_errors_excluded_from_list_active(self, tmp_path):
        sys.path.insert(
            0, str(get_bundle_path("standard", "tools/rye/agent/threads/persistence"))
        )
        try:
            from thread_registry import ThreadRegistry

            registry = ThreadRegistry(tmp_path)
            registry.register("t-1", "test/graph", None)
            registry.update_status("t-1", "completed_with_errors")

            registry.register("t-2", "test/graph2", None)
            registry.update_status("t-2", "running")

            active = registry.list_active()
            active_ids = [t["thread_id"] for t in active]
            assert "t-1" not in active_ids
            assert "t-2" in active_ids
        finally:
            sys.path.pop(0)

    def test_completed_status_still_works(self, tmp_path):
        sys.path.insert(
            0, str(get_bundle_path("standard", "tools/rye/agent/threads/persistence"))
        )
        try:
            from thread_registry import ThreadRegistry

            registry = ThreadRegistry(tmp_path)
            registry.register("t-1", "test/graph", None)
            registry.update_status("t-1", "completed")

            thread = registry.get_thread("t-1")
            assert thread["status"] == "completed"
            assert thread["completed_at"] is not None

            active = registry.list_active()
            assert len([t for t in active if t["thread_id"] == "t-1"]) == 0
        finally:
            sys.path.pop(0)


# ---------------------------------------------------------------------------
# processes/status tests
# ---------------------------------------------------------------------------


class TestProcessStatusErrorCount:
    """processes/status should include errors_suppressed for completed_with_errors."""

    def test_status_completed_with_errors_includes_count(self, tmp_path):
        from rye.constants import AI_DIR

        # Set up registry with a completed_with_errors thread
        db_path = tmp_path / AI_DIR / "agent" / "threads"
        db_path.mkdir(parents=True)

        import sqlite3

        with sqlite3.connect(db_path / "registry.db") as conn:
            conn.execute("""
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT,
                    pid INTEGER
                )
            """)
            result_json = json.dumps(
                {
                    "success": True,
                    "status": "completed_with_errors",
                    "errors_suppressed": 3,
                    "errors": [
                        {
                            "step": 4,
                            "node": "analyze_first",
                            "error": "Lockfile mismatch",
                        },
                        {
                            "step": 5,
                            "node": "analyze_second",
                            "error": "Lockfile mismatch",
                        },
                        {
                            "step": 9,
                            "node": "run_quick_summary",
                            "error": "Lockfile mismatch",
                        },
                    ],
                }
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-123",
                    "graphs/test",
                    None,
                    "completed_with_errors",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    "2026-03-11T00:01:00Z",
                    result_json,
                    12345,
                ),
            )
            conn.commit()

        status_dir = str(get_bundle_path("core", "tools/rye/core/processes"))
        sys.path.insert(0, status_dir)
        try:
            import importlib
            import status as status_mod

            importlib.reload(status_mod)

            result = status_mod.execute({"run_id": "run-123"}, str(tmp_path))
            assert result["success"] is True
            assert result["status"] == "completed_with_errors"
            assert result["errors_suppressed"] == 3
        finally:
            sys.path.remove(status_dir)

    def test_status_completed_no_errors_suppressed(self, tmp_path):
        from rye.constants import AI_DIR

        db_path = tmp_path / AI_DIR / "agent" / "threads"
        db_path.mkdir(parents=True)

        import sqlite3

        with sqlite3.connect(db_path / "registry.db") as conn:
            conn.execute("""
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT,
                    pid INTEGER
                )
            """)
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-456",
                    "graphs/test",
                    None,
                    "completed",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    "2026-03-11T00:01:00Z",
                    None,
                    12345,
                ),
            )
            conn.commit()

        status_dir = str(get_bundle_path("core", "tools/rye/core/processes"))
        sys.path.insert(0, status_dir)
        try:
            import importlib
            import status as status_mod

            importlib.reload(status_mod)

            result = status_mod.execute({"run_id": "run-456"}, str(tmp_path))
            assert result["success"] is True
            assert result["status"] == "completed"
            assert "errors_suppressed" not in result
        finally:
            sys.path.remove(status_dir)


# ---------------------------------------------------------------------------
# processes/list tests
# ---------------------------------------------------------------------------


class TestProcessListErrorCount:
    """processes/list should include errors_suppressed for completed_with_errors entries."""

    def test_list_completed_with_errors_includes_count(self, tmp_path):
        from rye.constants import AI_DIR

        db_path = tmp_path / AI_DIR / "agent" / "threads"
        db_path.mkdir(parents=True)

        import sqlite3

        with sqlite3.connect(db_path / "registry.db") as conn:
            conn.execute("""
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT,
                    pid INTEGER
                )
            """)
            result_json = json.dumps(
                {
                    "success": True,
                    "status": "completed_with_errors",
                    "errors_suppressed": 2,
                }
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-789",
                    "graphs/pipeline",
                    None,
                    "completed_with_errors",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    "2026-03-11T00:01:00Z",
                    result_json,
                    99999,
                ),
            )
            conn.commit()

        list_dir = str(get_bundle_path("core", "tools/rye/core/processes"))
        sys.path.insert(0, list_dir)
        try:
            import importlib

            # Use importlib to avoid name collision with builtins
            import list as list_mod

            importlib.reload(list_mod)

            # Filter by completed_with_errors status
            result = list_mod.execute(
                {"status": "completed_with_errors"}, str(tmp_path)
            )
            assert result["success"] is True
            assert result["count"] == 1
            entry = result["runs"][0]
            assert entry["status"] == "completed_with_errors"
            assert entry["errors_suppressed"] == 2
        finally:
            sys.path.remove(list_dir)

    def test_list_completed_with_errors_excluded_from_active(self, tmp_path):
        from rye.constants import AI_DIR

        db_path = tmp_path / AI_DIR / "agent" / "threads"
        db_path.mkdir(parents=True)

        import sqlite3

        with sqlite3.connect(db_path / "registry.db") as conn:
            conn.execute("""
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT,
                    pid INTEGER
                )
            """)
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-done",
                    "graphs/a",
                    None,
                    "completed_with_errors",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    "2026-03-11T00:01:00Z",
                    '{"errors_suppressed": 1}',
                    111,
                ),
            )
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-active",
                    "graphs/b",
                    None,
                    "running",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    None,
                    None,
                    222,
                ),
            )
            conn.commit()

        list_dir = str(get_bundle_path("core", "tools/rye/core/processes"))
        sys.path.insert(0, list_dir)
        try:
            import importlib
            import list as list_mod

            importlib.reload(list_mod)

            # Default (active only) should exclude completed_with_errors
            result = list_mod.execute({}, str(tmp_path))
            assert result["count"] == 1
            assert result["runs"][0]["run_id"] == "run-active"
        finally:
            sys.path.remove(list_dir)

    def test_list_normal_completed_no_errors_suppressed(self, tmp_path):
        from rye.constants import AI_DIR

        db_path = tmp_path / AI_DIR / "agent" / "threads"
        db_path.mkdir(parents=True)

        import sqlite3

        with sqlite3.connect(db_path / "registry.db") as conn:
            conn.execute("""
                CREATE TABLE threads (
                    thread_id TEXT PRIMARY KEY,
                    directive TEXT NOT NULL,
                    parent_id TEXT,
                    status TEXT DEFAULT 'created',
                    created_at TEXT,
                    updated_at TEXT,
                    completed_at TEXT,
                    result TEXT,
                    pid INTEGER
                )
            """)
            conn.execute(
                "INSERT INTO threads VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)",
                (
                    "run-ok",
                    "graphs/clean",
                    None,
                    "completed",
                    "2026-03-11T00:00:00Z",
                    "2026-03-11T00:01:00Z",
                    "2026-03-11T00:01:00Z",
                    None,
                    333,
                ),
            )
            conn.commit()

        list_dir = str(get_bundle_path("core", "tools/rye/core/processes"))
        sys.path.insert(0, list_dir)
        try:
            import importlib
            import list as list_mod

            importlib.reload(list_mod)

            result = list_mod.execute({"status": "completed"}, str(tmp_path))
            assert result["count"] == 1
            assert "errors_suppressed" not in result["runs"][0]
        finally:
            sys.path.remove(list_dir)
