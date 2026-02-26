# rye:signed:2026-02-26T05:02:40Z:ceb8605087685fca828bd0ea3b69303617797edffe2f13ed641d6c67ba15252d:X3LrtqXHFCU-7Lh3HBwumZQz3yOk1Df_aKOGqn0-sZGxGUD_vAYBr14eIPk3zOGsxyhRRnWgiqkq8JjOIMwECA==:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/events"
__tool_description__ = "Event emitter for thread lifecycle events"

import asyncio
from pathlib import Path
from typing import Any, Dict, Optional

from module_loader import load_module

_THREADS_ROOT = Path(__file__).parent.parent

events_loader = load_module("loaders/events_loader", anchor=_THREADS_ROOT)


class EventEmitter:
    """Emit events to transcript with criticality routing from config."""

    def __init__(self, project_path: Path):
        self.project_path = project_path
        self._loader = events_loader.get_events_loader()

    def emit(
        self,
        thread_id: str,
        event_type: str,
        payload: Dict,
        transcript: Any = None,
        criticality: Optional[str] = None,
    ) -> None:
        if criticality is None:
            event_config = self._loader.get_event_config(self.project_path, event_type)
            criticality = (
                event_config.get("criticality", "important")
                if event_config
                else "important"
            )

        if criticality == "critical":
            self.emit_critical(thread_id, event_type, payload, transcript)
        else:
            self.emit_droppable(thread_id, event_type, payload, transcript)

    def emit_critical(
        self, thread_id: str, event_type: str, payload: Dict, transcript: Any
    ) -> None:
        if transcript:
            transcript.write_event(thread_id, event_type, payload)

    def emit_droppable(
        self, thread_id: str, event_type: str, payload: Dict, transcript: Any
    ) -> None:
        if transcript:
            try:
                loop = asyncio.get_event_loop()
                loop.create_task(
                    self._async_emit(transcript, thread_id, event_type, payload)
                )
            except RuntimeError:
                transcript.write_event(thread_id, event_type, payload)

    async def _async_emit(
        self, transcript: Any, thread_id: str, event_type: str, payload: Dict
    ) -> None:
        try:
            transcript.write_event(thread_id, event_type, payload)
        except Exception:
            pass

    def get_criticality(self, event_type: str) -> str:
        return self._loader.get_criticality(self.project_path, event_type)
