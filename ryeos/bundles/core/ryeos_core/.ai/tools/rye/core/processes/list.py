# rye:signed:2026-04-20T05:37:45Z:80018174f409b48b4b9ef67009f9c195bcb55a9ce563948335194c443f391bc3:oeHd4OuObBKL4dJUs5Tf8eVx9uASXQxEtuT9rauH-p3A1oPIuiKYK6CemaRIpno3XzJ0oq8FQD_iBWUiIymiDQ:4b987fd4e40303ac
"""List running processes from daemon."""

import argparse
import json
import sys

__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/processes"
__tool_description__ = "List processes from thread registry"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "status": {
            "type": "string",
            "description": "Filter by status (running/completed/cancelled/error/killed). Omit for all active.",
            "enum": ["running", "completed", "completed_with_errors", "cancelled", "error", "killed"],
        },
    },
}

_TERMINAL_STATUSES = frozenset({"completed", "completed_with_errors", "error", "cancelled", "killed"})


def execute(params: dict, project_path: str) -> dict:
    from rye.runtime.daemon_rpc import (
        ThreadLifecycleClient,
        resolve_daemon_socket_path,
        RpcError,
    )

    socket_path = resolve_daemon_socket_path()
    if not socket_path:
        return {"success": True, "runs": [], "count": 0}

    try:
        client = ThreadLifecycleClient(socket_path)
        resp = client.list_threads(limit=200)
    except RpcError as e:
        return {"success": False, "error": f"Daemon RPC error: {e}"}

    all_threads = resp.get("threads", []) if resp else []

    status_filter = params.get("status")
    if status_filter:
        threads = [t for t in all_threads if t.get("status") == status_filter]
    else:
        threads = [t for t in all_threads if t.get("status") not in _TERMINAL_STATUSES]

    runs = []
    for t in threads:
        entry = {
            "run_id": t.get("thread_id"),
            "directive": t.get("item_ref"),
            "status": t.get("status"),
            "pid": t.get("pid"),
            "parent_id": t.get("parent_id"),
            "created_at": t.get("created_at"),
            "updated_at": t.get("updated_at"),
        }
        if t.get("status") == "completed_with_errors":
            stored_result = t.get("result")
            if stored_result:
                try:
                    parsed = json.loads(stored_result) if isinstance(stored_result, str) else stored_result
                    if isinstance(parsed, dict) and "errors_suppressed" in parsed:
                        entry["errors_suppressed"] = parsed["errors_suppressed"]
                except (json.JSONDecodeError, ValueError):
                    pass
        runs.append(entry)

    return {"success": True, "runs": runs, "count": len(runs)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
