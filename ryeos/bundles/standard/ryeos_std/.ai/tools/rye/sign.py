# rye:signed:2026-04-10T08:31:57Z:78dd1d36e791570314885ae8d0388f6b65f702e9444aa29b6984aca2ebe03743:wy5MmZ-UDyTos3fngDsruGFNQWWZPdzyUrCgKs20UXsTelJdsqRCG2rzaGu7-MSNKj2ffPIM8thyv-1ojZyqBA:4b987fd4e40303ac
"""Validate and sign a Rye item file."""

import argparse
import json
import asyncio

from rye.primary_action_descriptions import (
    SIGN_ITEM_ID_DESC,
    SIGN_SOURCE_DESC,
)

__version__ = "2.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye"
__tool_description__ = "Validate structure and write an Ed25519 signature to a Rye item file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
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
    "required": ["item_id"],
}


def execute(params: dict, project_path: str) -> dict:
    try:
        from rye.actions.sign import SignTool

        tool = SignTool()
        result = asyncio.run(tool.handle(
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
