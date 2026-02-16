# rye:signed:2026-02-16T06:58:52Z:2f3aaf6a8b7e762bf2edf98fb3d24127f6143021a72bf8bfcaf37937bb107615:c7y2kkduil52Cc59Rc3gOjkerkJJXRlp52_gHLSIow06-ArcHRiWSziobPI2ALPOZrhB4GMXmTzyY5ojupXBDA==:440443d0858f0199
"""Load item content for inspection."""

import argparse
import json
import asyncio

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary"
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
        "destination": {
            "type": "string",
            "enum": ["project", "user"],
            "description": "Copy item to this space",
        },
    },
    "required": ["item_type", "item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.load import LoadTool

        tool = LoadTool()
        kwargs = {
            "item_type": params["item_type"],
            "item_id": params["item_id"],
            "project_path": project_path,
            "source": params.get("source", "project"),
        }
        if "destination" in params:
            kwargs["destination"] = params["destination"]
        result = asyncio.run(tool.handle(**kwargs))
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
