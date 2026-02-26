# rye:signed:2026-02-25T00:02:14Z:bc6933754c264d2664c02ae8f69605bf002073e99e095201cd492c3e442d9e16:Wdz4IYdZBZepeA_vjnTxmrRNAXfQc_6vJ8ugSZQfioYj0SkNVUhbX2CwDO-5SNXJ_zJpQs35_kUECy1Nyy_uCQ==:9fbfabe975fa5a7f
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/agent/threads/loaders"
__tool_description__ = "Template interpolation for hook actions"

import re
from typing import Any, Dict

from .condition_evaluator import resolve_path

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

        result = _INTERPOLATION_RE.sub(_replace, template)
        # Also resolve {input:name} from context["inputs"]
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
