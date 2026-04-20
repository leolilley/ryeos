# rye:signed:2026-04-20T05:46:18Z:8d8b156c33a07534ee77f09220272490d57a9a8c63f9ac18fed96041e2c5f8de:w0ByaxustM_3h1UvNlFBnIjDc4K_35C9aofwjCl0lywBqmMD9B8ywrpaWzahC42JFhCo5xVf6lXPcPFV-hpzBg:4b987fd4e40303ac
# internal/thread_chain_search.py
__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Search across all threads in a continuation chain"

import json
import re
from typing import Dict

from rye.runtime.daemon_rpc import RpcError, ThreadLifecycleClient, resolve_daemon_socket_path


def _daemon_client() -> ThreadLifecycleClient:
    socket_path = resolve_daemon_socket_path()
    if not socket_path:
        raise RuntimeError("ryeosd socket path is unavailable")
    return ThreadLifecycleClient(socket_path)


def _load_thread_events(client: ThreadLifecycleClient, thread_id: str) -> list[Dict]:
    loaded = []
    cursor = None

    while True:
        page = client.replay_events(thread_id=thread_id, after_chain_seq=cursor, limit=200)
        events = page.get("events") or []
        if not events:
            break
        loaded.extend(events)
        cursor = page.get("next_cursor")
        if cursor is None:
            break

    return loaded

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "thread_id": {"type": "string", "description": "Any thread in the chain"},
        "query": {"type": "string", "description": "Search pattern (regex or text)"},
        "search_type": {"type": "string", "enum": ["regex", "text"], "default": "text"},
        "include_events": {
            "type": "array",
            "items": {"type": "string"},
            "default": ["cognition_in", "cognition_out", "tool_call_start", "tool_call_result"],
            "description": "Event types to search",
        },
        "max_results": {"type": "integer", "default": 50},
    },
    "required": ["thread_id", "query"],
}


def execute(params: Dict, project_path: str) -> Dict:
    """Search across all threads in a continuation chain.

    Collects the daemon-owned chain from root to current, then searches
    each thread's indexed event history for the query.
    """
    del project_path

    thread_id = params["thread_id"]
    query = params["query"]
    search_type = params.get("search_type", "text")
    include_events = set(params.get("include_events", [
        "cognition_in", "cognition_out", "tool_call_start", "tool_call_result"
    ]))
    max_results = params.get("max_results", 50)

    try:
        client = _daemon_client()
        chain = client.get_chain(thread_id)
    except (OSError, RuntimeError, RpcError) as exc:
        return {"success": False, "error": str(exc)}

    if not chain:
        return {"success": False, "error": f"No chain found for thread {thread_id}"}

    threads = chain.get("threads") or []

    results = []
    pattern = re.compile(query, re.IGNORECASE) if search_type == "regex" else None

    for thread in threads:
        tid = thread["thread_id"]
        try:
            events = _load_thread_events(client, tid)
        except (OSError, RpcError):
            continue

        for line_no, event in enumerate(events, 1):
            event_type = event.get("event_type", "")
            if event_type not in include_events:
                continue

            payload_str = json.dumps(event.get("payload", {}))

            if search_type == "regex":
                matches = pattern.findall(payload_str)
            else:
                matches = [query] if query.lower() in payload_str.lower() else []

            if matches:
                results.append({
                    "thread_id": tid,
                    "event_type": event_type,
                    "line_no": line_no,
                    "snippet": payload_str[:500],
                    "matches": matches[:5],
                })

                if len(results) >= max_results:
                    return {
                        "success": True,
                        "chain_length": len(threads),
                        "results": results,
                        "truncated": True,
                    }

    return {
        "success": True,
        "chain_length": len(threads),
        "chain_threads": [t["thread_id"] for t in threads],
        "results": results,
        "truncated": False,
    }
