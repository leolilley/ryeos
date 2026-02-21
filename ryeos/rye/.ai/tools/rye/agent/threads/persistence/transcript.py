# rye:signed:2026-02-21T05:56:40Z:dc1281c1ac3c02cf94cb30488044d548ae43f73d3279d2aadee2dc5839cbac3d:r-3m_x1l1-82jmaOhr7dZd5ehpusvG3kSWS6n5fp5q9aZDu1hwuOAgupzLkDjL58KeZ8nsSH_tvLyY7zK8rBCA==:9fbfabe975fa5a7f
"""
persistence/transcript.py: Thread execution transcript (JSONL)

Provides write_event() interface expected by EventEmitter.
Events are appended to .ai/threads/{thread_id}/transcript.jsonl
as newline-delimited JSON for crash resilience.
"""

__version__ = "1.4.0"
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
        self._project_path = project_path
        self._dir = project_path / AI_DIR / "threads" / thread_id
        self._dir.mkdir(parents=True, exist_ok=True)
        self._path = self._dir / "transcript.jsonl"
        self._events: List[Dict[str, Any]] = []

    def write_event(self, thread_id: str, event_type: str, payload: Dict) -> None:
        """Append event to JSONL file and in-memory list."""
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

    def get_events(self) -> List[Dict[str, Any]]:
        """Return accumulated events."""
        return list(self._events)

    @property
    def knowledge_path(self) -> Path:
        """Path to the knowledge markdown file for this thread."""
        knowledge_dir = self._project_path / AI_DIR / "knowledge" / "agent" / "threads"
        thread_path = Path(self.thread_id)
        if thread_path.parent != Path("."):
            knowledge_dir = knowledge_dir / thread_path.parent
        return knowledge_dir / f"{thread_path.name}.md"

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

    def render_knowledge(
        self,
        directive: str = "",
        status: str = "completed",
        model: str = "",
        cost: Optional[Dict] = None,
    ) -> Optional[Path]:
        """Render transcript as a signed knowledge entry.

        Produces a cognition-framed markdown file in .ai/knowledge/threads/
        with YAML frontmatter. Signed via KnowledgeMetadataStrategy.

        Called at each checkpoint (same cadence as JSONL signing) so the
        knowledge file stays in sync with the signed transcript.

        Returns the path to the knowledge file, or None if no events.
        """
        if not self._path.exists():
            return None

        events = []
        with open(self._path) as f:
            for line in f:
                line = line.strip()
                if not line:
                    continue
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    continue

        if not events:
            return None

        cost = cost or {}
        # Use thread_id path components for category
        thread_path = Path(self.thread_id)
        if thread_path.parent != Path("."):
            category = f"agent/threads/{thread_path.parent}"
        else:
            category = "agent/threads"
        safe_id = thread_path.name
        created_at = ""
        for e in events:
            if e.get("timestamp"):
                from datetime import datetime, timezone
                created_at = datetime.fromtimestamp(
                    e["timestamp"], tz=timezone.utc
                ).strftime("%Y-%m-%dT%H:%M:%SZ")
                break

        elapsed = cost.get("elapsed_seconds", 0)
        if elapsed >= 60:
            duration_str = f"{elapsed / 60:.1f}m"
        else:
            duration_str = f"{elapsed:.1f}s"

        frontmatter = (
            f"```yaml\n"
            f"id: {safe_id}\n"
            f'title: "{directive or self.thread_id}"\n'
            f"entry_type: thread_transcript\n"
            f"category: {category}\n"
            f'version: "1.0.0"\n'
            f"author: rye\n"
            f"created_at: {created_at}\n"
            f"thread_id: {self.thread_id}\n"
            f"directive: {directive}\n"
            f"status: {status}\n"
            f"model: {model}\n"
            f"duration: {duration_str}\n"
            f"elapsed_seconds: {elapsed:.2f}\n"
            f"turns: {cost.get('turns', 0)}\n"
            f"input_tokens: {cost.get('input_tokens', 0)}\n"
            f"output_tokens: {cost.get('output_tokens', 0)}\n"
            f"spend: {cost.get('spend', 0)}\n"
            f"tags: [thread, {status}]\n"
            f"```\n\n"
        )

        parts = [frontmatter]
        parts.append(f"# {directive or self.thread_id}\n\n")

        turn = 0
        for event in events:
            et = event.get("event_type", "")
            if et == "cognition_in":
                turn += 1
            # Skip completion/error events — we regenerate the footer from
            # the authoritative cost dict so elapsed time is accurate.
            if et in ("thread_completed", "thread_error"):
                continue
            chunk = self._render_cognition_event(event, turn)
            if chunk:
                parts.append(chunk)

        # Append footer from authoritative cost (not the stale event snapshot)
        tokens = cost.get("input_tokens", 0) + cost.get("output_tokens", 0)
        spend = cost.get("spend", 0)
        turns = cost.get("turns", 0)
        parts.append(f"---\n\n**{'Completed' if status == 'completed' else 'Error'}**"
                      f" -- {turns} turns, {tokens} tokens, ${spend:.4f}, {duration_str}\n")

        content = "".join(parts)

        # Mirror thread directory structure under knowledge/agent/threads/
        # e.g. thread_id "test/tools/file_system/write_file-123" →
        #   .ai/knowledge/agent/threads/test/tools/file_system/write_file-123.md
        knowledge_dir = self._project_path / AI_DIR / "knowledge" / "agent" / "threads"
        # Use the thread_id path components for subdirectories
        thread_path = Path(self.thread_id)
        if thread_path.parent != Path("."):
            knowledge_dir = knowledge_dir / thread_path.parent
        knowledge_dir.mkdir(parents=True, exist_ok=True)
        knowledge_path = knowledge_dir / f"{thread_path.name}.md"

        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

        signature = MetadataManager.create_signature(ItemType.KNOWLEDGE, content)
        signed_content = signature + content

        knowledge_path.write_text(signed_content, encoding="utf-8")
        return knowledge_path

    @staticmethod
    def _render_cognition_event(event: Dict, turn: int) -> str:
        """Render a single event as a cognition thread fragment."""
        event_type = event.get("event_type", "")
        payload = event.get("payload", {})

        if event_type == "cognition_in":
            role = payload.get("role", "user")
            if role == "tool":
                return ""
            return f"## Input — Turn {turn}\n\n{payload.get('text', '')}\n\n"

        if event_type == "cognition_out":
            text = payload.get("text", "")
            return f"### Response — Turn {turn}\n\n{text}\n\n"

        if event_type == "tool_call_start":
            tool = payload.get("tool", "unknown")
            input_data = payload.get("input", {})
            try:
                input_str = json.dumps(input_data, indent=2)
            except Exception:
                input_str = str(input_data)
            return f"### Tool: {tool}\n\n```json\n{input_str}\n```\n\n"

        if event_type == "tool_call_result":
            output = payload.get("output", "")
            error = payload.get("error")
            if error:
                return f"### Error\n\n{error}\n\n"
            return f"### Result\n\n```\n{output}\n```\n\n"

        if event_type == "thread_completed":
            cost = payload.get("cost", {})
            tokens = cost.get("input_tokens", 0) + cost.get("output_tokens", 0)
            spend = cost.get("spend", 0)
            turns = cost.get("turns", 0)
            elapsed = cost.get("elapsed_seconds", 0)
            if elapsed >= 60:
                dur = f"{elapsed / 60:.1f}m"
            else:
                dur = f"{elapsed:.1f}s"
            return (
                f"---\n\n"
                f"**Completed** -- {turns} turns, {tokens} tokens, ${spend:.4f}, {dur}\n"
            )

        if event_type == "thread_error":
            return f"---\n\n**Error** -- {payload.get('error', 'unknown')}\n"

        return ""
