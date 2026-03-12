# rye:signed:2026-03-12T01:20:23Z:cf91b660c0562afd765f9922591db051ce356be5e76e5adb4619c2901261d356:j_epJJKKYcnbGzORUXlMCq2E318FZLnCe0vCR0JSYkV3sJzV22nN8QHpl0lCK0h-t2rlWhIUArd4cwnWtUexCw==:4b987fd4e40303ac
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
    import sys
    parser = argparse.ArgumentParser()
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    params = json.loads(sys.stdin.read())
    result = execute(params, args.project_path)
    print(json.dumps(result))
