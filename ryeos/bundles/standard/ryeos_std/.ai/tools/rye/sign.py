# rye:signed:2026-03-31T07:27:23Z:4ed46d82c918090ac7e93e4888298111acf84f1020765e73f30f16f71a85fd91:m3H09WqlRajRWVl07WA0K12WfYJIqzYSXkjhR5JaSUCWkBo9H-R94HWhpiY45AWGKGU0G9pJtOhaGNlDxAYnDw:4b987fd4e40303ac
"""Validate and sign a directive, tool, knowledge, or config item."""

import argparse
import json
import asyncio

from rye.constants import ItemType
from rye.primary_action_descriptions import (
    ITEM_TYPE_DESC,
    SIGN_ITEM_ID_DESC,
    SIGN_SOURCE_DESC,
)

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Validate structure and write an Ed25519 signature to a Rye item file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "item_type": {
            "type": "string",
            "enum": ItemType.SIGNABLE,
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
        from rye.actions.sign import SignTool

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
    import sys
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
