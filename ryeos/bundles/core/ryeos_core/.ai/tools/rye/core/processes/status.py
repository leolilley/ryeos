# rye:signed:2026-04-19T09:49:53Z:f0f07111c42e853ac54d70fddd04d32a57a91ce1337e5c1add98d63745513cd6:3QDv551zSHH3lD8IIwuJjvoEGxiw+R7Z48akUeNmT7jlR1YRLqeZAoAfvC7QYoBYtUYtRkZtiBkADxyVNbBBCw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""Check status of a running process by run_id."""

import argparse
import asyncio
import json
import sys

__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/processes"
__tool_description__ = "Check process status by run_id"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "run_id": {
            "type": "string",
            "description": "Graph or thread run ID to check",
        },
    },
    "required": ["run_id"],
}


async def _check_pid(pid: int) -> dict:
    """Check if PID is alive via ExecutePrimitive."""
    from rye.primitives.execute import ExecutePrimitive

    sp = ExecutePrimitive()
    result = await sp.status(pid)
    return {"alive": result.alive, "pid": result.pid}


async def _execute_async(params: dict, project_path: str) -> dict:
    from rye.runtime.daemon_rpc import (
        ThreadLifecycleClient,
        resolve_daemon_socket_path,
        RpcError,
    )

    run_id = params["run_id"]

    socket_path = resolve_daemon_socket_path()
    if not socket_path:
        return {"success": False, "error": "Daemon not available (no socket path)"}

    try:
        client = ThreadLifecycleClient(socket_path)
        resp = client.get_thread(run_id)
    except RpcError as e:
        return {"success": False, "error": f"Daemon RPC error: {e}"}

    thread = resp.get("thread") if resp else None
    if not thread:
        return {"success": False, "error": f"Run not found: {run_id}"}

    pid = thread.get("pid")
    status = thread.get("status", "unknown")

    result = {
        "success": True,
        "run_id": run_id,
        "status": status,
        "pid": pid,
        "directive": thread.get("item_ref"),
        "created_at": thread.get("created_at"),
        "updated_at": thread.get("updated_at"),
    }

    if pid and status in ("running", "created"):
        pid_status = await _check_pid(pid)
        result["alive"] = pid_status["alive"]
    else:
        result["alive"] = False

    if status == "completed_with_errors":
        stored_result = resp.get("result")
        if stored_result:
            try:
                parsed = json.loads(stored_result) if isinstance(stored_result, str) else stored_result
                if isinstance(parsed, dict) and "errors_suppressed" in parsed:
                    result["errors_suppressed"] = parsed["errors_suppressed"]
            except (json.JSONDecodeError, ValueError):
                pass

    return result


def execute(params: dict, project_path: str) -> dict:
    return asyncio.run(_execute_async(params, project_path))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
