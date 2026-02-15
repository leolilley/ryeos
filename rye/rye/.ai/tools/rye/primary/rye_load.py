# rye:signed:2026-02-14T00:35:18Z:15d955f52cfd757c6868fc02c2be72571549645d8ea4db6f51c8fb2746e21f76:ScJzCBQW00gey0ZoOB3cjm9DsyHMRR_42yWiNahtX_yZl2ipoMG_pk81cRJrgeNtOpdF4RHuOu0xqqoOKVrdBw==:440443d0858f0199
"""Load item content for inspection."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary-tools"
__tool_description__ = "Load a directive, tool, or knowledge item for inspection"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_type": {
            "type": "string",
            "enum": ["directive", "tool", "knowledge"],
            "description": "Type of item to load",
        },
        "item_id": {
            "type": "string",
            "description": "ID of the item to load (relative path without extension)",
        },
        "source": {
            "type": "string",
            "enum": ["project", "user", "system"],
            "default": "project",
            "description": "Space to load from",
        },
    },
    "required": ["item_type", "item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.load import LoadTool

        tool = LoadTool()
        result = asyncio.run(tool.handle(
            item_type=params["item_type"],
            item_id=params["item_id"],
            project_path=project_path,
            source=params.get("source", "project"),
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
