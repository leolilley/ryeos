# rye:signed:2026-04-20T05:37:45Z:e9c3b838013d44cb754bd4f0fae2a8cdd77427d93f154209c28c64b19c60c8fe:wouYoewb3vjOxhtAjVBdCsts4zS8xRQ3vHGWydBYqOZbQQN3Ey5pPWEWKV-xsymOgo_H8iKVflw11ncQM4V3Aw:4b987fd4e40303ac
"""Parse a graph run's events and return a clean step summary."""

import argparse
import json
import sys
from pathlib import Path

__version__ = "1.1.0"
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


def _parse_events(events: list) -> dict:
    """Extract step_started/step_completed from event list."""
    started = {}
    steps = []

    for event in events:
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
        "steps": steps,
        "total_steps": len(steps),
        "errors": error_count,
        "cache_hits": cache_hits,
    }


def _fallback_transcript(run_id: str, project_path: str) -> dict | None:
    """Fall back to local transcript.jsonl if daemon is unavailable."""
    from rye.constants import AI_DIR

    proj = Path(project_path)
    transcript_path = proj / AI_DIR / "agent" / "graphs" / run_id / "transcript.jsonl"

    if not transcript_path.exists():
        return None

    events = []
    for line in transcript_path.read_text().splitlines():
        line = line.strip()
        if not line:
            continue
        try:
            events.append(json.loads(line))
        except json.JSONDecodeError:
            continue

    return events


def execute(params: dict, project_path: str) -> dict:
    from rye.runtime.daemon_rpc import (
        ThreadLifecycleClient,
        resolve_daemon_socket_path,
        RpcError,
    )

    run_id = params["run_id"]

    socket_path = resolve_daemon_socket_path()
    events = None

    if socket_path:
        try:
            client = ThreadLifecycleClient(socket_path)
            resp = client.replay_events(thread_id=run_id)
            events = resp.get("events", []) if resp else None
        except RpcError:
            events = None

    if events is None:
        events = _fallback_transcript(run_id, project_path)
        if events is None:
            return {"success": False, "error": f"No events found for run: {run_id}"}

    result = _parse_events(events)
    result["success"] = True
    result["run_id"] = run_id
    return result


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
