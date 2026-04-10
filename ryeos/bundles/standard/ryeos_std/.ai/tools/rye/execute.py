# rye:signed:2026-04-10T00:57:19Z:4a27a5dda5c19f40679a6e3008fcea44239aaca622006d96e19547e539676ade:zhJ47DwBsXrprAoi02jWBx1g1DybSt9r-U2FpJ1J9q7amsYdNo-5XXOd52gvKEp_PaqcC8lmILxkce2qsmr0CA:4b987fd4e40303ac
"""Execute a tool or directive via rye."""

import argparse
import json
import asyncio

from rye.primary_action_descriptions import (
    EXECUTE_ASYNC_DESC,
    EXECUTE_DRY_RUN_DESC,
    EXECUTE_PARAMETERS_DESC,
    EXECUTE_TARGET_DESC,
    EXECUTE_THREAD_DESC,
    ITEM_ID_DESC,
)

__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Run a Rye item (tool or directive). Knowledge is not executable — use rye fetch."

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_id": {
            "type": "string",
            "description": ITEM_ID_DESC,
        },
        "parameters": {
            "type": "object",
            "description": EXECUTE_PARAMETERS_DESC,
            "default": {},
        },
        "dry_run": {
            "type": "boolean",
            "description": EXECUTE_DRY_RUN_DESC,
            "default": False,
        },
        "target": {
            "type": "string",
            "default": "local",
            "description": EXECUTE_TARGET_DESC,
        },
        "thread": {
            "type": "string",
            "enum": ["inline", "fork"],
            "description": EXECUTE_THREAD_DESC,
            "default": "inline",
        },
        "async": {
            "type": "boolean",
            "description": EXECUTE_ASYNC_DESC,
            "default": False,
        },
    },
    "required": ["item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.actions.execute import ExecuteTool

        raw_params = params.get("parameters", {})
        if isinstance(raw_params, str):
            try:
                raw_params = json.loads(raw_params)
            except (json.JSONDecodeError, TypeError):
                raw_params = {"raw_input": raw_params}

        tool = ExecuteTool(project_path=project_path)
        result = asyncio.run(tool.handle(
            item_id=params["item_id"],
            project_path=project_path,
            parameters=raw_params,
            dry_run=params.get("dry_run", False),
            target=params.get("target", "local"),
            thread=params.get("thread", "inline"),
            **{"async": params.get("async", False)},
        ))
        return result
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    import sys
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
