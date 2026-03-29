# rye:signed:2026-03-29T05:38:21Z:3b88c7f5a4f986e8f2fc2982b051789205c459a0a4633eb5d3e25c4761bb5b8d:WIkV6I3OvefXFFg64WeY7GZUQqlx992H0jHGnIs03qJ0LijwI9wUSpJyKHPqo2NhOImYgXBR8IaWQcuQA3ipDQ==:4b987fd4e40303ac
"""Template interpolation for ${...} and {input:...} expressions.

Shared runtime library — resolves template expressions against
a context dict. Preserves types for whole-expression templates.

Used by: state-graph walker, agent thread hooks, safety harness.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/runtimes/python/lib"
__tool_description__ = "Template interpolation for parameter expressions"

import logging
import re
from typing import Any, Dict

from condition_evaluator import resolve_path

logger = logging.getLogger(__name__)

_INTERPOLATION_RE = re.compile(r"\$\{([^}]+)\}")
_INPUT_REF_RE = re.compile(r"\{input:(\w+)(\?|[:|][^}]*)?\}")
_WHOLE_EXPR_RE = re.compile(r"^\$\{([^}]+)\}$")


def _resolve_input_ref(match, inputs: Dict) -> str:
    """Resolve a {input:name} reference from inputs dict."""
    key = match.group(1)
    modifier = match.group(2)
    if key in inputs:
        return str(inputs[key])
    if modifier == "?":
        return ""
    if modifier and modifier[0] in (":", "|"):
        return modifier[1:]
    return match.group(0)


def _resolve_expr(context: Dict, expr: str) -> Any:
    """Resolve an expression, supporting ``||`` fallback chains.

    ``a.b || c.d`` tries each path left-to-right, returning the first
    non-None value.  Plain paths (no ``||``) are resolved directly.
    """
    if "||" not in expr:
        return resolve_path(context, expr.strip())
    for part in expr.split("||"):
        value = resolve_path(context, part.strip())
        if value is not None:
            return value
    return None


def interpolate(template: Any, context: Dict) -> Any:
    """Interpolate ${...} and {input:...} expressions in a value.

    Supports both syntaxes:
      - ${path.to.value} — resolved via resolve_path on context
      - {input:name} — resolved from context["inputs"] dict

    Works on strings, dicts (recursive), and lists (recursive).
    Non-string leaves are returned as-is.

    Type preservation: when a template is exactly "${path}" (single whole
    expression with no surrounding text), the raw resolved value is returned
    without str() conversion.
    """
    if isinstance(template, str):
        whole = _WHOLE_EXPR_RE.match(template)
        if whole:
            expr = whole.group(1)
            value = _resolve_expr(context, expr)
            if value is None:
                logger.warning("interpolation '${%s}' resolved to None", expr)
            return value

        def _replace(match):
            expr = match.group(1)
            value = _resolve_expr(context, expr)
            if value is None:
                logger.warning("interpolation '${%s}' resolved to None", expr)
                return ""
            return str(value)

        result = _INTERPOLATION_RE.sub(_replace, template)
        inputs = context.get("inputs", {})
        if inputs and _INPUT_REF_RE.search(result):
            result = _INPUT_REF_RE.sub(lambda m: _resolve_input_ref(m, inputs), result)
        return result
    if isinstance(template, dict):
        return {k: interpolate(v, context) for k, v in template.items()}
    if isinstance(template, list):
        return [interpolate(item, context) for item in template]
    return template


def interpolate_action(action: Dict, context: Dict) -> Dict:
    """Interpolate all ${...} in an action's interpolable fields.

    Interpolates: item_id, params.
    Preserves: primary, item_type.
    """
    result = dict(action)
    if "item_id" in result:
        result["item_id"] = interpolate(result["item_id"], context)
    if "params" in result:
        result["params"] = interpolate(result["params"], context)
    return result
