# rye:signed:2026-02-26T03:49:32Z:d7c948d358d6dd22cff7b5e062143546b4da6ed40f59e03f93c7f5bbd2790bb3:TytQQN9LR_atRlY1NcOYTe8H1arPuFnCnbcMAhRtiXNZVGrsTTMF-ey7Jk3PQu4hLgFVMJoV6tUqin_nRhqCDQ==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/events"
__tool_description__ = "Streaming tool call parser"

import json
from typing import Any, Dict, Generator, List, Optional


class StreamingToolParser:
    """Parse streaming chunks from LLM responses."""

    def __init__(self):
        self._buffer = ""
        self._tool_calls: List[Dict] = []
        self._current_call: Optional[Dict] = None

    def feed(self, chunk: str) -> Generator[Dict, None, None]:
        """Feed a chunk and yield parsed events."""
        self._buffer += chunk

        while self._buffer:
            event = self._try_parse_event()
            if event:
                yield event
            else:
                break

    def _try_parse_event(self) -> Optional[Dict]:
        """Try to parse a complete event from buffer."""
        try:
            if "\n\n" not in self._buffer:
                return None

            event_str, remainder = self._buffer.split("\n\n", 1)
            self._buffer = remainder

            if not event_str.strip():
                return None

            event = {"type": "unknown", "data": event_str}

            if event_str.startswith("data: "):
                data_str = event_str[6:]
                try:
                    data = json.loads(data_str)
                    event = self._parse_openai_style(data)
                except json.JSONDecodeError:
                    event["type"] = "raw"
                    event["data"] = data_str
            elif event_str.startswith("event: "):
                lines = event_str.split("\n")
                event_type = lines[0][7:]
                event = {"type": event_type, "data": {}}
                for line in lines[1:]:
                    if line.startswith("data: "):
                        try:
                            event["data"] = json.loads(line[6:])
                        except json.JSONDecodeError:
                            event["data"] = line[6:]

            return event

        except Exception:
            return None

    def _parse_openai_style(self, data: Dict) -> Dict:
        """Parse OpenAI-style streaming response."""
        event = {"type": "unknown", "data": data}

        if "choices" in data:
            choices = data.get("choices", [])
            if choices:
                delta = choices[0].get("delta", {})
                if "content" in delta:
                    event["type"] = "content_delta"
                    event["text"] = delta["content"]
                elif "tool_calls" in delta:
                    event["type"] = "tool_call_delta"
                    event["tool_calls"] = delta["tool_calls"]

        if data.get("finish_reason"):
            event["type"] = "finish"
            event["reason"] = data["finish_reason"]

        return event

    def get_tool_calls(self) -> List[Dict]:
        """Get accumulated tool calls."""
        return self._tool_calls

    def reset(self) -> None:
        """Reset parser state."""
        self._buffer = ""
        self._tool_calls = []
        self._current_call = None
