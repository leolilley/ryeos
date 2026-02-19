# rye:signed:2026-02-18T09:08:44Z:486eb2cea69b4167223f5c505a2fd972e485f06f721b9bc6bfa778facc804dc8:FYEsg8ojsk6P4S7XCSLzbUJRL6h2KVHcZX0PIkUuUnDBR6kSeAErXRK79wVRKcB-6reDu3tH2zRB9UGZhMa0CA==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Template interpolation for hook actions"

import re
from typing import Any, Dict

from .condition_evaluator import resolve_path

_INTERPOLATION_RE = re.compile(r"\$\{([^}]+)\}")


_WHOLE_EXPR_RE = re.compile(r"^\$\{([^}]+)\}$")


def interpolate(template: Any, context: Dict) -> Any:
    """Interpolate ${...} expressions in a value.

    Works on strings, dicts (recursive), and lists (recursive).
    Non-string leaves are returned as-is.

    Type preservation: when a template is exactly "${path}" (single whole
    expression with no surrounding text), the raw resolved value is returned
    without str() conversion.  Mixed templates like "Count: ${x}" retain
    string behavior.
    """
    if isinstance(template, str):
        # Fast path: single whole expression — preserve the resolved type
        whole = _WHOLE_EXPR_RE.match(template)
        if whole:
            value = resolve_path(context, whole.group(1))
            return value  # None, int, list, dict — whatever it is

        def _replace(match):
            path = match.group(1)
            value = resolve_path(context, path)
            return str(value) if value is not None else ""

        return _INTERPOLATION_RE.sub(_replace, template)
    if isinstance(template, dict):
        return {k: interpolate(v, context) for k, v in template.items()}
    if isinstance(template, list):
        return [interpolate(item, context) for item in template]
    return template


def interpolate_action(action: Dict, context: Dict) -> Dict:
    """Interpolate all ${...} in an action's params.

    Preserves primary/item_type/item_id.
    """
    result = dict(action)
    if "params" in result:
        result["params"] = interpolate(result["params"], context)
    return result
