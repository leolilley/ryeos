"""Tests for tool_result_guard — no_truncate behavior."""

import importlib.util
import json
from pathlib import Path
from unittest.mock import MagicMock, patch

from conftest import get_bundle_path

GUARD_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/internal/tool_result_guard.py"
)
_spec = importlib.util.spec_from_file_location("tool_result_guard", GUARD_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)
guard_result = _mod.guard_result


def _mock_artifact_store():
    store = MagicMock()
    store.has_content.return_value = None
    store.store.return_value = "artifact-ref-123"
    return store


def _large_data():
    return {"data": [{"id": f"item-{i}", "name": f"Show {i}", "desc": "x" * 200} for i in range(20)]}


class TestNoTruncate:
    """When no_truncate=True, guard_result returns full data (no summarization)."""

    def test_no_truncate_returns_full_result(self, tmp_path):
        large_data = _large_data()
        store = _mock_artifact_store()

        with patch.object(_mod, "load_module") as mock_load:
            mock_load.return_value.get_artifact_store.return_value = store
            result = guard_result(
                large_data,
                call_id="call-1",
                tool_name="test_tool",
                thread_id="T-test",
                project_path=tmp_path,
                no_truncate=True,
            )

        assert result == large_data
        assert "data_preview" not in result
        assert "data_count" not in result

    def test_no_truncate_still_dedupes(self, tmp_path):
        large_data = {"data": [{"id": f"item-{i}", "description": f"desc {'x' * 200}"} for i in range(20)]}
        store = _mock_artifact_store()
        store.has_content.return_value = "previous-call-id"

        with patch.object(_mod, "load_module") as mock_load:
            mock_load.return_value.get_artifact_store.return_value = store
            result = guard_result(
                large_data,
                call_id="call-2",
                tool_name="test_tool",
                thread_id="T-test",
                project_path=tmp_path,
                no_truncate=True,
            )

        assert result["status"] == "success"
        assert "previous-call-id" in result["note"]

    def test_default_truncates(self, tmp_path):
        large_data = _large_data()
        store = _mock_artifact_store()

        with patch.object(_mod, "load_module") as mock_load:
            mock_load.return_value.get_artifact_store.return_value = store
            result = guard_result(
                large_data,
                call_id="call-3",
                tool_name="test_tool",
                thread_id="T-test",
                project_path=tmp_path,
            )

        assert "_artifact_ref" in result
        assert result != large_data

    def test_small_result_unaffected_by_no_truncate(self, tmp_path):
        small_data = {"status": "ok", "count": 1}
        result = guard_result(
            small_data,
            call_id="call-4",
            tool_name="test_tool",
            thread_id="T-test",
            project_path=tmp_path,
            no_truncate=True,
        )
        assert result == small_data

    def test_no_truncate_still_stores_artifact(self, tmp_path):
        large_data = {"shows_text": "x" * 5000}
        store = _mock_artifact_store()

        with patch.object(_mod, "load_module") as mock_load:
            mock_load.return_value.get_artifact_store.return_value = store
            guard_result(
                large_data,
                call_id="call-5",
                tool_name="test_tool",
                thread_id="T-test",
                project_path=tmp_path,
                no_truncate=True,
            )

        store.store.assert_called_once()
