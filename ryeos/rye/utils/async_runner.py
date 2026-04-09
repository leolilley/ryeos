"""Generic async runner — child process entrypoint for detached execution.

Invoked by launch_detached() when async=True on a tool or remote directive.
Reads an execute payload from stdin, runs ExecuteTool.handle() synchronously,
and updates the ThreadRegistry on completion.

Uses the established thread system: thread_id registered in ThreadRegistry
    (SQLite at .ai/state/threads/registry.db), log dir at
    .ai/state/threads/{thread_id}/, results stored via registry.set_result().

Usage (internal — spawned by launch_detached, not called directly):
    python -m rye.utils.async_runner --project-path /path --thread-id <uuid>

Stdin: JSON payload with item_id, parameters, thread, etc.
"""

import asyncio
import json
import logging
import os
import sys
from pathlib import Path

logger = logging.getLogger(__name__)


async def _run(payload: dict, project_path: str) -> dict:
    """Execute the payload via ExecuteTool and return the result."""
    from rye.actions.execute import ExecuteTool

    tool = ExecuteTool(project_path=project_path)

    return await tool.handle(
        item_id=payload["item_id"],
        project_path=project_path,
        parameters=payload.get("parameters", {}),
        target=payload.get("target", "local"),
        thread=payload.get("thread", "inline"),
        # Never re-async — we ARE the async child
    )


def main():
    import argparse

    parser = argparse.ArgumentParser(description="Async execution runner")
    parser.add_argument("--project-path", required=True)
    parser.add_argument("--thread-id", required=True)
    args = parser.parse_args()

    if os.environ.get("RYE_DEBUG"):
        logging.basicConfig(
            level=logging.DEBUG,
            format="[async_runner] %(levelname)s: %(message)s",
            stream=sys.stderr,
        )

    # Read payload from stdin
    payload = json.loads(sys.stdin.read())

    project_path = args.project_path
    thread_id = args.thread_id
    proj = Path(project_path)

    # Get registry (optional — degrades gracefully)
    from rye.actions.execute import ExecuteTool

    registry = ExecuteTool._get_registry(proj)

    try:
        result = asyncio.run(_run(payload, project_path))

        status = "completed" if result.get("status") != "error" else "error"

        if registry:
            registry.update_status(thread_id, status)
            registry.set_result(thread_id, result)

        # Print result to stdout (captured in spawn.log by lillux)
        print(json.dumps(result, default=str))

    except Exception as exc:
        error_result = {"status": "error", "error": str(exc), "thread_id": thread_id}

        if registry:
            registry.update_status(thread_id, "error")
            registry.set_result(thread_id, error_result)

        print(json.dumps(error_result, default=str))
        sys.exit(1)


if __name__ == "__main__":
    main()
