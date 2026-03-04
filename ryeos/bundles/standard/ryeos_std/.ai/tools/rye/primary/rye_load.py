# rye:signed:2026-03-04T01:23:09Z:c0f79432a135a1ac1a539d19f134937d2d3046c150d6083df50be6932c9841fa:m-rAWuNBX4HMYnh-Lm56QCONFAh-yv8espuJEN8eN0fJxtUnxg4zDxIm5n4TSkp-1fOxV5oFgkNlZn1XBJezCw==:4b987fd4e40303ac
"""Load item content for inspection."""

import argparse
import json
import asyncio

from rye.primary_tool_descriptions import (
    ITEM_ID_DESC,
    ITEM_TYPE_DESC,
    LOAD_DESTINATION_DESC,
    LOAD_SOURCE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/primary"
__tool_description__ = "Read raw content and metadata of a Rye item for inspection"

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
        "source": {
            "type": "string",
            "enum": ["project", "user", "system"],
            "description": LOAD_SOURCE_DESC,
        },
        "destination": {
            "type": "string",
            "enum": ["project", "user"],
            "description": LOAD_DESTINATION_DESC,
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
            "source": params.get("source"),
        }
        if "destination" in params:
            kwargs["destination"] = params["destination"]
        result = asyncio.run(tool.handle(**kwargs))
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
