# rye:signed:2026-04-20T05:46:18Z:44640ec7ccdfe62f03d4ea0519f7739c1dafbffdf96398eec4650c92ed05f738:xZryEjwgHfcTkl7QBa6gf1PiatcM3vgCT-LPRH1mZaFOnXLkSA6biXu5816WMEKsOB91nBLzBcqEDCpJk23cAw:4b987fd4e40303ac
"""Thread history helpers for daemon-backed and file-backed runtimes."""

__version__ = "1.5.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/persistence"
__tool_description__ = "Thread history helpers for daemon-backed thread events"

from datetime import datetime, timezone
import json
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR, STATE_THREADS_REL, KNOWLEDGE_THREADS_REL
from rye.runtime.daemon_rpc import ThreadLifecycleClient, get_daemon_runtime_context


class Transcript:
    """Thread history helper backed by daemon events when runtime context is present."""

    def __init__(self, thread_id: str, project_path: Path):
        self.thread_id = thread_id
        self._project_path = project_path
        self._dir = project_path / AI_DIR / STATE_THREADS_REL / thread_id
        self._dir.mkdir(parents=True, exist_ok=True)
        self._path = self._dir / "transcript.jsonl"
        self._events: List[Dict[str, Any]] = []
        self._daemon_client: Optional[ThreadLifecycleClient] = None
        self._daemon_socket_path: Optional[str] = None

    def write_event(self, thread_id: str, event_type: str, payload: Dict) -> None:
        """Append an event through the daemon when available, else fall back to JSONL."""
        entry = {
            "timestamp": time.time(),
            "thread_id": thread_id,
            "event_type": event_type,
            "payload": payload,
        }
        self._events.append(entry)

        context = get_daemon_runtime_context()
        socket_path = context.get("socket_path")
        if socket_path:
            if self._daemon_client is None or self._daemon_socket_path != socket_path:
                self._daemon_client = ThreadLifecycleClient(socket_path)
                self._daemon_socket_path = socket_path
            self._daemon_client.append_event(
                thread_id,
                event_type,
                "journal_only" if event_type == "token_delta" else "indexed",
                payload,
            )
            return

        with open(self._path, "a") as f:
            f.write(json.dumps(entry, default=str) + "\n")
            f.flush()

    def get_events(self) -> List[Dict[str, Any]]:
        """Return thread events, preferring daemon replay when available."""
        return self._load_events(allow_corrupt=False)

    def write_capabilities(self, tool_defs: list, tree: str = "") -> Path:
        """Write signed capabilities.md alongside transcript.jsonl.

        Markdown format: tool_defs in a JSON fenced block, tree as plain text.
        Signed via MetadataManager like knowledge entries.
        Atomic write (tmp + rename) so readers never see partial content.
        Returns the path to the written file.
        """
        from rye.utils.metadata_manager import MetadataManager
        from rye.constants import ItemType

        defs_json = json.dumps(tool_defs, indent=2, default=str, ensure_ascii=False)
        body = f"# Capabilities\n\n## Tool Definitions\n\n```json\n{defs_json}\n```\n"
        if tree:
            body += f"\n## Capabilities Tree\n\n```\n{tree}\n```\n"

        signature = MetadataManager.create_signature(ItemType.KNOWLEDGE, body)
        signed_content = signature + body

        caps_path = self._dir / "capabilities.md"
        tmp_path = caps_path.with_suffix(".md.tmp")
        tmp_path.write_text(signed_content, encoding="utf-8")
        tmp_path.rename(caps_path)
        return caps_path

    @property
    def knowledge_path(self) -> Path:
        """Path to the knowledge markdown file for this thread."""
        knowledge_dir = self._project_path / AI_DIR / KNOWLEDGE_THREADS_REL
        thread_path = Path(self.thread_id)
        if thread_path.parent != Path("."):
            knowledge_dir = knowledge_dir / thread_path.parent
        return knowledge_dir / f"{thread_path.name}.md"

    def reconstruct_messages(self) -> Optional[List[Dict]]:
        """Reconstruct conversation messages from daemon-backed history or JSONL fallback.

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
        events = self._load_events(allow_corrupt=False)

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

    def render_knowledge_transcript(
        self,
        directive: str = "",
        status: str = "completed",
        model: str = "",
        cost: Optional[Dict] = None,
        permissions: Optional[list] = None,
    ) -> Optional[Path]:
        """Render transcript as a signed knowledge entry.

        Produces a cognition-framed markdown file in .ai/knowledge/threads/
        with YAML frontmatter. Signed via KnowledgeMetadataStrategy.

        Called at each checkpoint so the knowledge file stays in sync with
        daemon-backed history, falling back to JSONL only outside daemon mode.

        Returns the path to the knowledge file, or None if no events.
        """
        events = self._load_events(allow_corrupt=True)

        if not events:
            return None

        cost = cost or {}
        # Derive name (must match filename stem) and category (must match path)
        thread_path = Path(self.thread_id)
        name = thread_path.name
        if thread_path.parent != Path("."):
            category = f"agent/threads/{thread_path.parent}"
        else:
            category = "agent/threads"
        # Title is the leaf directive name, not the full path
        directive_leaf = directive.rsplit("/", 1)[-1] if directive else name
        created_at = ""
        for e in events:
            created_at = self._event_created_at(e)
            if created_at:
                break

        elapsed = cost.get("elapsed_seconds", 0)
        if elapsed >= 60:
            duration_str = f"{elapsed / 60:.1f}m"
        else:
            duration_str = f"{elapsed:.1f}s"

        frontmatter = (
            f"```yaml\n"
            f"name: {name}\n"
            f'title: "{directive_leaf}"\n'
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
        )
        if permissions:
            perms_str = ", ".join(permissions)
            frontmatter += f"permissions: [{perms_str}]\n"
        frontmatter += f"capabilities_ref: {AI_DIR}/{STATE_THREADS_REL}/{self.thread_id}/capabilities.md\n"
        frontmatter += (
            f"```\n\n"
        )

        parts = [frontmatter]
        parts.append(f"# {directive or self.thread_id}\n\n")

        turn = 0
        for event in events:
            et = event.get("event_type", "")
            if et == "cognition_in":
                turn += 1
            # Skip completion event — we regenerate the footer from
            # the authoritative cost dict so elapsed time is accurate.
            if et == "thread_completed":
                continue
            chunk = self._render_cognition_event(event, turn)
            if chunk:
                parts.append(chunk)

        # Append footer from authoritative cost (not the stale event snapshot)
        tokens = cost.get("input_tokens", 0) + cost.get("output_tokens", 0)
        spend = cost.get("spend", 0)
        turns = cost.get("turns", 0)
        status_labels = {"completed": "Completed", "running": "Running", "error": "Error"}
        label = status_labels.get(status, status.title())
        parts.append(f"---\n\n**{label}**"
                      f" -- {turns} turns, {tokens} tokens, ${spend:.4f}, {duration_str}\n")

        content = "".join(parts)

        # Mirror thread directory structure under knowledge/agent/threads/
        # e.g. thread_id "test/tools/file_system/write_file-123" →
        #   .ai/knowledge/agent/threads/test/tools/file_system/write_file-123.md
        knowledge_dir = self._project_path / AI_DIR / KNOWLEDGE_THREADS_REL
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

    def _load_events(self, allow_corrupt: bool) -> List[Dict[str, Any]]:
        daemon_events = self._load_daemon_events()
        if daemon_events is not None:
            return daemon_events
        return self._load_file_events(allow_corrupt=allow_corrupt)

    def _load_daemon_events(self) -> Optional[List[Dict[str, Any]]]:
        context = get_daemon_runtime_context()
        socket_path = context.get("socket_path")
        if not socket_path:
            return None

        client = ThreadLifecycleClient(socket_path)
        cursor = None
        loaded: List[Dict[str, Any]] = []

        while True:
            page = client.replay_events(
                thread_id=self.thread_id,
                after_chain_seq=cursor,
                limit=200,
            )
            events = page.get("events") or []
            if not events:
                break
            loaded.extend(self._normalize_daemon_event(event) for event in events)
            cursor = page.get("next_cursor")
            if cursor is None:
                break

        return loaded

    def _load_file_events(self, allow_corrupt: bool) -> List[Dict[str, Any]]:
        if not self._path.exists():
            return []

        events = []
        with open(self._path) as f:
            for line_no, line in enumerate(f, 1):
                line = line.strip()
                if not line:
                    continue
                try:
                    events.append(json.loads(line))
                except json.JSONDecodeError:
                    if allow_corrupt:
                        continue
                    from ..errors import TranscriptCorrupt
                    raise TranscriptCorrupt(str(self._path), line_no, line[:100])

        return events

    @staticmethod
    def _normalize_daemon_event(event: Dict[str, Any]) -> Dict[str, Any]:
        normalized = {
            "thread_id": event.get("thread_id", ""),
            "event_type": event.get("event_type", ""),
            "payload": event.get("payload") or {},
            "ts": event.get("ts"),
        }
        if event.get("ts"):
            try:
                normalized["timestamp"] = datetime.fromisoformat(
                    event["ts"].replace("Z", "+00:00")
                ).timestamp()
            except ValueError:
                pass
        return normalized

    @staticmethod
    def _event_created_at(event: Dict[str, Any]) -> str:
        if event.get("ts"):
            return str(event["ts"])
        if event.get("timestamp"):
            return datetime.fromtimestamp(
                event["timestamp"], tz=timezone.utc
            ).strftime("%Y-%m-%dT%H:%M:%SZ")
        return ""

    # Maximum characters for a single tool result in the knowledge markdown.
    # Full output is preserved in the daemon event store or JSONL fallback.
    _MAX_RESULT_CHARS = 2000

    # Maximum characters for file content shown in tool call inputs.
    # Large file writes are summarised to save context.
    _MAX_FILE_CONTENT_CHARS = 500

    @staticmethod
    def _render_cognition_event(event: Dict, turn: int) -> str:
        """Render a single event as a cognition thread fragment."""
        event_type = event.get("event_type", "")
        payload = event.get("payload", {})

        if event_type == "system_prompt":
            text = payload.get("text", "")
            layers = payload.get("layers", [])
            layer_str = ", ".join(layers) if layers else "custom"
            return f"## System Prompt ({layer_str})\n\n{text}\n\n"

        if event_type == "context_injected":
            # Context blocks are already included in cognition_in — skip
            # rendering them separately to avoid duplicate content.
            return ""

        if event_type == "cognition_in":
            role = payload.get("role", "user")
            if role == "tool":
                return ""
            prefix = "---\n\n" if turn == 1 else ""
            return f"{prefix}## Input — Turn {turn}\n\n{payload.get('text', '')}\n\n"

        if event_type == "cognition_reasoning":
            text = payload.get("text", "").strip()
            if text:
                # Collapse runs of blank lines into single blank line
                lines = text.splitlines()
                collapsed = []
                prev_blank = False
                for line in lines:
                    blank = not line.strip()
                    if blank and prev_blank:
                        continue
                    collapsed.append(line)
                    prev_blank = blank
                quoted = "\n".join(f"> *{line}*" if line.strip() else ">" for line in collapsed)
                return f"\n{quoted}\n\n"
            return ""

        if event_type == "cognition_out":
            text = payload.get("text", "")
            if text.strip():
                return f"### Response — Turn {turn}\n\n{text}\n\n"
            return f"### Response — Turn {turn}\n\n"

        if event_type == "tool_call_start":
            tool = payload.get("tool", "unknown")
            input_data = payload.get("input", {})
            input_data = Transcript._condense_tool_input(tool, input_data)
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
            cleaned = Transcript._clean_tool_output(output)
            return f"### Result\n\n```\n{cleaned}\n```\n\n"

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

        if event_type in {"thread_error", "thread_failed"}:
            return f"\n> **Error**: {payload.get('error', 'unknown')}\n\n"

        return ""

    @staticmethod
    def _parse_output(raw: str) -> Any:
        """Try to parse a tool output string as structured data.

        Tool outputs may arrive as JSON (double quotes) or Python repr
        (single quotes). Returns the parsed dict/list on success, or the
        original string on failure.
        """
        if not isinstance(raw, str):
            return raw
        stripped = raw.strip()
        if not stripped or stripped[0] not in ("{", "["):
            return raw
        # Try JSON first
        try:
            return json.loads(stripped)
        except (json.JSONDecodeError, ValueError):
            pass
        # Try Python literal (handles single-quoted dicts from repr())
        import ast
        try:
            return ast.literal_eval(stripped)
        except Exception:
            return raw

    @staticmethod
    def _clean_tool_output(raw: str) -> str:
        """Extract the meaningful content from a tool result string.

        Handles the common rye tool result wrapper:
          {'status': 'success', 'data': {'output': '...', ...}, ...}

        Strips internal metadata (_artifact_ref, _artifact_note),
        deduplicates stdout/output when identical, and caps length.
        """
        parsed = Transcript._parse_output(raw)

        if not isinstance(parsed, dict):
            text = str(raw)
            if len(text) > Transcript._MAX_RESULT_CHARS:
                return text[:Transcript._MAX_RESULT_CHARS] + "\n... (truncated)"
            return text

        # Remove internal metadata keys
        for key in ("_artifact_ref", "_artifact_note"):
            parsed.pop(key, None)

        # Extract the actual output from nested wrappers.
        # Prefer data.output > output > stdout, in that order.
        data = parsed.get("data", {})
        if isinstance(data, dict):
            for key in ("_artifact_ref", "_artifact_note"):
                data.pop(key, None)
            actual_output = data.get("output") or parsed.get("output") or parsed.get("stdout")
        else:
            actual_output = parsed.get("output") or parsed.get("stdout")

        # If we found a simple output string, use it directly
        if actual_output and isinstance(actual_output, str):
            # Include error info if present
            error = parsed.get("error") or (data.get("error") if isinstance(data, dict) else None)
            stderr = parsed.get("stderr") or (data.get("stderr", "") if isinstance(data, dict) else "")
            parts = [actual_output.rstrip()]
            if stderr and stderr.strip() and stderr.strip() != actual_output.strip():
                parts.append(f"[stderr] {stderr.strip()}")
            if error:
                parts.append(f"[error] {error}")
            text = "\n".join(parts)
        else:
            # Fallback: remove redundant fields and re-serialise
            # Drop stdout when identical to output
            output_val = parsed.get("output", "")
            stdout_val = parsed.get("stdout", "")
            if output_val and stdout_val and str(output_val).strip() == str(stdout_val).strip():
                parsed.pop("stdout", None)
            # Drop empty stderr
            if not parsed.get("stderr", "").strip():
                parsed.pop("stderr", None)
            # Drop exit_code 0 (success is the default assumption)
            if parsed.get("exit_code") == 0:
                parsed.pop("exit_code", None)
            # Drop redundant top-level status/success
            if parsed.get("status") == "success":
                parsed.pop("status", None)
            if parsed.get("success") is True:
                parsed.pop("success", None)
            try:
                text = json.dumps(parsed, indent=2, default=str)
            except Exception:
                text = str(parsed)

        if len(text) > Transcript._MAX_RESULT_CHARS:
            return text[:Transcript._MAX_RESULT_CHARS] + "\n... (truncated)"
        return text

    @staticmethod
    def _condense_tool_input(tool: str, input_data: Any) -> Any:
        """Condense tool call inputs to reduce context bloat.

        File write operations embed the full file content in the input,
        which can be very large. Since the file itself is the source of
        truth, we truncate long content values.
        """
        if not isinstance(input_data, dict):
            return input_data

        # For file-system write tools, truncate large content fields
        if "file-system/write" in tool or "file-system/create" in tool:
            content = input_data.get("content", "")
            if isinstance(content, str) and len(content) > Transcript._MAX_FILE_CONTENT_CHARS:
                lines = content.count("\n") + 1
                input_data = dict(input_data)  # shallow copy
                preview = content[:Transcript._MAX_FILE_CONTENT_CHARS]
                input_data["content"] = f"{preview}\n... ({lines} lines, {len(content)} chars total)"

        return input_data
