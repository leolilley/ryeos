# rye:signed:2026-03-30T04:30:49Z:5910c0a9b4fdf5200375b7331f5ccc1d0fb7f28f210741b7b2418ded2e606f19:HxIFbITJ2_cdW5V1wdDZcGJROSrLDlS0wGrlEDhbryNx42D3lCsy6-tNhRZLnWi6-8YwxKGfVT78WPGo4UjKAg:4b987fd4e40303ac
"""Parse a graph run's transcript.jsonl and return a clean step summary."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "rye/core/processes"
__tool_description__ = "Parse graph run transcript into step summary"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "run_id": {
            "type": "string",
            "description": "Graph run ID to parse transcript for",
        },
    },
    "required": ["run_id"],
}


def execute(params: dict, project_path: str) -> dict:
    from rye.constants import AI_DIR

    run_id = params["run_id"]
    proj = Path(project_path)
    transcript_path = proj / AI_DIR / "agent" / "graphs" / run_id / "transcript.jsonl"

    if not transcript_path.exists():
        return {"success": False, "error": f"Transcript not found: {transcript_path}"}

    started = {}
    steps = []

    for line in transcript_path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            event = json.loads(line)
        except json.JSONDecodeError:
            continue

        et = event.get("event_type", "")
        p = event.get("payload", {})

        if et == "step_started":
            step_num = p.get("step", 0)
            started[step_num] = {
                "node": p.get("node", ""),
                "action_id": p.get("action_id", ""),
            }

        elif et == "step_completed":
            step_num = p.get("step", 0)
            start_info = started.pop(step_num, {})
            steps.append({
                "step": step_num,
                "node": p.get("node", start_info.get("node", "")),
                "action_id": p.get("action_id", start_info.get("action_id", "")),
                "status": p.get("status", "ok"),
                "elapsed_s": round(p.get("elapsed_s", 0), 4),
                "cache_hit": p.get("cache_hit", False),
                "error": str(p.get("error", "")),
                "thread_id": p.get("thread_id", ""),
            })

    error_count = sum(1 for s in steps if s["status"] == "error")
    cache_hits = sum(1 for s in steps if s["cache_hit"])

    return {
        "success": True,
        "run_id": run_id,
        "steps": steps,
        "total_steps": len(steps),
        "errors": error_count,
        "cache_hits": cache_hits,
    }


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
