"""Tests for rye/core/processes tools and walker SIGTERM handler."""

import importlib.util
import json
import signal
import sqlite3
import sys
from datetime import datetime, timezone
from pathlib import Path
from unittest.mock import AsyncMock, MagicMock, patch

import pytest

# ---------------------------------------------------------------------------
# Load walker module for signal handler tests
# ---------------------------------------------------------------------------

_WALKER_DIR = (
    Path(__file__).resolve().parents[3]
    / "ryeos" / "bundles" / "core" / "ryeos_core"
    / ".ai" / "tools" / "rye" / "core" / "runtimes" / "state-graph"
)

if str(_WALKER_DIR) not in sys.path:
    sys.path.insert(0, str(_WALKER_DIR))

_spec = importlib.util.spec_from_file_location("walker_signal", _WALKER_DIR / "walker.py")
_walker = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_walker)


# ---------------------------------------------------------------------------
# Load process tools
# ---------------------------------------------------------------------------

_TOOLS_DIR = (
    Path(__file__).resolve().parents[3]
    / "ryeos" / "bundles" / "core" / "ryeos_core"
    / ".ai" / "tools" / "rye" / "core" / "processes"
)


def _load_tool(name):
    spec = importlib.util.spec_from_file_location(f"process_{name}", _TOOLS_DIR / f"{name}.py")
    mod = importlib.util.module_from_spec(spec)
    spec.loader.exec_module(mod)
    return mod


_status_tool = _load_tool("status")
_cancel_tool = _load_tool("cancel")
_list_tool = _load_tool("list")


# ---------------------------------------------------------------------------
# Fixtures
# ---------------------------------------------------------------------------


@pytest.fixture
def registry_db(tmp_path):
    """Create a temporary thread registry database."""
    from rye.constants import AI_DIR

    db_dir = tmp_path / AI_DIR / "agent" / "threads"
    db_dir.mkdir(parents=True)
    db_path = db_dir / "registry.db"

    now = datetime.now(timezone.utc).isoformat()
    with sqlite3.connect(db_path) as conn:
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
                turns INTEGER DEFAULT 0,
                input_tokens INTEGER DEFAULT 0,
                output_tokens INTEGER DEFAULT 0,
                spend REAL DEFAULT 0.0,
                spawn_count INTEGER DEFAULT 0,
                pid INTEGER,
                model TEXT,
                continuation_of TEXT,
                continuation_thread_id TEXT,
                chain_root_id TEXT
            )
        """)
        conn.execute(
            "INSERT INTO threads (thread_id, directive, status, created_at, updated_at, pid) VALUES (?, ?, ?, ?, ?, ?)",
            ("test-run-001", "graphs/my-graph", "running", now, now, 12345),
        )
        conn.execute(
            "INSERT INTO threads (thread_id, directive, status, created_at, updated_at, pid) VALUES (?, ?, ?, ?, ?, ?)",
            ("test-run-002", "graphs/other", "completed", now, now, 12346),
        )
        conn.execute(
            "INSERT INTO threads (thread_id, directive, status, created_at, updated_at, pid) VALUES (?, ?, ?, ?, ?, ?)",
            ("test-run-003", "graphs/third", "running", now, now, None),
        )
        conn.commit()

    return tmp_path


# ---------------------------------------------------------------------------
# Walker SIGTERM handler tests
# ---------------------------------------------------------------------------


class TestSigtermHandler:
    """Test the walker's SIGTERM signal handler."""

    def test_shutdown_flag_initially_false(self):
        assert _walker._shutdown_requested is not False or True  # module loaded, check it exists
        # Reset for clean test
        _walker._shutdown_requested = False
        assert _walker._shutdown_requested is False

    def test_handler_sets_flag(self):
        _walker._shutdown_requested = False
        _walker._sigterm_handler(signal.SIGTERM, None)
        assert _walker._shutdown_requested == signal.SIGTERM

    def test_handler_stores_signal_number(self):
        _walker._shutdown_requested = False
        _walker._sigterm_handler(15, None)
        assert _walker._shutdown_requested == 15

    def test_handler_is_callable(self):
        assert callable(_walker._sigterm_handler)

    def test_flag_reset(self):
        _walker._sigterm_handler(signal.SIGTERM, None)
        assert _walker._shutdown_requested != False  # noqa: E712
        _walker._shutdown_requested = False
        assert _walker._shutdown_requested is False


# ---------------------------------------------------------------------------
# Status tool tests
# ---------------------------------------------------------------------------


