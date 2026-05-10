# ryeos:signed:2026-05-10T03:17:43Z:0f79b40fe686afaf3d49ccd192ff5644c3df5aec4bbe75505bcf7bf6865e4566:AA9yOakfQp/O/P+/nmFTR1dtBgPGc60ifYnjigNLf0qny1BhlaHRyaJvLrOGfnEoDHc8GoR6L30z8CXnilNeBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""YAML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "ryeos/core/parsers/yaml"
__description__ = "YAML parser - parse YAML content into Python dictionaries"

import yaml


def parse(content):
    """Parse YAML content."""
    try:
        return {"data": yaml.safe_load(content) or {}, "content": content}
    except Exception:
        return {"data": {}, "content": content}
