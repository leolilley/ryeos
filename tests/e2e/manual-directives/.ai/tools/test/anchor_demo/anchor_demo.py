# ryeos:signed:2026-06-07T05:42:18Z:d5fd48d63dcac5bc4ff7cdb7feffb673cd7b5285ff9d102148238df2f367ab9d:Kew/7lOykavT7KyqdBBVupqKeJR4VR5056S9aNO53Ggy127153vZy+9UvnT5YrHRAz+1MpTn4Yg+hMT2vUZXDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
# ryeos-tool:
#   category: test/anchor_demo
#   version: "1.0.0"
#   tool_type: python
#   executor_id: ryeos/core/runtimes/python/function
#   tool_description: "Demo tool testing anchor system with sibling imports"
"""Anchor demo tool - tests the anchor system with multi-file imports.

Imports a sibling helper module via PYTHONPATH injection from the
anchor system. If anchor doesn't work, `from helpers import ...` fails.
"""

import json
from pathlib import Path

from helpers import format_greeting

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "name": {"type": "string", "description": "Name to greet"},
        "style": {"type": "string", "default": "friendly"},
    },
    "required": ["name"],
}


def execute(params: dict, project_path: str) -> dict:
    """Execute the anchor demo tool."""
    name = params.get("name", "World")
    style = params.get("style", "friendly")

    greeting = format_greeting(name, style)

    return {
        "success": True,
        "greeting": greeting,
        "anchor_worked": True,
        "tool_dir": str(Path(__file__).parent),
    }
