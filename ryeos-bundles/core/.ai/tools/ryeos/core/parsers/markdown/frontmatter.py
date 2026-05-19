# ryeos:signed:2026-05-19T01:36:20Z:f72cc3eb12ba09a220324c18c732d5b06500795139cd0fd3a511ca5c505ea29d:jdSVuRSv80wDGqf3RopgyFO1Y5KO7Rk33UBsUhf1wTBnE30HifB/Z2i/7YKOjVtGZz4Iu0NBILM1ZyzcS1zfAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea
"""Markdown YAML parser for knowledge entries.

Extracts YAML metadata from --- frontmatter (canonical) or ```yaml
code fences (backward compat), and separates it from body content.
Also handles pure YAML files (.yaml/.yml) with no fences.
"""

__version__ = "3.0.0"
__tool_type__ = "parser"
__category__ = "ryeos/core/parsers/markdown"
__description__ = (
    "Markdown YAML parser - extracts YAML metadata from frontmatter "
    "or code fences in markdown files"
)

import re
from typing import Any, Dict, Optional, Tuple

import yaml


def _skip_signature_comment(content: str) -> str:
    stripped = content.lstrip()
    if stripped.startswith("<!--"):
        end = stripped.find("-->")
        if end != -1:
            return stripped[end + 3:].lstrip("\r\n")
    return content


def _extract_dashes_frontmatter(content: str) -> Tuple[Optional[str], str]:
    """Extract YAML from --- frontmatter (canonical markdown format).

    Returns (yaml_content, body) where:
      - yaml_content: the YAML string between the delimiters, or None
      - body: everything after the closing ---
    """
    stripped = _skip_signature_comment(content)
    if not stripped.startswith("---"):
        return None, ""

    after_opening = stripped[3:]
    # The character right after --- must be a newline or EOF
    if after_opening and after_opening[0] not in ("\n", "\r"):
        return None, ""  # e.g. ----something, not a frontmatter delimiter

    rest = after_opening.lstrip("\r\n")

    # Find closing --- on its own line
    lines = rest.splitlines()
    for i, line in enumerate(lines):
        if line.strip() == "---":
            yaml_str = "\n".join(lines[:i]).strip()
            body_lines = lines[i + 1:]
            body = "\n".join(body_lines).strip()
            return yaml_str, body

    return None, ""  # unclosed — fall through to other formats


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

    For .md files: extracts YAML metadata, body is the rest.
    Tries formats in order:
      1. --- frontmatter (canonical, standard markdown convention)
      2. ```yaml fenced code block (backward compat)
      3. Pure YAML (.yaml/.yml files)
    """
    result: Dict[str, Any] = {
        "body": "",
        "raw": content,
    }

    # 1. Try --- frontmatter (canonical)
    yaml_str, body = _extract_dashes_frontmatter(content)
    if yaml_str is not None:
        data = yaml.safe_load(yaml_str)
        if isinstance(data, dict):
            result.update(data)
        result["body"] = body
        return result

    # 2. Try ```yaml code fence (backward compat)
    yaml_str, body = _extract_yaml_block(content)
    if yaml_str is not None:
        data = yaml.safe_load(yaml_str)
        if isinstance(data, dict):
            result.update(data)
        result["body"] = body
        return result

    # 3. Pure YAML file — strip signature comment then parse as metadata
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
