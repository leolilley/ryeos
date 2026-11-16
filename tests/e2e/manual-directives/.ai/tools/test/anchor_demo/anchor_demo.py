# ryeos:signed:2026-11-16T03:23:52Z:9a8f419a8dc1b1380c405d6a52d27cfb7cd5836f9cdce3af7555f580b84b14ee:9yS9jS2+QPfIPiYR4At1nSJu+V9mScqtApRkpX/SBVVXq6HACEs4cW3tpfTilnTFFtX2MwtCQkijWN4wbIMJCQ==:ae0ca349cdef0b514a7589669b7b5ff12ac666d38374eb657abd94b53aeccbfb
# ryeos-tool:
#   category: test/anchor_demo
#   version: "1.0.0"
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   execution_protocol: "protocol:ryeos/core/tool_callback"
#   description: "Demo tool testing anchor system with sibling imports"
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
