# rye:signed:2026-04-29T02:47:29Z:e021397d26bf21b327a872eff56571bd27a0524d8d49618da26e31d224a062cd:JFzVHAqIKk7CWt79492bPmK9mmU4KQkeZ5C90D7cPaGxHEsgfzTd+HCPSvYNODOtmJUnA9YwfvgvFz4/GQIFCg==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
"""JavaScript/TypeScript parser for extracting metadata from JS/TS tools.

Extracts exported constants and CONFIG_SCHEMA using regex.
Handles both `export const` and bare `const` declarations.
"""

__version__ = "1.0.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers/javascript"
__description__ = (
    "JavaScript/TypeScript parser - extracts metadata from JS/TS source code"
)

import json
import re
from typing import Any, Dict, Optional


def parse(content: str) -> Dict[str, Any]:
    """Parse JS/TS source and extract metadata.

    Returns dict of module-level constants, matching the python/ast parser
    output format (keys like __version__, __tool_type__, CONFIG_SCHEMA).
    """
    result: Dict[str, Any] = {
        "raw": content,
    }

    # Extract string-valued __dunder__ constants
    # Matches: export const __version__ = "1.0.0"
    #          const __version__ = '1.0.0'
    for match in re.finditer(
        r'(?:export\s+)?(?:const|let|var)\s+(__\w+__)\s*=\s*["\']([^"\']*)["\']',
        content,
    ):
        result[match.group(1)] = match.group(2)

    # Handle null assignments: export const __executor_id__ = null
    for match in re.finditer(
        r"(?:export\s+)?(?:const|let|var)\s+(__\w+__)\s*=\s*null\b",
        content,
    ):
        key = match.group(1)
        if key not in result:
            result[key] = None

    # Extract CONFIG_SCHEMA via balanced brace matching
    schema_match = re.search(
        r"(?:export\s+)?(?:const|let|var)\s+CONFIG_SCHEMA\s*"
        r"(?::\s*\w+)?\s*=\s*(\{)",
        content,
    )
    if schema_match:
        schema_str = _extract_balanced(content, schema_match.start(1), "{", "}")
        if schema_str:
            parsed = _try_parse_js_object(schema_str)
            if parsed is not None:
                result["CONFIG_SCHEMA"] = parsed

    # Extract ENV_CONFIG
    env_match = re.search(
        r"(?:export\s+)?(?:const|let|var)\s+ENV_CONFIG\s*" r"(?::\s*\w+)?\s*=\s*(\{)",
        content,
    )
    if env_match:
        env_str = _extract_balanced(content, env_match.start(1), "{", "}")
        if env_str:
            parsed = _try_parse_js_object(env_str)
            if parsed is not None:
                result["ENV_CONFIG"] = parsed

    # Extract CONFIG
    config_match = re.search(
        r"(?:export\s+)?(?:const|let|var)\s+CONFIG\s*" r"(?::\s*\w+)?\s*=\s*(\{)",
        content,
    )
    if config_match:
        config_str = _extract_balanced(content, config_match.start(1), "{", "}")
        if config_str:
            parsed = _try_parse_js_object(config_str)
            if parsed is not None:
                result["CONFIG"] = parsed

    return result


def _extract_balanced(
    content: str, start: int, open_ch: str, close_ch: str
) -> Optional[str]:
    """Extract a balanced block starting at `start`."""
    depth = 0
    for i in range(start, len(content)):
        if content[i] == open_ch:
            depth += 1
        elif content[i] == close_ch:
            depth -= 1
            if depth == 0:
                return content[start : i + 1]
    return None


def _try_parse_js_object(text: str) -> Optional[Any]:
    """Best-effort conversion of a JS object literal to a Python dict.

    Handles unquoted keys, single quotes, and trailing commas.
    Returns None if parsing fails.
    """
    try:
        # Strip JS comments (// and /* */)
        cleaned = re.sub(r"//[^\n]*", "", text)
        cleaned = re.sub(r"/\*.*?\*/", "", cleaned, flags=re.DOTALL)
        # Quote unquoted keys
        cleaned = re.sub(r"(?<=[{,\n])\s*(\w+)\s*:", r' "\1":', cleaned)
        # Single quotes → double quotes
        cleaned = cleaned.replace("'", '"')
        # Trailing commas
        cleaned = re.sub(r",\s*([}\]])", r"\1", cleaned)
        return json.loads(cleaned)
    except (json.JSONDecodeError, ValueError):
        return None
