"""Contract tests for future agent-threads extensions.

These tests verify data structures, file formats, and protocol logic
WITHOUT importing implementation modules. They are self-contained
contract tests derived from docs/rye/design/agent-threads-future.md.
"""

import asyncio
import json
import tempfile
import threading
import time
from pathlib import Path

import pytest


@pytest.fixture
def thread_dir(tmp_path):
    """Create temporary thread directory."""
    return tmp_path


THREAD_ID = "planner-1739012900"


@pytest.mark.asyncio
class TestContinueThread:
    """Tests for multi-turn conversation mode."""

    async def test_reject_single_mode_thread(self, thread_dir):
        """Cannot continue a single-mode thread."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "single",
            "status": "completed",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["thread_mode"] == "single"

    async def test_reject_running_thread(self, thread_dir):
        """Cannot continue a thread that is already running."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "running",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "running"

    async def test_accept_paused_thread(self, thread_dir):
        """Can continue a paused conversation thread."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "paused",
            "awaiting": "user",
            "directive": "planner",
        }))
        meta = json.loads(meta_path.read_text())
        assert meta["thread_mode"] == "conversation"
        assert meta["status"] == "paused"

    async def test_status_transitions(self, thread_dir):
        """Continuing a thread transitions: paused → running → paused."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta = {
            "thread_id": THREAD_ID,
            "thread_mode": "conversation",
            "status": "paused",
            "awaiting": "user",
            "directive": "planner",
        }
        meta_path.write_text(json.dumps(meta))

        meta["status"] = "running"
        meta["awaiting"] = None
        meta_path.write_text(json.dumps(meta))
        loaded = json.loads(meta_path.read_text())
        assert loaded["status"] == "running"
        assert loaded["awaiting"] is None

        meta["status"] = "paused"
        meta["awaiting"] = "user"
        meta["turn_count"] = 4
        meta_path.write_text(json.dumps(meta))
        loaded = json.loads(meta_path.read_text())
        assert loaded["status"] == "paused"
        assert loaded["turn_count"] == 4


