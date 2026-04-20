# rye:signed:2026-04-19T09:49:53Z:9bedbd4aa24c1dc6b6154f6732996dafea6da76ebaf9360445e890f31af604eb:4nMa9obwAXFCwUND0qbwMwOwI2UXn6SGZaC4GulSxWcWGu1wlM4rUaN6GhQxcJ9XLTjw58G2v7rpq/geTRu3Dw==:8f4c002347bcb25b80e32a9f5ba7064638f0d372b8dd5cfbff3da765f94ef4bb
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/internal"
__tool_description__ = "Parse tool calls from LLM text responses"

import re
from typing import Dict, List

INVOKE_RE = re.compile(
    r'<invoke\s+name="([^"]+)">(.*?)</invoke>',
    re.DOTALL,
)

PARAM_RE = re.compile(
    r'<parameter\s+name="([^"]+)">(.*?)</parameter>',
    re.DOTALL,
)


def extract_tool_calls(text: str) -> List[Dict]:
    """Extract tool calls from LLM text that contains <invoke> XML blocks."""
    results: List[Dict] = []

    for idx, invoke_match in enumerate(INVOKE_RE.finditer(text)):
        name = invoke_match.group(1)
        body = invoke_match.group(2)

        params: Dict[str, str] = {}
        for param_match in PARAM_RE.finditer(body):
            params[param_match.group(1)] = param_match.group(2).strip()

        results.append({
            "id": f"textcall_{idx}",
            "name": name,
            "input": params,
        })

    return results
