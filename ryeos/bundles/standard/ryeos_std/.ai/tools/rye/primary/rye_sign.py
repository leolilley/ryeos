# rye:signed:2026-03-03T22:32:56Z:ade6e713f13b025e3f94c9a516bfc86a53f194935231b22b46024dda1f3e8090:rm_7O8pHFQOfSh4vdEWurKVheBLeEfRu2s6qSj5b0_XtEFU259do4iaP05udgLUlijfLkt8UPtywn7OVm-HdCg==:4b987fd4e40303ac
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
__executor_id__ = "rye/core/runtimes/python/script"
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
    parser.add_argument("--params", default=None, help="Parameters as JSON (legacy, prefer stdin)")
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    import sys
    params = json.loads(args.params) if args.params else json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
