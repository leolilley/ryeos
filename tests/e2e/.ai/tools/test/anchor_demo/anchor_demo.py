# rye:signed:2026-02-14T00:22:16Z:ac84c88e6bc82acd69566a7e9c0bbc95c7bcbec7830f0217ee7f2970818a8098:QxEPjk85cmhH3paGe__JNOHPxRFKQm-4368JhRx7ggPQhaudTPH5THGr-aUkXGd3ltcKidcs-_ySee_2TAE3Cg==:440443d0858f0199
"""Anchor demo tool - tests the anchor system with multi-file imports.

Imports a sibling helper module via PYTHONPATH injection from the
anchor system. If anchor doesn't work, `from helpers import ...` fails.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
__category__ = "test/anchor_demo"
__tool_description__ = "Demo tool testing anchor system with sibling imports"

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
