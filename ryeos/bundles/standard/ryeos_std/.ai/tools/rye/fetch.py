# rye:signed:2026-04-10T00:57:19Z:dde97f9db693d10103b0f7c46f62106de857e865928f492dd7cadcf5cec25e20:URJWmZK2_H3OIp0dUxuAhZE8n1mDqDlcWM169_ugLAHTYr38g5Z4_OFZ8QiRZxy5pBihCduMf9QvCqW_ZQLqDg:4b987fd4e40303ac
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
)

__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Resolve items by ID or discover by query"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
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
        for key in ("item_id", "query", "scope", "source", "destination", "version", "limit"):
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
