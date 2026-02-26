# rye:signed:2026-02-26T06:42:42Z:8b7c8d285b7389089a0dbcff7050209c68b2e4b4754ea448692bb1eb5311d18a:r7CSuo9yj6opzQsoL8isQEYCzME8u9bzg4JY7s8LayyGrcXeZScPwRQCocU-S3SxG7EIWNiCOKiEZlghrHWjAg==:4b987fd4e40303ac
"""Template interpolation for ${...} and {input:...} expressions.

Shared runtime library — resolves template expressions against
a context dict. Preserves types for whole-expression templates.

Used by: state-graph walker, agent thread hooks, safety harness.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/runtimes/python/lib"
__tool_description__ = "Template interpolation for parameter expressions"

import re
from typing import Any, Dict

from condition_evaluator import resolve_path

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
            value = resolve_path(context, whole.group(1))
            return value

        def _replace(match):
            path = match.group(1)
            value = resolve_path(context, path)
            return str(value) if value is not None else ""

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