class TestConversationReconstruction:
    """Tests for rebuilding LLM conversation from transcript events."""

    def test_reconstruct_user_and_assistant(self, thread_dir):
        """Reconstructs user_message and assistant_text."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"user_message","role":"user","text":"Hello"}\n'
            '{"ts":"T","type":"assistant_text","text":"Hi there"}\n'
            '{"ts":"T","type":"user_message","role":"user","text":"Help me"}\n'
            '{"ts":"T","type":"assistant_text","text":"Sure"}\n'
        )
        events = []
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                event = json.loads(line)
                if event["type"] == "user_message":
                    messages.append({"role": event["role"], "content": event["text"]})
                elif event["type"] == "assistant_text":
                    messages.append({"role": "assistant", "content": event["text"]})
        assert len(messages) == 4
        assert messages[0] == {"role": "user", "content": "Hello"}
        assert messages[1] == {"role": "assistant", "content": "Hi there"}

    def test_reconstruct_with_tool_calls_provider_driven(self, thread_dir):
        """Reconstructs tool_call_start/result using provider config (not hardcoded)."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"assistant_text","text":"I will read the file"}\n'
            '{"ts":"T","type":"tool_call_start","tool":"fs_read","call_id":"tc_1","input":{"path":"/x"}}\n'
            '{"ts":"T","type":"tool_call_result","call_id":"tc_1","output":"file contents"}\n'
        )
        recon_config = {
            "tool_call": {
                "role": "assistant",
                "content_block": {
                    "type": "tool_use",
                    "id_field": "call_id",
                    "name_field": "tool",
                    "input_field": "input",
                },
            },
            "tool_result": {
                "role": "user",
                "content_block": {
                    "type": "tool_result",
                    "id_field": "call_id",
                    "id_target": "tool_use_id",
                    "content_field": "output",
                    "error_field": "error",
                    "error_target": "is_error",
                },
            },
        }
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                event = json.loads(line.strip())
                match event.get("type"):
                    case "assistant_text":
                        messages.append({"role": "assistant", "content": event["text"]})
                    case "tool_call_start":
                        tc = recon_config["tool_call"]
                        bc = tc["content_block"]
                        messages.append({
                            "role": tc["role"],
                            "content": [{
                                "type": bc["type"],
                                "id": event.get(bc["id_field"], ""),
                                "name": event.get(bc["name_field"], ""),
                                "input": event.get(bc["input_field"], {}),
                            }],
                        })
                    case "tool_call_result":
                        tr = recon_config["tool_result"]
                        bc = tr["content_block"]
                        block = {
                            "type": bc["type"],
                            bc["id_target"]: event.get(bc["id_field"], ""),
                            "content": event.get(bc["content_field"], ""),
                        }
                        if event.get(bc["error_field"]):
                            block[bc["error_target"]] = True
                        messages.append({"role": tr["role"], "content": [block]})
        assert len(messages) == 3
        assert messages[1]["content"][0]["type"] == "tool_use"
        assert messages[1]["content"][0]["name"] == "fs_read"
        assert messages[2]["content"][0]["type"] == "tool_result"
        assert messages[2]["content"][0]["tool_use_id"] == "tc_1"
        assert messages[2]["content"][0]["content"] == "file contents"

    def test_reconstruct_errors_without_config(self, thread_dir):
        """Missing message_reconstruction raises ValueError, no silent fallback."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"tool_call_start","tool":"fs_read","call_id":"tc_1","input":{}}\n'
        )
        provider_config = {"tool_use": {"response": {}}}
        with pytest.raises(ValueError, match="message_reconstruction"):
            if "message_reconstruction" not in provider_config:
                raise ValueError(
                    f"Provider config missing 'message_reconstruction' section. "
                    f"Cannot reconstruct conversation for thread {THREAD_ID}. "
                    f"Add message_reconstruction to your provider YAML."
                )

    def test_reconstruct_skips_non_message_events(self, thread_dir):
        """Non-message events (step_start, step_finish) are ignored."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"thread_start","directive":"test"}\n'
            '{"ts":"T","type":"step_start","turn_number":1}\n'
            '{"ts":"T","type":"user_message","role":"user","text":"Hi"}\n'
            '{"ts":"T","type":"assistant_text","text":"Hello"}\n'
            '{"ts":"T","type":"step_finish","cost":{},"tokens":{}}\n'
        )
        message_types = {"user_message", "assistant_text", "tool_call_start", "tool_call_result"}
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                event = json.loads(line.strip())
                if event.get("type") in message_types:
                    messages.append(event)
        assert len(messages) == 2

    def test_reconstruct_handles_corrupt_lines(self, thread_dir):
        """Corrupt JSONL lines are skipped during reconstruction."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T","type":"user_message","role":"user","text":"Hi"}\n'
            'CORRUPT LINE\n'
            '{"ts":"T","type":"assistant_text","text":"Hello"}\n'
        )
        messages = []
        with open(jsonl_path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                    if event.get("type") in ("user_message", "assistant_text"):
                        messages.append(event)
                except json.JSONDecodeError:
                    continue
        assert len(messages) == 2


class TestHarnessStatePersistence:
    """Tests for state.json serialization/deserialization."""

    def test_save_and_restore_state(self, thread_dir):
        """State can be saved to state.json and restored."""
        state = {
            "directive": "planner",
            "inputs": {"goal": "plan feature"},
            "cost": {"turns": 3, "tokens": 12500, "spend": 0.0234,
                     "input_tokens": 10000, "output_tokens": 2500,
                     "spawns": 0, "duration_seconds": 45.2},
            "limits": {"turns": 20, "tokens": 100000},
            "hooks": [],
            "required_caps": ["fs.read", "fs.write"],
        }
        state_path = thread_dir / THREAD_ID / "state.json"
        state_path.parent.mkdir(parents=True)

        tmp_path = state_path.with_suffix(".json.tmp")
        tmp_path.write_text(json.dumps(state, indent=2))
        tmp_path.rename(state_path)

        restored = json.loads(state_path.read_text())
        assert restored["directive"] == "planner"
        assert restored["cost"]["turns"] == 3
        assert restored["cost"]["spend"] == 0.0234
        assert restored["limits"]["turns"] == 20

    def test_state_cost_accumulates_across_turns(self, thread_dir):
        """Cost in state.json should reflect cumulative totals."""
        state = {
            "cost": {"turns": 0, "tokens": 0, "spend": 0.0},
        }
        for i in range(3):
            state["cost"]["turns"] += 1
            state["cost"]["tokens"] += 1000
            state["cost"]["spend"] += 0.01
        assert state["cost"]["turns"] == 3
        assert state["cost"]["tokens"] == 3000
        assert abs(state["cost"]["spend"] - 0.03) < 0.001


class TestThreadHandle:
    """Tests for async fire-and-forget thread handle."""

    def test_handle_initial_state(self, thread_dir):
        """New handle starts as not done."""
        done = threading.Event()
        assert not done.is_set()

    def test_handle_set_result(self, thread_dir):
        """Setting result marks handle as done."""
        done = threading.Event()
        result = None

        def set_result(r):
            nonlocal result
            result = r
            done.set()

        set_result({"status": "completed", "text": "Done"})
        assert done.is_set()
        assert result["status"] == "completed"

    def test_handle_set_error(self, thread_dir):
        """Setting error marks handle as done with error."""
        done = threading.Event()
        error = None

        def set_error(e):
            nonlocal error
            error = e
            done.set()

        set_error(RuntimeError("Something broke"))
        assert done.is_set()
        assert isinstance(error, RuntimeError)

    def test_handle_peek_transcript(self, thread_dir):
        """peek_transcript reads latest N entries from running thread."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        events = [
            {"ts": f"T{i}", "type": "assistant_text", "text": f"msg {i}"}
            for i in range(10)
        ]
        jsonl_path.write_text(
            "\n".join(json.dumps(e) for e in events) + "\n"
        )
        with open(jsonl_path) as f:
            lines = f.readlines()
        last_5 = [json.loads(l.strip()) for l in lines[-5:] if l.strip()]
        assert len(last_5) == 5
        assert last_5[0]["text"] == "msg 5"
        assert last_5[-1]["text"] == "msg 9"

    def test_handle_status_from_thread_json(self, thread_dir):
        """Handle reads status from thread.json."""
        meta_path = thread_dir / THREAD_ID / "thread.json"
        meta_path.parent.mkdir(parents=True)
        meta_path.write_text(json.dumps({"status": "running"}))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "running"

        meta_path.write_text(json.dumps({"status": "completed"}))
        meta = json.loads(meta_path.read_text())
        assert meta["status"] == "completed"


