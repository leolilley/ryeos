# rye:signed:2026-04-06T04:14:24Z:e8742bb9c670a715f7864c09f3f6a488b50c1010a9b5e9e7522e3e31bb966e52:5JgjLMua-rmYk7ziwU4X43YmARkJVSvRtDLz95YSH5ifkOQ-fjnseIE4Nh3FXjfTKXCxl1QYv5ukBtV6hWH7Dg:4b987fd4e40303ac
"""Resolve items by ID or discover by query."""

import argparse
import json
import sys
import asyncio

from rye.primary_action_descriptions import (
    FETCH_DESTINATION_DESC,
    FETCH_LIMIT_DESC,
    FETCH_QUERY_DESC,
    FETCH_SCOPE_DESC,
    FETCH_SOURCE_DESC,
    ITEM_ID_DESC,
    ITEM_TYPE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Resolve items by ID or discover by query"

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
        "query": {
            "type": "string",
            "description": FETCH_QUERY_DESC,
        },
        "scope": {
            "type": "string",
            "description": FETCH_SCOPE_DESC,
        },
        "source": {
            "type": "string",
            "enum": ["project", "user", "system", "local", "registry", "all"],
            "description": FETCH_SOURCE_DESC,
        },
        "destination": {
            "type": "string",
            "enum": ["project", "user"],
            "description": FETCH_DESTINATION_DESC,
        },
        "version": {
            "type": "string",
            "description": "Version to pull (registry source only).",
        },
        "limit": {
            "type": "integer",
            "default": 10,
            "description": FETCH_LIMIT_DESC,
        },
    },
    "required": [],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.actions.fetch import FetchTool

        tool = FetchTool()
        kwargs = {"project_path": project_path}
        for key in ("item_type", "item_id", "query", "scope", "source", "destination", "version", "limit"):
            if key in params and params[key] is not None:
                kwargs[key] = params[key]
        result = asyncio.run(tool.handle(**kwargs))
        return result
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
