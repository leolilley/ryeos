# rye:signed:2026-04-29T02:47:29Z:7351ff144821e1a2b1b46afb86fd9ff149c758411f2e6a748be5d2c6b6bb43a8:XLLtgIF+PlvzJ2UAw6ihnIWFyb25amZbfjW1xC6nLsPAqNF53GOx3ckxjQZ34GkEvzq4/hOHftOz9wEA5qeMDQ==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""YAML parser for RYE."""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/yaml"
__description__ = "YAML parser - parse YAML content into Python dictionaries"

import yaml


def parse(content):
    """Parse YAML content."""
    try:
        return {"data": yaml.safe_load(content) or {}, "content": content}
    except Exception:
        return {"data": {}, "content": content}
