# rye:signed:2026-02-16T09:56:55Z:f48bd1dfc2a7246749f7d8eb198aa8000b7a9ea4dc513946efb612d3b4175827:JeVN0ppF8fgmEek4cglBvG_mjx_zhBcoLdeKI4ha_Jrh0VVV5_aYqnj0t-mK0ODEIVVoU_ph109PA-qtZv6nBA==:440443d0858f0199
"""
persistence/transcript.py: Thread execution transcript (JSONL)

Provides write_event() interface expected by EventEmitter.
Events are appended to .ai/threads/{thread_id}/transcript.jsonl
as newline-delimited JSON for crash resilience.
"""

__version__ = "1.3.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Thread transcript JSONL persistence"

import json
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR


class Transcript:
    """Append-only JSONL transcript for a thread.

    Each event is written as a single JSON line, flushed immediately.
    This survives crashes — partial transcripts are still readable.
    """

    def __init__(self, thread_id: str, project_path: Path):
        self.thread_id = thread_id
        self._dir = project_path / AI_DIR / "threads" / thread_id
        self._dir.mkdir(parents=True, exist_ok=True)
        self._path = self._dir / "transcript.jsonl"
        self._events: List[Dict[str, Any]] = []

    def write_event(self, thread_id: str, event_type: str, payload: Dict) -> None:
        """Append event to JSONL file, in-memory list, and stream to transcript.md."""
        entry = {
            "timestamp": time.time(),
            "thread_id": thread_id,
            "event_type": event_type,
            "payload": payload,
        }
        self._events.append(entry)
        with open(self._path, "a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
            f.flush()

        # Stream markdown chunk to transcript.md
        chunk = self._render_event(entry)
        if chunk:
            md_path = self._dir / "transcript.md"
            with open(md_path, "a", encoding="utf-8") as f:
                f.write(chunk)
                f.flush()

    def get_events(self) -> List[Dict[str, Any]]:
        """Return accumulated events."""
        return list(self._events)

    def reconstruct_messages(self) -> Optional[List[Dict]]:
        """Reconstruct conversation messages from transcript.jsonl.

        Rebuilds the exact message format that runner.py uses internally:
          - user messages from cognition_in
          - assistant messages from cognition_out WITH tool_calls from tool_call_start
          - tool result messages from tool_call_result

        The tool_calls on assistant messages are critical — without them,
        providers like Anthropic reject the conversation (orphaned tool_results).

        Two-pass reconstruction: first pass collects all events, second pass
        groups tool_call_start events by their preceding cognition_out since
        the runner interleaves start/result pairs sequentially.
        """
        if not self._path.exists():
            return None

        # Pass 1: Parse all events
        events = []
        with open(self._path) as f:
            for line_no, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    from ..errors import TranscriptCorrupt
                    raise TranscriptCorrupt(str(self._path), line_no, line[:100])

        if not events:
            return None

        # Pass 2: Group tool_call_starts per cognition_out turn.
        # Events arrive as: cognition_out → (start → result)+ → cognition_in
        # We need all starts attached to the assistant message, not just the first.
        turn_tool_calls = {}  # event_index_of_cognition_out → [tool_call dicts]
        current_assistant_idx = None
        for i, event in enumerate(events):
            et = event.get("event_type", "")
            if et == "cognition_out":
                current_assistant_idx = i
            elif et in ("cognition_in",):
                current_assistant_idx = None
            elif et == "tool_call_start" and current_assistant_idx is not None:
                p = event.get("payload", {})
                tc = {
                    "name": p.get("tool", ""),
                    "id": p.get("call_id", ""),
                    "input": p.get("input", {}),
                }
                turn_tool_calls.setdefault(current_assistant_idx, []).append(tc)

        # Pass 3: Build messages
        messages = []
        for i, event in enumerate(events):
            et = event.get("event_type", "")
            p = event.get("payload", {})

            if et == "cognition_in":
                # Skip tool role — tool results are captured by tool_call_result
                if p.get("role") == "tool":
                    continue
                messages.append({
                    "role": p.get("role", "user"),
                    "content": p.get("text", ""),
                })

            elif et == "cognition_out":
                msg = {
                    "role": "assistant",
                    "content": p.get("text", ""),
                }
                if i in turn_tool_calls:
                    msg["tool_calls"] = turn_tool_calls[i]
                messages.append(msg)

            elif et == "tool_call_result":
                call_id = p.get("call_id", "")
                output = p.get("output", "")
                error = p.get("error")
                messages.append({
                    "role": "tool",
                    "tool_call_id": call_id,
                    "content": error or output,
                })

        return messages if messages else None

    def render_markdown(self) -> None:
        """Render transcript.jsonl to transcript.md for human reading."""
        if not self._path.exists():
            return

        md_path = self._dir / "transcript.md"
        parts = []

        with open(self._path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    continue

                chunk = self._render_event(event)
                if chunk:
                    parts.append(chunk)

        with open(md_path, "w", encoding="utf-8") as f:
            f.write("".join(parts))

    @staticmethod
    def _render_event(event: Dict) -> str:
        """Render a single event to markdown fragment."""
        event_type = event.get("event_type", "")
        payload = event.get("payload", {})

        if event_type == "thread_started":
            return (
                f"# {payload.get('directive', 'Thread')}\n\n"
                f"**Thread ID:** `{event.get('thread_id', '')}`\n"
                f"**Model:** {payload.get('model', 'unknown')}\n"
                f"**Started:** {event.get('timestamp', '')}\n\n---\n\n"
            )

        if event_type == "cognition_in":
            role = payload.get("role", "user")
            if role == "tool":
                return ""
            return f"## {role.title()}\n\n{payload.get('text', '')}\n\n---\n\n"

        if event_type == "cognition_out":
            text = payload.get("text", "")
            return f"**Assistant:**\n\n{text}\n\n"

        if event_type == "tool_call_start":
            tool = payload.get("tool", "unknown")
            call_id = payload.get("call_id", "?")
            input_data = payload.get("input", {})
            try:
                input_str = json.dumps(input_data, indent=2)
            except Exception:
                input_str = str(input_data)
            return (
                f"**Tool Call:** `{tool}` (ID: `{call_id}`)\n\n"
                f"```json\n{input_str}\n```\n\n"
            )

        if event_type == "tool_call_result":
            call_id = payload.get("call_id", "?")
            output = payload.get("output", "")
            error = payload.get("error")
            result = f"**Tool Result** (ID: `{call_id}`)\n\n"
            if error:
                result += f"**Error:** {error}\n\n"
            else:
                result += f"```\n{output}\n```\n\n"
            return result

        if event_type == "thread_completed":
            cost = payload.get("cost", {})
            tokens = cost.get("input_tokens", 0) + cost.get("output_tokens", 0)
            spend = cost.get("spend", 0)
            return (
                f"## Completed\n\n"
                f"**Total Tokens:** {tokens}\n"
                f"**Total Cost:** ${spend:.6f}\n\n"
            )

        if event_type == "thread_error":
            return f"## Error\n\n{payload.get('error', 'unknown')}\n\n"

        if event_type == "thread_resumed":
            new_id = payload.get("new_thread_id", "unknown")
            directive = payload.get("directive", "")
            preview = payload.get("message_preview", "")
            turns = payload.get("reconstructed_turns", 0)
            return (
                f"## Resumed → `{new_id}`\n\n"
                f"**Directive:** {directive}\n"
                f"**Reconstructed turns:** {turns}\n"
                f"**Message:** {preview}\n\n"
            )

        if event_type == "thread_handoff":
            new_id = payload.get("new_thread_id", "unknown")
            directive = payload.get("directive", "")
            summary = "yes" if payload.get("summary_generated") else "no"
            trailing = payload.get("trailing_turns", 0)
            return (
                f"## Handoff → `{new_id}`\n\n"
                f"**Directive:** {directive}\n"
                f"**Summary generated:** {summary}\n"
                f"**Trailing turns carried:** {trailing}\n\n"
                f"This thread's context limit was reached. "
                f"Execution continues in the new thread above.\n\n"
            )

        if event_type == "context_limit_reached":
            ratio = payload.get("usage_ratio", 0)
            used = payload.get("tokens_used", 0)
            limit = payload.get("tokens_limit", 0)
            return (
                f"## Context Limit Reached\n\n"
                f"**Usage:** {ratio:.1%} ({used}/{limit} tokens)\n\n"
            )

        return ""
