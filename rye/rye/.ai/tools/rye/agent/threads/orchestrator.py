# rye:signed:2026-02-16T05:31:57Z:775cbc66b3e1def2e8e22f0ca423b9dd7b47364f04528042b05b8390eb9da381:YqgGNuzDn1QLBNNJMKWYn0jNy1d97dQV6P3bobRpd9cWdhUyBEy0AAZksGDB3BBY22OPgfEpDDaACT2cySorDg==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread coordination: wait, cancel, status"

from typing import Any, Dict, List, Optional

import asyncio
from pathlib import Path

from module_loader import load_module

_ANCHOR = Path(__file__).parent


CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": ["wait_threads", "cancel_thread", "get_status", "list_active"],
        },
        "thread_ids": {"type": "array", "items": {"type": "string"}},
        "thread_id": {"type": "string"},
        "timeout": {"type": "number"},
    },
    "required": ["operation"],
}


_thread_events: Dict[str, asyncio.Event] = {}
_thread_results: Dict[str, Dict] = {}
_active_harnesses: Dict[str, Any] = {}
_spawn_counts: Dict[str, int] = {}


def register_thread(thread_id: str, harness: Any) -> None:
    """Called by runner.py at thread start. Creates Event for wait coordination."""
    _thread_events[thread_id] = asyncio.Event()
    _active_harnesses[thread_id] = harness


def check_spawn_limit(parent_thread_id: str, limit: int) -> Optional[Dict]:
    """Check if parent has exceeded spawn limit. Returns error dict or None."""
    count = _spawn_counts.get(parent_thread_id, 0)
    if count >= limit:
        return {
            "limit_code": "spawns_exceeded",
            "current_value": count,
            "current_max": limit,
        }
    return None


def increment_spawn_count(parent_thread_id: str) -> int:
    """Increment spawn count for parent. Returns new count."""
    _spawn_counts[parent_thread_id] = _spawn_counts.get(parent_thread_id, 0) + 1
    return _spawn_counts[parent_thread_id]


def complete_thread(thread_id: str, result: Dict) -> None:
    """Called by runner.py in finally block. Signals Event so wait_threads unblocks."""
    _thread_results[thread_id] = result
    event = _thread_events.get(thread_id)
    if event:
        event.set()
    _active_harnesses.pop(thread_id, None)
    _spawn_counts.pop(thread_id, None)


async def execute(params: Dict, project_path: str) -> Dict:
    operation = params["operation"]

    if operation == "wait_threads":
        thread_ids = params.get("thread_ids", [])
        timeout = params.get("timeout")

        if timeout is None:
            resilience_loader = load_module("loaders/resilience_loader", anchor=_ANCHOR)
            config = resilience_loader.load(Path(project_path))
            timeout = config.get("coordination", {}).get("wait_timeout_seconds", 300.0)

        results: Dict[str, Dict] = {}
        try:
            for tid in thread_ids:
                event = _thread_events.get(tid)
                if event:
                    await asyncio.wait_for(event.wait(), timeout=timeout)
                    results[tid] = _thread_results.get(tid, {"status": "unknown"})
                else:
                    results[tid] = {"status": "not_found"}
        except asyncio.TimeoutError:
            for tid in thread_ids:
                if tid not in results:
                    results[tid] = {"status": "timeout"}

        all_success = all(r.get("success", False) for r in results.values())
        return {"success": all_success, "results": results}

    if operation == "cancel_thread":
        thread_id = params.get("thread_id")
        harness = _active_harnesses.get(thread_id)
        if harness:
            harness.request_cancel()
            return {"success": True, "cancelled": thread_id}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "get_status":
        thread_id = params.get("thread_id")
        if thread_id in _thread_results:
            return {"success": True, **_thread_results[thread_id]}
        if thread_id in _thread_events:
            return {"success": True, "status": "running"}
        return {"success": False, "error": f"Thread not found: {thread_id}"}

    if operation == "list_active":
        active = [tid for tid, event in _thread_events.items() if not event.is_set()]
        return {"success": True, "active_threads": active, "count": len(active)}

    return {"success": False, "error": f"Unknown operation: {operation}"}
