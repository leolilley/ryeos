# rye:signed:2026-02-26T06:42:42Z:72eb70e34d07eb466071a74357fbed98529e14e68947fe2909bf500cf2e41c96:bXLjlypwj3jWNpQo_IDLArn732JW_FTnOzAN782YgsjULimsbi2EWvmziHln93sG3BhqKhtHEAWTXTPB213_Ag==:4b987fd4e40303ac
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
