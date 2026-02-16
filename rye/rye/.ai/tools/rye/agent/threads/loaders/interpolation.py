# rye:signed:2026-02-16T05:32:06Z:6db2952faf9d251e11c10b52a976bc4c4932d68c05520a8a28bb59760b23de93:3WQSTEPQFIDA2A_KZPzYa8LLeAR6026VCKHHx8TdlfvruRo48SVTIjxQVr7GCZ70CsbzGBzXcqgbXEoT6g9JDQ==:440443d0858f0199
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Template interpolation for hook actions"

import re
from typing import Any, Dict

from .condition_evaluator import resolve_path

_INTERPOLATION_RE = re.compile(r"\$\{([^}]+)\}")


def interpolate(template: Any, context: Dict) -> Any:
    """Interpolate ${...} expressions in a value.

    Works on strings, dicts (recursive), and lists (recursive).
    Non-string leaves are returned as-is.
    """
    if isinstance(template, str):

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
