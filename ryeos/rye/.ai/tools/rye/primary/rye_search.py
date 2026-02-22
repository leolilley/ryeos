# rye:signed:2026-02-22T09:00:56Z:b16c866ace36f124d2869a0470cba58270b4cd302a972d44038a98085c7e5c23:hfkxd33LamRZgCMsLTDRxg_4phMBJsl5IGRHDssjd7XugUmd7pptbbXRF1YCeLx0XdqXeSLhQy6XxVMVffTKBw==:9fbfabe975fa5a7f
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
__executor_id__ = "rye/core/runtimes/python_script_runtime"
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
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
