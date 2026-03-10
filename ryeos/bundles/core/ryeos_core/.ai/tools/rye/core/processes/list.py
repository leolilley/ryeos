# rye:signed:2026-03-10T04:35:37Z:46a404bfbe6e56be0364708fc794674128d067747974986532280b1963e0ac6f:xoWvtLIXIRMA0HzUCOiJEbA4yhYWAULfQ_v2i4MQidHejvJPWZpAOZrYZesuvcuQAe47hVXsmHEV9nZv4fVyDg==:4b987fd4e40303ac
"""List running processes from thread registry."""

import argparse
import json
import sqlite3
import sys
from pathlib import Path

__version__ = "1.0.0"
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
            "enum": ["running", "completed", "cancelled", "error", "killed"],
        },
    },
}


def execute(params: dict, project_path: str) -> dict:
    from rye.constants import AI_DIR

    proj = Path(project_path)
    db_path = proj / AI_DIR / "agent" / "threads" / "registry.db"
    if not db_path.exists():
        return {"success": True, "runs": [], "count": 0}

    status_filter = params.get("status")

    with sqlite3.connect(db_path) as conn:
        conn.row_factory = sqlite3.Row
        if status_filter:
            cursor = conn.execute(
                "SELECT * FROM threads WHERE status = ? ORDER BY created_at DESC",
                (status_filter,),
            )
        else:
            cursor = conn.execute("""
                SELECT * FROM threads
                WHERE status NOT IN ('completed', 'error', 'cancelled', 'killed')
                ORDER BY created_at DESC
            """)
        rows = cursor.fetchall()

    runs = []
    for row in rows:
        r = dict(row)
        runs.append({
            "run_id": r.get("thread_id"),
            "directive": r.get("directive"),
            "status": r.get("status"),
            "pid": r.get("pid"),
            "parent_id": r.get("parent_id"),
            "created_at": r.get("created_at"),
            "updated_at": r.get("updated_at"),
        })

    return {"success": True, "runs": runs, "count": len(runs)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
