# rye:signed:2026-04-19T09:49:53Z:72eb70e34d07eb466071a74357fbed98529e14e68947fe2909bf500cf2e41c96:bXLjlypwj3jWNpQo/IDLArn732JW/FTnOzAN782YgsjULimsbi2EWvmziHln93sG3BhqKhtHEAWTXTPB213/Ag==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
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
