# rye:signed:2026-02-14T00:35:18Z:dfd6f7cde3abf1a990f9d081863fc66dcc8e11b33680fa04b6e6bc6a0436b521:01T2V0l4RiWheSKMvFqsuTsAyb9EpOUtPCjwgIXRHQ1ueXXjbU5aqVIx7mYeUFS1GzCu3y1mS6UxocQnY4CZDw==:440443d0858f0199
"""Search for directives, tools, or knowledge items."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary-tools"
__tool_description__ = "Search for directives, tools, or knowledge items by query"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "query": {
            "type": "string",
            "description": "Search query (supports AND, OR, NOT, wildcards, phrases)",
        },
        "item_type": {
            "type": "string",
            "enum": ["directive", "tool", "knowledge"],
            "description": "Type of items to search",
        },
        "source": {
            "type": "string",
            "enum": ["project", "user", "system", "all"],
            "default": "project",
            "description": "Space to search in",
        },
        "limit": {
            "type": "integer",
            "default": 10,
            "description": "Maximum results to return",
        },
    },
    "required": ["query", "item_type"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.search import SearchTool

        tool = SearchTool()
        result = asyncio.run(tool.handle(
            query=params["query"],
            item_type=params["item_type"],
            project_path=project_path,
            source=params.get("source", "project"),
            limit=params.get("limit", 10),
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
