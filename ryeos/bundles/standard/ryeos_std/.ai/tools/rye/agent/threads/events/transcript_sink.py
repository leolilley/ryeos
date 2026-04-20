# rye:signed:2026-04-20T05:46:18Z:df4cc743ad35a5a8ab87d5d79d110cc6098a6f25db997f59be88945411580a2d:QspDs0Qh3oXvucvQDbigO4vBwpxxxb_I8NF8XvBK0fmMqHTdOFG8N6kXH7MQp36X-GichRTJqiQ5lXdT-gUxCA:4b987fd4e40303ac
__version__ = "1.2.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/events"
__tool_description__ = "Streaming sink that appends daemon events and mirrors knowledge markdown"

import asyncio
import json
from pathlib import Path
from typing import Any, Optional

from rye.runtime.daemon_rpc import require_daemon_runtime_context


class TranscriptSink:
    """Sink that appends daemon-owned stream events and mirrors knowledge markdown.

    Implements the write/close interface expected by the streaming provider
    adapter's fan-out. Each SSE event is parsed and appended through the
    daemon chain journal so live subscribers receive gap-free streaming.

    When knowledge_path is set, text deltas are also appended to the
    knowledge markdown file so human-readable thread output still forms live.
    """

    def __init__(
        self,
        thread_id: str,
        response_format: str = "content_blocks",
        knowledge_path: Optional[Path] = None,
        turn: int = 0,
    ):
        self.thread_id = thread_id
        self._response_format = response_format
        self._knowledge_path = knowledge_path
        self._turn = turn
        self._kfh = None
        self._wrote_turn_header = False
        self._client = None
        self._resolved_thread_id = thread_id
        self._delta_count = 0
        self._text_parts: list[str] = []
        self._thinking_parts: list[str] = []
        self._tool_calls: list[dict[str, str]] = []
        self._last_message_delta: dict[str, Any] | None = None
        self._stream_opened = False
        self._pending_deltas: list[dict[str, Any]] = []
        self._flush_interval = 0.05  # 50ms batching window

    def _ensure_client(self):
        if self._client is None:
            client, resolved_thread_id, _ = require_daemon_runtime_context(
                thread_id=self.thread_id
            )
            self._client = client
            self._resolved_thread_id = resolved_thread_id

    def _ensure_knowledge_open(self):
        if self._kfh is None and self._knowledge_path:
            self._knowledge_path.parent.mkdir(parents=True, exist_ok=True)
            self._kfh = open(self._knowledge_path, "a")

    async def write(self, event: str) -> None:
        """Parse an SSE event string and append token deltas through the daemon."""
        if not event or event == "[DONE]":
            return

        try:
            data = json.loads(event)
        except (json.JSONDecodeError, ValueError):
            return

        delta = self._extract_delta(data)
        if not delta:
            return

        self._ensure_client()
        if not self._stream_opened:
            self._stream_opened = True
            await self._append_events(
                [
                    {
                        "event_type": "stream_opened",
                        "storage_class": "indexed",
                        "payload": {
                            "turn": self._turn,
                            "response_format": self._response_format,
                        },
                    }
                ]
            )

        self._record_delta(delta)
        self._pending_deltas.append(
            {
                "event_type": "token_delta",
                "storage_class": "journal_only",
                "payload": delta,
            }
        )
        await self._flush_deltas()

        self._write_knowledge_delta(delta)

    async def _flush_deltas(self, force: bool = False) -> None:
        """Flush pending token_delta events in a single batched RPC call.

        Accumulates deltas for up to _flush_interval seconds before
        flushing, unless force=True which flushes immediately.
        """
        if not self._pending_deltas:
            return
        if not force and len(self._pending_deltas) < 16:
            # Yield briefly to let more deltas accumulate within the batch window
            await asyncio.sleep(0)
            if not self._pending_deltas:
                return
        batch = self._pending_deltas
        self._pending_deltas = []
        await self._append_events(batch)

    async def _append_events(self, events: list[dict[str, Any]]) -> None:
        await asyncio.to_thread(
            self._client.append_events,
            self._resolved_thread_id,
            events,
        )

    def _record_delta(self, delta: dict[str, Any]) -> None:
        self._delta_count += 1
        delta_type = delta.get("type", "")
        if delta_type == "text":
            self._text_parts.append(delta.get("text", ""))
        elif delta_type == "thinking":
            self._thinking_parts.append(delta.get("text", ""))
        elif delta_type == "tool_call_start":
            self._tool_calls.append(
                {
                    "id": delta.get("id", ""),
                    "name": delta.get("name", ""),
                }
            )
        elif delta_type == "message_delta":
            self._last_message_delta = delta

    def _snapshot_payload(self) -> dict[str, Any]:
        payload: dict[str, Any] = {
            "turn": self._turn,
            "response_format": self._response_format,
            "delta_count": self._delta_count,
            "text": "".join(self._text_parts),
        }
        thinking = "".join(self._thinking_parts)
        if thinking:
            payload["thinking"] = thinking
        if self._tool_calls:
            payload["tool_calls"] = self._tool_calls
        if self._last_message_delta:
            payload["message_delta"] = self._last_message_delta
        return payload

    def _write_knowledge_delta(self, delta: dict) -> None:
        """Append delta content to the knowledge markdown file."""
        if not self._knowledge_path:
            return

        delta_type = delta.get("type", "")

        if delta_type == "thinking":
            self._ensure_knowledge_open()
            text = delta.get("text", "").strip()
            if text:
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
                self._kfh.write(f"\n{quoted}\n\n")
                self._kfh.flush()

        elif delta_type == "text":
            self._ensure_knowledge_open()
            if not self._wrote_turn_header:
                self._kfh.write(f"\n### Response — Turn {self._turn}\n\n")
                self._wrote_turn_header = True
            self._kfh.write(delta.get("text", ""))
            self._kfh.flush()

        elif delta_type == "tool_call_start":
            self._ensure_knowledge_open()
            if not self._wrote_turn_header:
                self._kfh.write(f"\n### Response — Turn {self._turn}\n\n")
                self._wrote_turn_header = True
            name = delta.get("name", "unknown")
            self._kfh.write(f"\n\n### Tool: {name}\n\n")
            self._kfh.flush()

    def _extract_delta(self, data: dict) -> Optional[dict]:
        """Extract text/tool delta from a parsed SSE event."""
        if self._response_format == "chat_completion":
            return self._extract_openai_delta(data)
        if self._response_format == "complete_chunks":
            return self._extract_gemini_delta(data)
        return self._extract_anthropic_delta(data)

    def _extract_openai_delta(self, data: dict) -> Optional[dict]:
        choices = data.get("choices", [])
        if not choices:
            return None
        delta = choices[0].get("delta", {})
        if "content" in delta and delta["content"]:
            return {"type": "text", "text": delta["content"]}
        if "tool_calls" in delta:
            tc = delta["tool_calls"]
            return {"type": "tool_call_delta", "tool_calls": tc}
        return None

    def _extract_gemini_delta(self, data: dict) -> Optional[dict]:
        candidates = data.get("candidates", [])
        if not candidates:
            return None
        parts = (candidates[0].get("content") or {}).get("parts", [])
        for part in parts:
            if "functionCall" in part:
                fc = part["functionCall"]
                return {"type": "tool_call_start", "id": "", "name": fc.get("name", "")}
            if "text" in part:
                if part.get("thought"):
                    return {"type": "thinking", "text": part["text"]}
                return {"type": "text", "text": part["text"]}
        return None

    def _extract_anthropic_delta(self, data: dict) -> Optional[dict]:
        event_type = data.get("type", "")

        if event_type == "content_block_delta":
            block_delta = data.get("delta", {})
            delta_type = block_delta.get("type", "")
            if delta_type == "text_delta":
                return {"type": "text", "text": block_delta.get("text", "")}
            if delta_type == "input_json_delta":
                return {"type": "tool_input_delta", "partial_json": block_delta.get("partial_json", "")}

        if event_type == "content_block_start":
            block = data.get("content_block", {})
            if block.get("type") == "tool_use":
                return {
                    "type": "tool_call_start",
                    "id": block.get("id", ""),
                    "name": block.get("name", ""),
                }

        if event_type == "message_delta":
            delta = data.get("delta", {})
            usage = data.get("usage", {})
            return {
                "type": "message_delta",
                "stop_reason": delta.get("stop_reason"),
                "output_tokens": usage.get("output_tokens", 0),
            }

        return None

    async def close(self) -> None:
        # Flush any remaining buffered token deltas before closing
        if self._pending_deltas:
            self._ensure_client()
            await self._flush_deltas(force=True)
        if self._stream_opened:
            self._ensure_client()
            snapshot = self._snapshot_payload()
            await self._append_events(
                [
                    {
                        "event_type": "stream_snapshot",
                        "storage_class": "indexed",
                        "payload": snapshot,
                    },
                    {
                        "event_type": "stream_closed",
                        "storage_class": "indexed",
                        "payload": {
                            "turn": self._turn,
                            "response_format": self._response_format,
                            "delta_count": self._delta_count,
                        },
                    },
                ]
            )
            self._stream_opened = False
        if self._kfh is not None:
            self._kfh.write("\n")
            self._kfh.flush()
            self._kfh.close()
            self._kfh = None
