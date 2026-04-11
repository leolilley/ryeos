"""Tests for daemon-aware thread chain search."""

import importlib.util

from conftest import get_bundle_path

SEARCH_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/internal/thread_chain_search.py"
)
_spec = importlib.util.spec_from_file_location("thread_chain_search", SEARCH_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)


class _FakeClient:
    def __init__(self, socket_path):
        assert socket_path == "/tmp/ryeosd.sock"

    def get_chain(self, thread_id):
        assert thread_id == "T-root"
        return {
            "threads": [{"thread_id": "T-root"}, {"thread_id": "T-child"}],
            "edges": [],
        }

    def replay_events(self, *, thread_id, after_chain_seq=None, limit=200, chain_root_id=None):
        assert chain_root_id is None
        if thread_id == "T-root":
            events = [
                {
                    "chain_seq": 1,
                    "event_type": "cognition_in",
                    "payload": {"text": "hello root"},
                },
            ]
        else:
            events = [
                {
                    "chain_seq": 1,
                    "event_type": "tool_call_result",
                    "payload": {"output": "needle result"},
                },
                {
                    "chain_seq": 2,
                    "event_type": "stream_snapshot",
                    "payload": {"text": "ignored"},
                },
            ]

        cursor = after_chain_seq or 0
        page = [event for event in events if event["chain_seq"] > cursor][:limit]
        next_cursor = page[-1]["chain_seq"] if len(page) == limit else None
        return {"events": page, "next_cursor": next_cursor}


def test_chain_search_uses_daemon_chain_and_events(tmp_path, monkeypatch):
    monkeypatch.setattr(_mod, "resolve_daemon_socket_path", lambda: "/tmp/ryeosd.sock")
    monkeypatch.setattr(_mod, "ThreadLifecycleClient", _FakeClient)

    result = _mod.execute(
        {
            "thread_id": "T-root",
            "query": "needle",
            "search_type": "text",
            "max_results": 10,
        },
        str(tmp_path),
    )

    assert result["success"] is True
    assert result["chain_length"] == 2
    assert result["chain_threads"] == ["T-root", "T-child"]
    assert result["results"] == [
        {
            "thread_id": "T-child",
            "event_type": "tool_call_result",
            "line_no": 1,
            "snippet": '{"output": "needle result"}',
            "matches": ["needle"],
        }
    ]
    assert result["truncated"] is False
