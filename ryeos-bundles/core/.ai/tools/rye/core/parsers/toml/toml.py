# rye:signed:2026-04-23T23:06:25Z:8febd1a77e329a7ea74a168404f10b5ad6cf99f21f9b5fae752747af6091fc75:GoA8D+KP6ThHEN1rKvGh5odtBsLV1eXoe6sKDJsGcSnN3r2gyOwlf+xdigfjEdR31Ie0waNexjIlXKLACLSpBw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""TOML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/toml"
__tool_description__ = "TOML parser - parse TOML content into Python dictionaries"

import sys

if sys.version_info >= (3, 11):
    import tomllib
else:
    try:
        import tomli as tomllib
    except ImportError:
        tomllib = None


def parse(content):
    """Parse TOML content."""
    if tomllib is None:
        return {"error": "No TOML parser available. Install tomli for Python < 3.11."}
    try:
        return {"data": tomllib.loads(content) or {}, "content": content}
    except Exception as e:
        return {"error": f"TOML parse error: {e}", "content": content}
