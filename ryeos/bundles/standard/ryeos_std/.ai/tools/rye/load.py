# rye:signed:2026-03-12T01:20:23Z:47c4e354dd363f312fc9672740c7d7ec7cf0d3924233ee93277eb32c6786f124:CXPBcYq6VVaHKj2-8lsHK17W3g9YH815Ya33SxaHGnshy43hiJQBMhnomNjeIaqe-W4gtWssDBHoqBH04vNMCw==:4b987fd4e40303ac
"""Load item content for inspection."""

import argparse
import json
import asyncio

from rye.primary_action_descriptions import (
    ITEM_ID_DESC,
    ITEM_TYPE_DESC,
    LOAD_DESTINATION_DESC,
    LOAD_SOURCE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
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
            "enum": ["project", "user", "system", "registry"],
            "description": LOAD_SOURCE_DESC,
        },
        "destination": {
            "type": "string",
            "enum": ["project", "user"],
            "description": LOAD_DESTINATION_DESC,
        },
        "version": {
            "type": "string",
            "description": "Version to pull (registry source only).",
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
        if "version" in params:
            kwargs["version"] = params["version"]
        result = asyncio.run(tool.handle(**kwargs))
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
