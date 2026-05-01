# rye:signed:2026-05-01T04:58:53Z:88fb8c1dc00c1a4170389ebc4b8b3f5773efb04c64e9f2047ce91c1b384ac938:RwF9YytLTyHboZf+V6ArLkuK90bpNilTC0KopF8N+uV5/hKVqPP6xcBPMBdZCTjm/ZUlkOm01oTpiUt7+v+gDA==:09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c
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
