# rye:signed:2026-04-20T05:46:18Z:ee7100cd46014081ac90feca9b085c422a5f044350686def889eb0d988218050:ePSBnANQ4bJNLaIdS8Xv6hl8x6kJSEiR-d5dQJ519SQuJDwMkZ9x4ru-elTDNB2a_hHcYpMpYRzOqsB2ly70Bg:4b987fd4e40303ac
__version__ = "1.6.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads"
__tool_description__ = "Thread coordination: wait, cancel, status, chain resolution"

from typing import Any, Dict, Optional

import asyncio
from pathlib import Path

from rye.runtime.daemon_rpc import RpcError, ThreadLifecycleClient, resolve_daemon_socket_path
from module_loader import load_module

_ANCHOR = Path(__file__).parent


CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": [
                "wait_threads", "cancel_thread", "kill_thread",
                "get_status", "list_active", "aggregate_results",
                "get_chain", "chain_search",
                "read_transcript", "resume_thread", "handoff_thread",
            ],
        },
        "thread_ids": {"type": "array", "items": {"type": "string"}},
        "thread_id": {"type": "string"},
        "timeout": {"type": "number"},
        "query": {"type": "string", "description": "Search query for chain_search"},
        "search_type": {"type": "string", "enum": ["regex", "text"], "default": "text"},
        "max_results": {"type": "integer", "default": 50},
        "tail_lines": {"type": "integer", "description": "Number of lines from the end of the thread history export to return"},
        "message": {"type": "string", "description": "User message to append for resume_thread"},
    },
    "required": ["operation"],
}


# In-process tracking (only valid within the same Python process)
_thread_events: Dict[str, asyncio.Event] = {}
_thread_results: Dict[str, Dict] = {}
_active_harnesses: Dict[str, Any] = {}
_spawn_counts: Dict[str, int] = {}
_thread_depths: Dict[str, int] = {}

_ACTIVE_STATUSES = {"created", "running"}
_TERMINAL_STATUSES = {"completed", "failed", "cancelled", "killed", "timed_out", "continued"}


def register_thread(thread_id: str, harness: Any, depth: int = 0) -> None:
    """Called by runner.py at thread start. Creates Event for wait coordination."""
    _thread_events[thread_id] = asyncio.Event()
    _active_harnesses[thread_id] = harness
    _thread_depths[thread_id] = depth


def get_depth(thread_id: str) -> int:
    """Get current depth of a thread. 0 = root."""
    return _thread_depths.get(thread_id, 0)


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


def _normalize_status(status: Optional[str]) -> str:
    if status == "error":
        return "failed"
    return status or "unknown"


def _daemon_client() -> ThreadLifecycleClient:
    socket_path = resolve_daemon_socket_path()
    if not socket_path:
        raise RuntimeError("ryeosd socket path is unavailable")
    return ThreadLifecycleClient(socket_path)


def _local_result(thread_id: str) -> Dict:
    result = dict(_thread_results.get(thread_id, {}))
    result["status"] = _normalize_status(result.get("status"))
    result.setdefault("thread_id", thread_id)
    return result


def _daemon_result(record: Dict) -> Dict:
    thread = record.get("thread") or {}
    result = record.get("result") or {}
    return {
        "thread_id": thread.get("thread_id"),
        "status": _normalize_status(thread.get("status")),
        "outcome_code": result.get("outcome_code"),
        "result": result.get("result"),
        "error": result.get("error"),
        "artifacts": record.get("artifacts") or [],
    }


async def _wait_single(thread_id: str, timeout: float, project_path: Path) -> Dict:
    """Wait for a single thread using daemon-owned state once it leaves-process."""
    del project_path

    if thread_id in _thread_results:
        return _local_result(thread_id)

    event = _thread_events.get(thread_id)
    if event:
        try:
            await asyncio.wait_for(event.wait(), timeout=timeout)
            return _local_result(thread_id)
        except asyncio.TimeoutError:
            return {"status": "timeout", "thread_id": thread_id}

    return await _poll_daemon_thread(thread_id, timeout)


async def _poll_daemon_thread(thread_id: str, timeout: float) -> Dict:
    """Wait for thread completion via daemon polling."""
    import time

    client = _daemon_client()
    deadline = time.monotonic() + timeout
    poll_interval = 0.5

    while True:
        record = client.get_thread(thread_id)
        if not record:
            return {"status": "not_found", "thread_id": thread_id}

        result = _daemon_result(record)
        if result["status"] in _TERMINAL_STATUSES:
            return result

        remaining = deadline - time.monotonic()
        if remaining <= 0:
            return {"status": "timeout", "thread_id": thread_id}
        await asyncio.sleep(min(poll_interval, remaining))


def _unsupported(message: str) -> Dict:
    return {"success": False, "error": message}


