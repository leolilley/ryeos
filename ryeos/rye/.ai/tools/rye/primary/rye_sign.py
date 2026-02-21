# rye:signed:2026-02-21T05:56:40Z:c5f7cafb9afd4f22a5f452b5de56c8d1e987fadcb605bd18ad9365ae5377f86d:gZdmQ6-8gNeYm4wysqhlId-skBeUTqn65la3qvUq9_6ejb6vTAZprqpBmIruwRPgX8qHxY49dI2MEZl4R2zeDg==:9fbfabe975fa5a7f
"""Validate and sign a directive, tool, or knowledge item."""

import argparse
import json
import asyncio

from rye.primary_tool_descriptions import (
    ITEM_TYPE_DESC,
    SIGN_ITEM_ID_DESC,
    SIGN_SOURCE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary"
__tool_description__ = "Validate structure and write an Ed25519 signature to a Rye item file"

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
            "description": SIGN_ITEM_ID_DESC,
        },
        "source": {
            "type": "string",
            "enum": ["project", "user"],
            "default": "project",
            "description": SIGN_SOURCE_DESC,
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
