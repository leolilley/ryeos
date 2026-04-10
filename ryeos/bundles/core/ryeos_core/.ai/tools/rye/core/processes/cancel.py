# rye:signed:2026-04-10T00:57:18Z:0e8c720461d8f3dc334d308cbd4ea266fbb470370a3aa565231c05b9d3433c8b:tOowIwsVQBrFDInCieHBfpjSL4fkXTCHAhqSqxI3B3DRWwIckFK0mtxD432vcQ_N0K30Wg6NPnTtk9usPCIQCg:4b987fd4e40303ac
"""Cancel a running process by run_id via SIGTERM."""

import argparse
import asyncio
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
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


def _get_registry(project_path: Path):
    """Get thread registry instance."""
    from rye.constants import AI_DIR

    db_path = project_path / AI_DIR / "agent" / "threads" / "registry.db"
    if not db_path.exists():
        return None

    import sqlite3
    from datetime import datetime, timezone

    class _Registry:
        def __init__(self, db):
            self.db_path = db

        def get_thread(self, thread_id):
            with sqlite3.connect(self.db_path) as conn:
                conn.row_factory = sqlite3.Row
                cursor = conn.execute(
                    "SELECT * FROM threads WHERE thread_id = ?", (thread_id,)
                )
                row = cursor.fetchone()
                return dict(row) if row else None

        def update_status(self, thread_id, status):
            now = datetime.now(timezone.utc).isoformat()
            with sqlite3.connect(self.db_path) as conn:
                conn.execute(
                    "UPDATE threads SET status = ?, updated_at = ? WHERE thread_id = ?",
                    (status, now, thread_id),
                )
                conn.commit()

    return _Registry(db_path)


async def _execute_async(params: dict, project_path: str) -> dict:
    run_id = params["run_id"]
    grace = params.get("grace", 5)
    proj = Path(project_path)

    registry = _get_registry(proj)
    if registry is None:
        return {"success": False, "error": "Thread registry not found"}

    thread = registry.get_thread(run_id)
    if not thread:
        return {"success": False, "error": f"Run not found: {run_id}"}

    pid = thread.get("pid")
    if not pid:
        return {"success": False, "error": f"No PID recorded for run: {run_id}"}

    status = thread.get("status", "unknown")
    if status in ("completed", "error", "cancelled", "killed"):
        return {
            "success": False,
            "error": f"Run already in terminal state: {status}",
            "run_id": run_id,
            "status": status,
        }

    from rye.primitives.execute import ExecutePrimitive

    sp = ExecutePrimitive()
    kill_result = await sp.kill(pid, grace=grace)

    if kill_result.success:
        registry.update_status(run_id, "cancelled")
        return {
            "success": True,
            "run_id": run_id,
            "pid": pid,
            "method": kill_result.method,
        }

    return {
        "success": False,
        "error": f"Failed to cancel PID {pid}: {kill_result.error}",
        "run_id": run_id,
        "pid": pid,
    }


def execute(params: dict, project_path: str) -> dict:
    return asyncio.run(_execute_async(params, project_path))


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
