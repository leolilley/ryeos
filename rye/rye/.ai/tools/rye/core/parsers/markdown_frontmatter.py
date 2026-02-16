# rye:signed:2026-02-16T07:16:19Z:6dacbf26764fb8feccb4d590da0eb757b5b8e2be32b51790226e1b9d4584dcfc:E7OdqL0CUZ5TJOCvPlzECcSGpQ_EyoiBpKnc4PJwvRaeZNdw0k5Qq683-5aIxtyZwOGVSnyHmFlZUHNbOSWdAw==:440443d0858f0199
"""Markdown frontmatter parser for knowledge entries.

Extracts YAML frontmatter and separates it from body content.
"""

__version__ = "1.1.0"
__tool_type__ = "parser"
__category__ = "rye/core/parsers"
__tool_description__ = (
    "Markdown frontmatter parser - extracts YAML frontmatter from markdown files"
)

from typing import Any, Dict


def parse(content: str) -> Dict[str, Any]:
    """Parse markdown with YAML frontmatter.

    Returns parsed frontmatter dict + body content.
    """
    result: Dict[str, Any] = {
        "body": "",
        "raw": content,
    }

    if not content.startswith("---"):
        # Pure YAML file — parse entire content as metadata
        try:
            import yaml
            data = yaml.safe_load(content)
            if isinstance(data, dict):
                result.update(data)
        except Exception:
            pass
        return result

    lines = content.split("\n")
    frontmatter_end = None

    # Find closing ---
    for i, line in enumerate(lines[1:], 1):
        if line.strip() == "---":
            frontmatter_end = i
            break

    if not frontmatter_end:
        return result

    # Parse frontmatter lines
    for line in lines[1:frontmatter_end]:
        if not line.strip() or line.strip().startswith("#"):
            continue

        if ":" not in line:
            continue

        key, value = line.split(":", 1)
        key = key.strip()
        value = value.strip().strip("'\"")

        # Handle list values
        if value.startswith("[") and value.endswith("]"):
            # Parse list
            items = value[1:-1].split(",")
            value = [item.strip().strip("'\"") for item in items if item.strip()]

        result[key] = value

    # Extract body
    body_start = frontmatter_end + 1
    if body_start < len(lines):
        result["body"] = "\n".join(lines[body_start:]).strip()

    return result
