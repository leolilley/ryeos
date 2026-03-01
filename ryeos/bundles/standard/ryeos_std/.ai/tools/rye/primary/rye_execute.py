# rye:signed:2026-03-01T08:42:55Z:53d503dc1e7bf6e989f5afd64c45ebb0c44ea0fce59f50d6c1da0abdbe977827:EQOOec9G_8Mqc3xIo_5WvYhaRlHGoTbeuOFSxKXl009DZRBfBCv18eBPsn7IFzkfkJ8_rvzJgyYAoF6y5h9oCw==:4b987fd4e40303ac
"""Execute a directive, tool, or knowledge item via rye."""

import argparse
import json
import asyncio

from rye.primary_tool_descriptions import (
    EXECUTE_DRY_RUN_DESC,
    EXECUTE_PARAMETERS_DESC,
    ITEM_ID_DESC,
    ITEM_TYPE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/primary"
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
        ))
        return result
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", default=None, help="Parameters as JSON (legacy, prefer stdin)")
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    import sys
    params = json.loads(args.params) if args.params else json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
