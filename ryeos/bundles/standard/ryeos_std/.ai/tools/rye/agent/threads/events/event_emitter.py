# rye:signed:2026-04-11T01:34:05Z:876963b578c74a660cbfa97f4b2af9bd2db6e4684c1aac740fbd37c904701ebb:D3EJw9oYa-3MkY7cC1M9o2lMRntkwdKlbTvXWA75oT5pp5A8A-dLkgxB6-nmrXItdP3jUAaXQ8VIIoCmwTdQBg:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/events"
__tool_description__ = "Event emitter for daemon-owned thread lifecycle events"

from pathlib import Path
from typing import Any, Dict, Optional

from module_loader import load_module
from rye.runtime.daemon_rpc import require_daemon_runtime_context

_THREADS_ROOT = Path(__file__).parent.parent

events_loader = load_module("loaders/events_loader", anchor=_THREADS_ROOT)


class EventEmitter:
    """Emit daemon-owned thread events with config-driven storage routing."""

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
        del transcript, criticality
        self._append(thread_id, event_type, payload)

    def _storage_class(self, event_type: str) -> str:
        event_config = self._loader.get_event_config(self.project_path, event_type) or {}
        storage_class = event_config.get("storage_class")
        if storage_class in {"indexed", "journal_only"}:
            return storage_class
        return "indexed"

    def _append(self, thread_id: str, event_type: str, payload: Dict) -> None:
        client, resolved_thread_id, _ = require_daemon_runtime_context(thread_id=thread_id)
        client.append_event(
            resolved_thread_id,
            event_type,
            self._storage_class(event_type),
            payload,
        )

    def get_criticality(self, event_type: str) -> str:
        return self._loader.get_criticality(self.project_path, event_type)
