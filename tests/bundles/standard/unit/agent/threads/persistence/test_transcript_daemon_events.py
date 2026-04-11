"""Tests for daemon-backed transcript reconstruction and rendering."""

import importlib.util
from pathlib import Path

from conftest import get_bundle_path

TRANSCRIPT_PATH = get_bundle_path(
    "standard", "tools/rye/agent/threads/persistence/transcript.py"
)
_spec = importlib.util.spec_from_file_location("transcript_module", TRANSCRIPT_PATH)
_mod = importlib.util.module_from_spec(_spec)
_spec.loader.exec_module(_mod)
Transcript = _mod.Transcript


def _daemon_events():
    return [
        {
            "event_id": 1,
            "chain_root_id": "T-test",
            "chain_seq": 1,
            "thread_id": "T-test",
            "thread_seq": 1,
            "event_type": "cognition_in",
            "storage_class": "indexed",
            "ts": "2026-04-11T08:00:00Z",
            "payload": {"role": "user", "text": "hello"},
        },
        {
            "event_id": 2,
            "chain_root_id": "T-test",
            "chain_seq": 2,
            "thread_id": "T-test",
            "thread_seq": 2,
            "event_type": "cognition_out",
            "storage_class": "indexed",
            "ts": "2026-04-11T08:00:01Z",
            "payload": {"text": "working"},
        },
        {
            "event_id": 3,
            "chain_root_id": "T-test",
            "chain_seq": 3,
            "thread_id": "T-test",
            "thread_seq": 3,
            "event_type": "tool_call_start",
            "storage_class": "indexed",
            "ts": "2026-04-11T08:00:02Z",
            "payload": {
                "tool": "tool:read",
                "call_id": "call-1",
                "input": {"path": "foo.txt"},
            },
        },
        {
            "event_id": 4,
            "chain_root_id": "T-test",
            "chain_seq": 4,
            "thread_id": "T-test",
            "thread_seq": 4,
            "event_type": "tool_call_result",
            "storage_class": "indexed",
            "ts": "2026-04-11T08:00:03Z",
            "payload": {"call_id": "call-1", "output": "ok"},
        },
    ]


class _FakeClient:
    def __init__(self, socket_path: str):
        self.socket_path = socket_path

    def replay_events(
        self,
        *,
        chain_root_id=None,
        thread_id=None,
        after_chain_seq=None,
        limit=200,
    ):
        assert chain_root_id is None
        assert thread_id == "T-test"
        all_events = _daemon_events()
        cursor = after_chain_seq or 0
        batch = [event for event in all_events if event["chain_seq"] > cursor][:limit]
        next_cursor = batch[-1]["chain_seq"] if len(batch) == limit else None
        return {"events": batch, "next_cursor": next_cursor}


def test_reconstruct_messages_reads_daemon_events(monkeypatch, tmp_path):
    monkeypatch.setattr(
        _mod,
        "get_daemon_runtime_context",
        lambda: {"socket_path": "/tmp/ryeosd.sock", "thread_id": "T-test", "chain_root_id": "T-test"},
    )
    monkeypatch.setattr(_mod, "ThreadLifecycleClient", _FakeClient)

    transcript = Transcript("T-test", tmp_path)
    messages = transcript.reconstruct_messages()

    assert messages == [
        {"role": "user", "content": "hello"},
        {
            "role": "assistant",
            "content": "working",
            "tool_calls": [
                {"name": "tool:read", "id": "call-1", "input": {"path": "foo.txt"}}
            ],
        },
        {"role": "tool", "tool_call_id": "call-1", "content": "ok"},
    ]


def test_render_knowledge_transcript_reads_daemon_events(
    monkeypatch, tmp_path, _setup_user_space
):
    monkeypatch.setattr(
        _mod,
        "get_daemon_runtime_context",
        lambda: {"socket_path": "/tmp/ryeosd.sock", "thread_id": "T-test", "chain_root_id": "T-test"},
    )
    monkeypatch.setattr(_mod, "ThreadLifecycleClient", _FakeClient)

    project_path = tmp_path / "project"
    project_path.mkdir()
    transcript = Transcript("T-test", project_path)

    knowledge_path = transcript.render_knowledge_transcript(
        directive="agent/test",
        status="running",
        model="test-model",
        cost={
            "turns": 1,
            "elapsed_seconds": 2.5,
            "input_tokens": 10,
            "output_tokens": 20,
            "spend": 0.125,
        },
        permissions=["read"],
    )

    assert knowledge_path == project_path / ".ai" / "knowledge" / "agent" / "threads" / "T-test.md"
    content = knowledge_path.read_text()
    assert "created_at: 2026-04-11T08:00:00Z" in content
    assert "## Input — Turn 1" in content
    assert "### Response — Turn 1" in content
    assert "### Tool: tool:read" in content
    assert "### Result" in content
    assert "**Running** -- 1 turns, 30 tokens, $0.1250, 2.5s" in content
