# rye:signed:2026-02-16T06:58:52Z:b449f1d2b0d655247ed3bf39703ffa3afad1ca4b7798460210a16db3b15431b0:6T1Bmb6AwcMzAGWhTlsL37KT3_QU10BYryeezCZSa7m1zbpfmd_PeeoRZjMtYpW-WBu_jN0p4QV9LbPDLkZKCQ==:440443d0858f0199
"""Validate and sign a directive, tool, or knowledge item."""

import argparse
import json
import asyncio

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/primary"
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
