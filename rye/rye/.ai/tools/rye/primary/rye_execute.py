# rye:signed:2026-02-14T00:35:18Z:bd9bd839110700156d10eb29f4cf669e8f4cbe10431db990d3d9898fdec7c44c:Ig3a8Uu4Tfq1_OuBdWFgd2OUjnMD6xb-A6KUAExn3wQdF650jOgJnrBJzfp-C0fR6wrgz97qQlFlY2hn6fYRDQ==:440443d0858f0199
"""Execute a directive, tool, or knowledge item via rye."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary-tools"
__tool_description__ = "Execute a directive, tool, or knowledge item"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_type": {
            "type": "string",
            "enum": ["directive", "tool", "knowledge"],
            "description": "Type of item to execute",
        },
        "item_id": {
            "type": "string",
            "description": "ID of the item to execute (relative path without extension)",
        },
        "parameters": {
            "type": "object",
            "description": "Parameters to pass to the item",
            "default": {},
        },
        "dry_run": {
            "type": "boolean",
            "description": "Validate without executing",
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
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
