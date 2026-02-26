# rye:signed:2026-02-25T00:02:14Z:72eb70e34d07eb466071a74357fbed98529e14e68947fe2909bf500cf2e41c96:kSW5h6v5mlUe9Cr2Ya2Bbeb-B6eAK3PBIhDJUdCrvYWIsZCl6lDJeCOl6DYijaveUg0JQiEQ5pKlpiVi1mfmCQ==:9fbfabe975fa5a7f
"""YAML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/yaml"
__tool_description__ = "YAML parser - parse YAML content into Python dictionaries"

import yaml


def parse(content):
    """Parse YAML content."""
    try:
        return {"data": yaml.safe_load(content) or {}, "content": content}
    except Exception:
        return {"data": {}, "content": content}
