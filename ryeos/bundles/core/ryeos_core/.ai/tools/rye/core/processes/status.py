# rye:signed:2026-03-10T04:35:37Z:2e4ecc6beb380b0ea1704a6c7fb9853f1334a1c6c75ea05c41d0723a34e6bce3:krpKrZ1uL-GMb80CwzxBq1mLHsSbtyfScsNU7nA4xUcBoczrVnZMccOwK4G4FNRMWQWApWodaw9xQyq4IDGECA==:4b987fd4e40303ac
"""Check status of a running process by run_id."""

import argparse
import asyncio
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
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


def _get_registry(project_path: Path):
    """Get thread registry instance."""
    from rye.constants import AI_DIR

    db_path = project_path / AI_DIR / "agent" / "threads" / "registry.db"
    if not db_path.exists():
        return None

    import sqlite3

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

    return _Registry(db_path)


async def _check_pid(pid: int) -> dict:
    """Check if PID is alive via SubprocessPrimitive."""
    from lillux.primitives.subprocess import SubprocessPrimitive

    sp = SubprocessPrimitive()
    result = await sp.status(pid)
    return {"alive": result.alive, "pid": result.pid}


async def _execute_async(params: dict, project_path: str) -> dict:
    run_id = params["run_id"]
    proj = Path(project_path)

    registry = _get_registry(proj)
    if registry is None:
        return {"success": False, "error": "Thread registry not found"}

    thread = registry.get_thread(run_id)
    if not thread:
        return {"success": False, "error": f"Run not found: {run_id}"}

    pid = thread.get("pid")
    status = thread.get("status", "unknown")

    result = {
        "success": True,
        "run_id": run_id,
        "status": status,
        "pid": pid,
        "directive": thread.get("directive"),
        "created_at": thread.get("created_at"),
        "updated_at": thread.get("updated_at"),
    }

    if pid and status in ("running", "created"):
        pid_status = await _check_pid(pid)
        result["alive"] = pid_status["alive"]
    else:
        result["alive"] = False

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
