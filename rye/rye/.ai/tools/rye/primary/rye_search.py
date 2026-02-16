# rye:signed:2026-02-16T06:58:52Z:0d9db6838f4bfa5808872909b048c05429613d815f065efc6f5b56dde49a9cd0:4IcVAkx3noP5dFQkY0Zz-1sVUwqBHxbYyij0_jIpGAkmMIl4vrR8FfgCa5Ij0ed8caRIs1-KHeM5R-37zYK2AQ==:440443d0858f0199
"""Search for directives, tools, or knowledge items."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary"
__tool_description__ = "Search for directives, tools, or knowledge items by query"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "query": {
            "type": "string",
            "description": "Search query (supports AND, OR, NOT, wildcards, phrases)",
        },
        "scope": {
            "type": "string",
            "description": "Capability-format scope: rye.search.{item_type}.{namespace}.* or shorthand: directive, tool.rye.core.*",
        },
        "space": {
            "type": "string",
            "enum": ["project", "user", "system", "all"],
            "default": "all",
            "description": "Space to search in",
        },
        "limit": {
            "type": "integer",
            "default": 10,
            "description": "Maximum results to return",
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
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
