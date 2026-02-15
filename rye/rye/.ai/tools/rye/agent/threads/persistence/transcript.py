# rye:signed:2026-02-14T00:28:39Z:542f03f548d29464800e9bbd69d0499950d600f2e085f655cbe75a4634a3aa53:TbCgo_ZshG5u2BB2MMilorPFJCd9LajBobkS-l40eR6V6dpISNA0w7e5bW1equyZ-MNXl7VIVArVlzVkxc8OBQ==:440443d0858f0199
"""
persistence/transcript.py: Thread execution transcript (JSONL)

Provides write_event() interface expected by EventEmitter.
Events are appended to .ai/threads/{thread_id}/transcript.jsonl
as newline-delimited JSON for crash resilience.
"""

__version__ = "1.0.0"
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

        Used when state.json is missing or corrupt but transcript exists.
        Handles interleaved streaming events by using cognition_out
        (complete text) and ignoring cognition_out_delta (partial).
        """
        if not self._path.exists():
            return None

        messages = []

        with open(self._path) as f:
            for line_no, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    event = json.loads(line)
                except json.JSONDecodeError:
                    from ..errors import TranscriptCorrupt
                    raise TranscriptCorrupt(str(self._path), line_no, line[:100])

                event_type = event.get("event_type", "")
                payload = event.get("payload", {})

                if event_type == "cognition_in":
                    messages.append({
                        "role": payload.get("role", "user"),
                        "content": payload.get("text", ""),
                    })

                elif event_type == "cognition_out":
                    messages.append({
                        "role": "assistant",
                        "content": payload.get("text", ""),
                    })

                elif event_type == "tool_call_result":
                    call_id = payload.get("call_id", "")
                    output = payload.get("output", "")
                    error = payload.get("error")
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

        return ""
