# rye:signed:2026-04-11T00:45:35Z:a31f6753adfceeec7b83d1e21bea55c4686ef67b091cca56a80111f11d6a4c5f:vQNnoHoCOnPsCBRXvRM80bjZsjIEiFNmBrCfgk66KbZgwYWcRn1YwS_76os_CTDOFy-PjeQ7lScuzqPlItsXDw:4b987fd4e40303ac
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Append daemon-owned thread events"

from pathlib import Path
from typing import Dict

from rye.runtime.daemon_rpc import RpcError, ThreadLifecycleClient

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "socket_path": {"type": "string"},
        "event_type": {"type": "string"},
        "storage_class": {
            "type": "string",
            "enum": ["indexed", "journal_only"],
            "default": "indexed",
        },
        "payload": {"type": "object", "default": {}},
        "thread_id": {"type": "string"},
    },
    "required": ["socket_path", "event_type", "thread_id"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Append an event through the daemon RPC surface."""
    try:
        socket_path = params.get("socket_path")
        if not socket_path:
            raise RpcError("invalid_request", "socket_path is required")
        thread_id = params.get("thread_id")
        if not thread_id:
            raise RpcError("invalid_request", "thread_id is required")
        event_type = params.get("event_type")
        if not event_type:
            raise RpcError("invalid_request", "event_type is required")

        payload = params.get("payload") or {}
        if not isinstance(payload, dict):
            raise RpcError("invalid_request", "payload must be an object")

        storage_class = params.get("storage_class", "indexed")
        client = ThreadLifecycleClient(socket_path)
        persisted = client.append_event(
            thread_id,
            event_type,
            storage_class,
            payload,
        )
        return {
            "success": True,
            "event_type": event_type,
            "persisted": persisted,
        }
    except Exception as exc:
        return {"success": False, "error": str(exc)}