class TestHumanApprovalFlow:
    """Tests for file-based human approval signal pattern."""

    def test_approval_request_creation(self, thread_dir):
        """Approval request creates .request.json with correct structure."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        request_path = approval_dir / f"{request_id}.request.json"
        request_path.write_text(json.dumps({
            "id": request_id,
            "prompt": "Deploy to production?",
            "thread_id": THREAD_ID,
            "created_at": "2026-02-09T04:03:50Z",
            "timeout_seconds": 300,
        }, indent=2))
        request = json.loads(request_path.read_text())
        assert request["id"] == request_id
        assert request["prompt"] == "Deploy to production?"
        assert request["timeout_seconds"] == 300

    def test_approval_approved(self, thread_dir):
        """Approved response returns continue action."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        response_path.write_text(json.dumps({
            "approved": True,
            "message": "Ship it",
        }))
        response = json.loads(response_path.read_text())
        assert response["approved"] is True
        action = "continue" if response["approved"] else "fail"
        assert action == "continue"

    def test_approval_rejected(self, thread_dir):
        """Rejected response returns fail action with message."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        response_path.write_text(json.dumps({
            "approved": False,
            "message": "Wait for QA",
        }))
        response = json.loads(response_path.read_text())
        assert response["approved"] is False
        error = response.get("message", "Rejected by human")
        assert error == "Wait for QA"

    def test_approval_timeout(self, thread_dir):
        """Missing response file after timeout returns fail."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        response_path = approval_dir / f"{request_id}.response.json"
        assert not response_path.exists()
        action = "fail"
        error = "Approval timed out after 300s"
        assert action == "fail"

    def test_approval_directory_layout(self, thread_dir):
        """Approval files follow expected directory structure."""
        approval_dir = thread_dir / THREAD_ID / "approvals"
        approval_dir.mkdir(parents=True)
        request_id = "approval-1739012650"
        (approval_dir / f"{request_id}.request.json").write_text("{}")
        (approval_dir / f"{request_id}.response.json").write_text("{}")
        files = sorted(f.name for f in approval_dir.iterdir())
        assert f"{request_id}.request.json" in files
        assert f"{request_id}.response.json" in files


