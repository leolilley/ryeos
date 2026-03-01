# rye:signed:2026-03-01T08:42:55Z:9834ae53aa07b73045a6cfe19080c82aef4345f7df3372bd62346faf7973ab95:4XgewIBT2rjYClwZJ4GhbTm62386AYqDP_8qBTkCgf47Ni89dvS9CMOsON_S_gUbpwMdSHAnqt-4p3XARqH7AA==:4b987fd4e40303ac
"""Search for directives, tools, or knowledge items."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

from rye.primary_tool_descriptions import (
    SEARCH_LIMIT_DESC,
    SEARCH_QUERY_DESC,
    SEARCH_SCOPE_DESC,
    SEARCH_SPACE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/primary"
__tool_description__ = "Discover item IDs before calling execute or load"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "query": {
            "type": "string",
            "description": SEARCH_QUERY_DESC,
        },
        "scope": {
            "type": "string",
            "description": SEARCH_SCOPE_DESC,
        },
        "space": {
            "type": "string",
            "enum": ["project", "user", "system", "all"],
            "default": "all",
            "description": SEARCH_SPACE_DESC,
        },
        "limit": {
            "type": "integer",
            "default": 10,
            "description": SEARCH_LIMIT_DESC,
        },
    },
    "required": ["query", "scope"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.search import SearchTool

        tool = SearchTool()
        result = asyncio.run(tool.handle(
            query=params["query"],
            scope=params["scope"],
            project_path=project_path,
            space=params.get("space", "all"),
            limit=params.get("limit", 10),
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
