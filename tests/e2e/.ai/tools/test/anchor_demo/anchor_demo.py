# rye:signed:2026-03-10T04:07:19Z:186450e22c28b23086a4cfdc235607cf130b3f77fe7164f8a2d35df4e53cbfb5:uI35E-cq-gM1_x1tDU12bMPmd95IEUiCXe03lFzqXuyO_T4d7o1V7lGL7Zdx_VoOSKP1Fpm82opa50RXR292AQ==:4b987fd4e40303ac
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