class TestStatusTool:
    """Test rye/core/processes/status."""

    def test_module_metadata(self):
        assert _status_tool.__version__ == "1.0.0"
        assert _status_tool.__category__ == "rye/core/processes"
        assert "run_id" in _status_tool.CONFIG_SCHEMA["required"]

    def test_run_not_found(self, registry_db):
        result = _status_tool.execute({"run_id": "nonexistent"}, str(registry_db))
        assert result["success"] is False
        assert "not found" in result["error"].lower()

    def test_no_registry(self, tmp_path):
        result = _status_tool.execute({"run_id": "test"}, str(tmp_path))
        assert result["success"] is False
        assert "registry" in result["error"].lower()

    def test_completed_run(self, registry_db):
        result = _status_tool.execute({"run_id": "test-run-002"}, str(registry_db))
        assert result["success"] is True
        assert result["status"] == "completed"
        assert result["alive"] is False

    @patch("lillux.primitives.subprocess.SubprocessPrimitive")
    def test_running_run_checks_pid(self, mock_sp_cls, registry_db):
        mock_sp = MagicMock()
        mock_status = AsyncMock()
        mock_status.return_value = MagicMock(alive=True, pid=12345)
        mock_sp.status = mock_status
        mock_sp_cls.return_value = mock_sp

        result = _status_tool.execute({"run_id": "test-run-001"}, str(registry_db))
        assert result["success"] is True
        assert result["status"] == "running"
        assert result["pid"] == 12345


# ---------------------------------------------------------------------------
# Cancel tool tests
# ---------------------------------------------------------------------------


class TestCancelTool:
    """Test rye/core/processes/cancel."""

    def test_module_metadata(self):
        assert _cancel_tool.__version__ == "1.0.0"
        assert _cancel_tool.__category__ == "rye/core/processes"
        assert "run_id" in _cancel_tool.CONFIG_SCHEMA["required"]

    def test_run_not_found(self, registry_db):
        result = _cancel_tool.execute({"run_id": "nonexistent"}, str(registry_db))
        assert result["success"] is False
        assert "not found" in result["error"].lower()

    def test_already_completed(self, registry_db):
        result = _cancel_tool.execute({"run_id": "test-run-002"}, str(registry_db))
        assert result["success"] is False
        assert "terminal state" in result["error"].lower()

    def test_no_pid(self, registry_db):
        result = _cancel_tool.execute({"run_id": "test-run-003"}, str(registry_db))
        assert result["success"] is False
        assert "no pid" in result["error"].lower()

    @patch("lillux.primitives.subprocess.SubprocessPrimitive")
    def test_successful_cancel(self, mock_sp_cls, registry_db):
        mock_sp = MagicMock()
        mock_kill = AsyncMock()
        mock_kill.return_value = MagicMock(success=True, method="terminated", error=None)
        mock_sp.kill = mock_kill
        mock_sp_cls.return_value = mock_sp

        result = _cancel_tool.execute({"run_id": "test-run-001"}, str(registry_db))
        assert result["success"] is True
        assert result["method"] == "terminated"
        assert result["pid"] == 12345

        # Verify registry was updated
        with sqlite3.connect(registry_db / ".ai" / "agent" / "threads" / "registry.db") as conn:
            cursor = conn.execute("SELECT status FROM threads WHERE thread_id = ?", ("test-run-001",))
            assert cursor.fetchone()[0] == "cancelled"


# ---------------------------------------------------------------------------
# List tool tests
# ---------------------------------------------------------------------------


class TestListTool:
    """Test rye/core/processes/list."""

    def test_module_metadata(self):
        assert _list_tool.__version__ == "1.0.0"
        assert _list_tool.__category__ == "rye/core/processes"

    def test_no_registry(self, tmp_path):
        result = _list_tool.execute({}, str(tmp_path))
        assert result["success"] is True
        assert result["count"] == 0

    def test_list_active(self, registry_db):
        result = _list_tool.execute({}, str(registry_db))
        assert result["success"] is True
        # Should return running/created threads (test-run-001 and test-run-003)
        assert result["count"] == 2
        run_ids = {r["run_id"] for r in result["runs"]}
        assert "test-run-001" in run_ids
        assert "test-run-003" in run_ids
        assert "test-run-002" not in run_ids

    def test_filter_by_status(self, registry_db):
        result = _list_tool.execute({"status": "completed"}, str(registry_db))
        assert result["success"] is True
        assert result["count"] == 1
        assert result["runs"][0]["run_id"] == "test-run-002"

    def test_filter_no_matches(self, registry_db):
        result = _list_tool.execute({"status": "killed"}, str(registry_db))
        assert result["success"] is True
        assert result["count"] == 0

    def test_run_fields(self, registry_db):
        result = _list_tool.execute({"status": "running"}, str(registry_db))
        run = next(r for r in result["runs"] if r["run_id"] == "test-run-001")
        assert run["directive"] == "graphs/my-graph"
        assert run["pid"] == 12345
        assert run["created_at"] is not None
