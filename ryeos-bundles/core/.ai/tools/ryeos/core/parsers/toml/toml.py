# ryeos:signed:2026-05-09T10:22:04Z:44ee9121be9f28dd7a29ca5c7a534e649b223b5589c98f71470837745c51d8d6:zYZg4l5dkPjhSlR1Ayo29Ne7qzsj6MxYLuRkw3Cvnp4z33vhDAXeV0IXTfPRhRoHOuXXmpfz72lBkPITZSdzBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""TOML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "ryeos/core/parsers/toml"
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
