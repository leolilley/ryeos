# rye:signed:2026-04-29T02:47:29Z:88fb8c1dc00c1a4170389ebc4b8b3f5773efb04c64e9f2047ce91c1b384ac938:Yz3v9Y6wv9q6D+nV7tvcqL9HZS4ICFxJJrq7ibJQsXQtgpP3S00IQq2XBO7x4540ZBdO5jQuFoyubZmYVQZZDA==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""TOML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/toml"
__description__ = "TOML parser - parse TOML content into Python dictionaries"

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
