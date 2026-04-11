# rye:signed:2026-04-11T02:23:29Z:246bc40ae041813b8695e19303b234639ae6343da34e1f28cca76d787b510b90:zO8oWh6qCZp-lngGaZC8SRAIFVub3rDIjS6TqzLVIqxr3AKnTB2gUvqhjB4csF3xxlAnJFqTrHM0X-GUC9uvDg:4b987fd4e40303ac
"""Cancel a running process by run_id via SIGTERM."""

import argparse
import asyncio
import json
import sys

__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/processes"
__tool_description__ = "Cancel a running process by run_id"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "run_id": {
            "type": "string",
            "description": "Graph or thread run ID to cancel",
        },
        "grace": {
            "type": "number",
            "description": "Grace period in seconds before SIGKILL (default: 5)",
            "default": 5,
        },
    },
    "required": ["run_id"],
}


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
        result = client.send_command(run_id, "cancel")
        return {"success": True, "run_id": run_id, "command": result}
    except RpcError as e:
        return {"success": False, "error": str(e)}


def execute(params: dict, project_path: str) -> dict:
    return asyncio.run(_execute_async(params, project_path))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
