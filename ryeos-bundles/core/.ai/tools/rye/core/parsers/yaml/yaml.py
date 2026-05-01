# rye:signed:2026-05-01T04:49:09Z:7351ff144821e1a2b1b46afb86fd9ff149c758411f2e6a748be5d2c6b6bb43a8:P+4NWlNNPdtrAnnUym2sumsZNSFME0OEKZsgcZE8cg2TRuXkBLgpOsdQX5CHPVKBORnFOw3RUelAHRT4rOAjAg==:09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c
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
