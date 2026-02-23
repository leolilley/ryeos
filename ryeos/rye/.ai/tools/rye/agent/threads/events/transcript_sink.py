# rye:signed:2026-02-23T00:42:51Z:ff8400ed0811055174c6e88f6a06d1b9e1a0c87a23056b610f1a0bf059188f59:TNcX1J0w-VBz-GUMbZiMCEDOuQX97NcASfXSGM7xx_zkIsqnTSIT9y82FOG7oUGlV6vLt8vwTKn1GhxPVQBZDg==:9fbfabe975fa5a7f
__version__ = "1.1.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/events"
__tool_description__ = "Streaming sink that writes token_delta events to transcript JSONL and knowledge markdown"

import json
import time
from pathlib import Path
from typing import Any, Optional


class TranscriptSink:
    """Sink that writes token_delta events to both transcript JSONL and knowledge markdown.

    Implements the write/close interface expected by HttpClientPrimitive's
    streaming fan-out. Each SSE event is parsed and written as a JSONL line
    so `tail -f transcript.jsonl` shows tokens arriving in real-time.

    When knowledge_path is set, text deltas are also appended to the
    knowledge markdown file so `tail -f *.md` shows the response forming.
    render_knowledge_transcript() rewrites the file cleanly at each checkpoint.
    """

    def __init__(
        self,
        transcript_path: Path,
        thread_id: str,
        response_format: str = "content_blocks",
        knowledge_path: Optional[Path] = None,
        turn: int = 0,
    ):
        self.transcript_path = transcript_path
        self.thread_id = thread_id
        self._response_format = response_format
        self._knowledge_path = knowledge_path
        self._turn = turn
        self._fh = None
        self._kfh = None
        self._wrote_turn_header = False

    def _ensure_open(self):
        if self._fh is None:
            self._fh = open(self.transcript_path, "a")

    def _ensure_knowledge_open(self):
        if self._kfh is None and self._knowledge_path:
            self._knowledge_path.parent.mkdir(parents=True, exist_ok=True)
            self._kfh = open(self._knowledge_path, "a")

    async def write(self, event: str) -> None:
        """Parse an SSE event string and write token_delta to transcript + markdown."""
        if not event or event == "[DONE]":
            return

        try:
            data = json.loads(event)
        except (json.JSONDecodeError, ValueError):
            return

        delta = self._extract_delta(data)
        if not delta:
            return

        # Write JSONL token_delta
        entry = {
            "timestamp": time.time(),
            "thread_id": self.thread_id,
            "event_type": "token_delta",
            "payload": delta,
        }

        self._ensure_open()
        self._fh.write(json.dumps(entry, default=str) + "\n")
        self._fh.flush()

        # Write to knowledge markdown
        self._write_knowledge_delta(delta)

    def _write_knowledge_delta(self, delta: dict) -> None:
        """Append delta content to the knowledge markdown file."""
        if not self._knowledge_path:
            return

        delta_type = delta.get("type", "")

        if delta_type == "text":
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
        if self._fh is not None:
            self._fh.flush()
            self._fh.close()
            self._fh = None
        if self._kfh is not None:
            self._kfh.write("\n")
            self._kfh.flush()
            self._kfh.close()
            self._kfh = None
