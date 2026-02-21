# rye:signed:2026-02-21T05:56:40Z:1809b1141acdcc8a3cde3d5b62427bd127b1b9aaf1e041503d4269127972625f:07iwDr7UE4FxWkcoQdJO5R0BO0TQJQOKqZj7artUcNd-CZRaGC0ByUc-TldLvFH-5itN4u7cN7lCNyQQwwUkAA==:9fbfabe975fa5a7f
"""YAML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers"
__tool_description__ = "YAML parser - parse YAML content into Python dictionaries"

import yaml


def parse(content):
    """Parse YAML content."""
    try:
        return {"data": yaml.safe_load(content) or {}, "content": content}
    except Exception:
        return {"data": {}, "content": content}
