# rye:signed:2026-02-14T00:35:18Z:74d34ffaaf9830b59ca5fd54ef6f8cff0858034a1cf0c271cfd955820c18d76c:x9KBUC44Q-yYGzR4ULb6ctgLp4GNVfhYzb5h-smYUqyaZY6F59jt3rT-N3XDvJ9e-BFrlqvUXs85yIQqSQYfAA==:440443d0858f0199
"""Validate and sign a directive, tool, or knowledge item."""

import argparse
import json
import sys
import asyncio
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary-tools"
__tool_description__ = "Validate and sign a directive, tool, or knowledge item"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_type": {
            "type": "string",
            "enum": ["directive", "tool", "knowledge"],
            "description": "Type of item to sign",
        },
        "item_id": {
            "type": "string",
            "description": "ID of the item to sign (relative path without extension)",
        },
        "source": {
            "type": "string",
            "enum": ["project", "user"],
            "default": "project",
            "description": "Space to sign in",
        },
    },
    "required": ["item_type", "item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.tools.sign import SignTool

        tool = SignTool()
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
