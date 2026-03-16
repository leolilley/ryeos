# rye:signed:2026-03-16T09:27:24Z:0082af4676aa99ac86a68fadc01d1304ad4cb94b6a781d7b034b935d306516cf:e99YDyxAS7dCVzxrmmhrkXMRBHGDKFuVs1_hia5epQZzcctdba2HIVmUiYC-O3_3ApockSHJHKodWaCIs7pnBA==:4b987fd4e40303ac
"""Execute a directive, tool, or knowledge item via rye."""

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
    ITEM_TYPE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Run a Rye item (directive, tool, or knowledge)"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_type": {
            "type": "string",
            "enum": ["directive", "tool", "knowledge"],
            "description": ITEM_TYPE_DESC,
        },
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
    "required": ["item_type", "item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.execute import ExecuteTool

        raw_params = params.get("parameters", {})
        if isinstance(raw_params, str):
            try:
                raw_params = json.loads(raw_params)
            except (json.JSONDecodeError, TypeError):
                raw_params = {"raw_input": raw_params}

        tool = ExecuteTool(project_path=project_path)
        result = asyncio.run(tool.handle(
            item_type=params["item_type"],
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