async def execute(params: Dict, project_path: str) -> Dict:
    operation = params["operation"]
    proj_path = Path(project_path)

    if operation == "wait_threads":
        thread_ids = params.get("thread_ids", [])
        timeout = params.get("timeout")

        if timeout is None:
            try:
                coordination_loader = load_module("loaders/coordination_loader", anchor=_ANCHOR)
                config = coordination_loader.load(proj_path)
                timeout = config.get("coordination", {}).get("wait_threads", {}).get("default_timeout", 600.0)
            except Exception:
                timeout = 600.0

        # Wait for all threads concurrently
        wait_tasks = [
            _wait_single(tid, timeout, proj_path)
            for tid in thread_ids
        ]
        results_list = await asyncio.gather(*wait_tasks, return_exceptions=True)

        results = {}
        for i, (tid, result) in enumerate(zip(thread_ids, results_list)):
            # tid may be a dict (failed spawn error) instead of a string
            key = tid if isinstance(tid, str) else f"__failed_{i}"
            if isinstance(result, Exception):
                results[key] = {"status": "error", "error": str(result)}
            elif not isinstance(tid, str):
                # Spawn itself failed — tid is the error dict
                results[key] = tid if isinstance(tid, dict) else {"status": "error", "error": str(tid)}
            else:
                results[key] = result

        all_success = all(
            r.get("status") == "completed"
            for r in results.values()
        )
        return {"success": all_success, "results": results}

    if operation == "cancel_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        try:
            result = _daemon_client().send_command(thread_id, "cancel")
            return {"success": True, "thread_id": thread_id, "command": result}
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}

    if operation == "kill_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        try:
            result = _daemon_client().send_command(thread_id, "kill")
            return {"success": True, "thread_id": thread_id, "command": result}
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}

    if operation == "get_status":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        # In-process check first
        if thread_id in _thread_results:
            return {"success": True, **_local_result(thread_id)}
        if thread_id in _thread_events:
            return {"success": True, "status": "running", "thread_id": thread_id}
        try:
            record = _daemon_client().get_thread(thread_id)
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}
        if not record:
            return {"success": False, "error": f"Thread not found: {thread_id}"}
        return {"success": True, **_daemon_result(record)}

    if operation == "list_active":
        try:
            threads = (_daemon_client().list_threads(limit=200) or {}).get("threads") or []
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}
        active_threads = [
            thread["thread_id"]
            for thread in threads
            if _normalize_status(thread.get("status")) in _ACTIVE_STATUSES
        ]
        return {"success": True, "active_threads": active_threads, "count": len(active_threads)}

    if operation == "aggregate_results":
        thread_ids = params.get("thread_ids", [])
        results = {}
        try:
            client = _daemon_client()
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}
        for tid in thread_ids:
            if tid in _thread_results:
                results[tid] = _local_result(tid)
            else:
                record = client.get_thread(tid)
                results[tid] = _daemon_result(record) if record else {"status": "not_found", "thread_id": tid}
        return {"success": True, "results": results}

    if operation == "get_chain":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        try:
            chain = _daemon_client().get_chain(thread_id)
        except (OSError, RuntimeError, RpcError) as exc:
            return {"success": False, "error": str(exc)}
        if not chain:
            return {"success": False, "error": f"Thread not found: {thread_id}"}
        threads = chain.get("threads") or []
        return {
            "success": True,
            "chain_length": len(threads),
            "threads": threads,
            "edges": chain.get("edges") or [],
            "chain": [
                {
                    "thread_id": thread.get("thread_id"),
                    "status": thread.get("status"),
                    "kind": thread.get("kind"),
                    "item_ref": thread.get("item_ref"),
                }
                for thread in threads
            ],
        }

    if operation == "chain_search":
        thread_id = params.get("thread_id")
        query = params.get("query")
        if not thread_id or not query:
            return {"success": False, "error": "thread_id and query required"}
        chain_search = load_module("internal/thread_chain_search", anchor=_ANCHOR)
        return chain_search.execute({
            "thread_id": thread_id,
            "query": query,
            "search_type": params.get("search_type", "text"),
            "max_results": params.get("max_results", 50),
        }, project_path)

    if operation == "read_transcript":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        from rye.constants import AI_DIR, KNOWLEDGE_THREADS_REL
        from pathlib import PurePosixPath
        thread_path = PurePosixPath(thread_id)
        history_path = proj_path / AI_DIR / KNOWLEDGE_THREADS_REL / thread_path.parent / f"{thread_path.name}.md"
        if not history_path.exists():
            return {"success": False, "error": f"Thread history export not found for thread: {thread_id}"}
        content = history_path.read_text(encoding="utf-8")
        tail_lines = params.get("tail_lines")
        if tail_lines and tail_lines > 0:
            lines = content.splitlines()
            content = "\n".join(lines[-tail_lines:])
        return {"success": True, "thread_id": thread_id, "content": content}

    if operation == "resume_thread":
        thread_id = params.get("thread_id")
        message = params.get("message")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        if not message:
            return {"success": False, "error": "message required"}
        return _unsupported(
            "resume_thread is not available until daemon-owned continuation replaces transcript-driven resume",
        )

    if operation == "handoff_thread":
        thread_id = params.get("thread_id")
        if not thread_id:
            return {"success": False, "error": "thread_id required"}
        return _unsupported(
            "handoff_thread is not available until daemon-owned continuation creates successor threads and continued edges",
        )

    return {"success": False, "error": f"Unknown operation: {operation}"}