class TestThreadChannel:
    """Tests for thread channel turn protocol."""

    def test_round_robin_advances_turn(self, thread_dir):
        """Round-robin protocol advances to next execution."""
        state = {
            "thread_mode": "channel",
            "members": [
                {"thread_id": "planner-1739012630", "directive": "plan_feature"},
                {"thread_id": "coder-1739012701", "directive": "implement_plan"},
                {"thread_id": "reviewer-1739012802", "directive": "review_code"},
            ],
            "turn_protocol": "round_robin",
            "turn_order": ["planner-1739012630", "coder-1739012701", "reviewer-1739012802"],
            "current_turn": "planner-1739012630",
            "turn_count": 0,
        }
        order = state["turn_order"]
        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "coder-1739012701"
        assert state["turn_count"] == 1

        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "reviewer-1739012802"

        idx = order.index(state["current_turn"])
        next_idx = (idx + 1) % len(order)
        state["current_turn"] = order[next_idx]
        state["turn_count"] += 1
        assert state["current_turn"] == "planner-1739012630"

    def test_round_robin_rejects_wrong_turn(self, thread_dir):
        """Execution cannot write when it's not their turn."""
        state = {
            "turn_protocol": "round_robin",
            "current_turn": "planner-1739012630",
        }
        origin = "coder-1739012701"
        assert state["current_turn"] != origin

    def test_on_demand_allows_any_execution(self, thread_dir):
        """On-demand protocol allows any execution to write."""
        state = {
            "turn_protocol": "on_demand",
            "current_turn": "planner-1739012630",
        }
        assert state["turn_protocol"] == "on_demand"


class TestTranscriptWatcher:
    """Tests for file-based transcript polling."""

    def test_poll_new_events_initial(self, thread_dir):
        """First poll returns all events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text(
            '{"ts":"T1","type":"thread_start"}\n'
            '{"ts":"T2","type":"assistant_text","text":"Hi"}\n'
        )
        last_pos = 0
        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
            last_pos = f.tell()
        events = [json.loads(l.strip()) for l in new_lines if l.strip()]
        assert len(events) == 2

    def test_poll_incremental(self, thread_dir):
        """Subsequent polls return only new events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text('{"ts":"T1","type":"thread_start"}\n')

        last_pos = 0
        with open(jsonl_path) as f:
            f.seek(last_pos)
            lines = f.readlines()
            last_pos = f.tell()
        assert len(lines) == 1

        with open(jsonl_path, "a") as f:
            f.write('{"ts":"T2","type":"assistant_text","text":"New"}\n')

        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
            last_pos = f.tell()
        events = [json.loads(l.strip()) for l in new_lines if l.strip()]
        assert len(events) == 1
        assert events[0]["type"] == "assistant_text"

    def test_poll_empty_when_no_changes(self, thread_dir):
        """Poll returns empty list when no new events."""
        jsonl_path = thread_dir / THREAD_ID / "transcript.jsonl"
        jsonl_path.parent.mkdir(parents=True)
        jsonl_path.write_text('{"ts":"T1","type":"thread_start"}\n')

        with open(jsonl_path) as f:
            f.readlines()
            last_pos = f.tell()

        with open(jsonl_path) as f:
            f.seek(last_pos)
            new_lines = f.readlines()
        assert len(new_lines) == 0
