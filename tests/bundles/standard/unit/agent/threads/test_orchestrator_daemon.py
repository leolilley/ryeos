"""Tests for daemon-backed orchestrator control surfaces."""

import asyncio
import importlib.util

from conftest import get_bundle_path

ORCHESTRATOR_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/orchestrator.py"
)
_spec = importlib.util.spec_from_file_location("orchestrator_module", ORCHESTRATOR_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)


class _FakeClient:
    def __init__(self, socket_path):
        assert socket_path == "/tmp/ryeosd.sock"

    def get_thread(self, thread_id):
        assert thread_id == "T-1"
        return {
            "thread": {
                "thread_id": "T-1",
                "status": "failed",
            },
            "result": {
                "outcome_code": "engine_error",
                "result": None,
                "error": {"message": "boom"},
            },
            "artifacts": [{"artifact_id": 1, "uri": "file:///tmp/log.txt"}],
        }


def test_get_status_reads_daemon_thread(monkeypatch, tmp_path):
    monkeypatch.setattr(_mod, "resolve_daemon_socket_path", lambda: "/tmp/ryeosd.sock")
    monkeypatch.setattr(_mod, "ThreadLifecycleClient", _FakeClient)

    result = asyncio.run(
        _mod.execute({"operation": "get_status", "thread_id": "T-1"}, str(tmp_path))
    )

    assert result == {
        "success": True,
        "thread_id": "T-1",
        "status": "failed",
        "outcome_code": "engine_error",
        "result": None,
        "error": {"message": "boom"},
        "artifacts": [{"artifact_id": 1, "uri": "file:///tmp/log.txt"}],
    }


def test_resume_thread_is_explicitly_unsupported(tmp_path):
    result = asyncio.run(
        _mod.execute(
            {
                "operation": "resume_thread",
                "thread_id": "T-1",
                "message": "continue",
            },
            str(tmp_path),
        )
    )

    assert result["success"] is False
    assert "daemon-owned continuation" in result["error"]
