# rye:signed:2026-02-17T23:53:59Z:db14381140045aa3fe0ac7f43b079ccfad1ef055bf22da175d117bc56c94430a:EkWPOVarWB0HGQ4N1Z0acChSFJGNTsXRwwXXvBXERSI0oO_deOHm9sJpz-R_zgXPqG5dR0ESc9LC3-tWGAjBAA==:440443d0858f0199
"""Markdown YAML parser for knowledge entries.

Extracts YAML metadata from ```yaml code fences (matching how
directives use ```xml fences) and separates it from body content.
Also handles pure YAML files (.yaml/.yml) with no fences.
"""

__version__ = "2.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers"
__tool_description__ = (
    "Markdown YAML parser - extracts YAML metadata from code fences in markdown files"
)

import re
from typing import Any, Dict, Optional, Tuple

import yaml


def _extract_yaml_block(content: str) -> Tuple[Optional[str], str]:
    """Extract YAML from markdown ```yaml ... ``` block.

    Mirrors _extract_xml_block in markdown_xml.py.

    Returns (yaml_content, body) where:
      - yaml_content: the YAML string inside the fence, or None
      - body: everything after the closing fence
    """
    # Match ```yaml not inside an outer fence (````markdown etc.)
    outer_open_pat = re.compile(r"^`{4,}", re.MULTILINE)
    outer_close_pat = re.compile(r"^`{4,}\s*$", re.MULTILINE)

    for match in re.finditer(r"^```yaml\s*$", content, re.MULTILINE):
        before = content[: match.start()]
        if len(outer_open_pat.findall(before)) > len(outer_close_pat.findall(before)):
            continue  # Inside an outer fence — skip

        start = match.end() + 1

        # Find closing ```
        close_match = re.search(r"^```\s*$", content[start:], re.MULTILINE)
        if close_match:
            yaml_content = content[start : start + close_match.start()].strip()
            after_fence = start + close_match.end()
            body = content[after_fence:].strip()
            return yaml_content, body

    return None, ""


def parse(content: str) -> Dict[str, Any]:
    """Parse knowledge entry content.

    For .md files: extracts YAML from ```yaml code fence, body is the rest.
    For .yaml/.yml files: parses entire content as YAML metadata.
    """
    result: Dict[str, Any] = {
        "body": "",
        "raw": content,
    }

    # Try ```yaml code fence extraction
    yaml_str, body = _extract_yaml_block(content)
    if yaml_str is not None:
        data = yaml.safe_load(yaml_str)
        if isinstance(data, dict):
            result.update(data)
        result["body"] = body
        return result

    # Pure YAML file — strip signature comment then parse as metadata
    try:
        stripped = content
        if stripped.startswith("<!--"):
            end = stripped.find("-->")
            if end != -1:
                stripped = stripped[end + 3:].lstrip("\n")
        data = yaml.safe_load(stripped)
        if isinstance(data, dict):
            result.update(data)
    except Exception:
        pass

    return result
