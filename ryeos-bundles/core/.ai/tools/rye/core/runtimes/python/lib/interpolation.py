# rye:signed:2026-05-01T05:22:27Z:899fac5618145541b6ddf0130e1d6e31cc380c8edba8f5824b6d23306ba805a5:sHRZMlb6NFvThAy1awjNcCeTnCyu9S5Q3U1aQw9DgIf6n+ITdvNwFU0gSlobc9+ldgcD9clsKGjCSlywJwWzAQ==:09674c8998e9dd01bfc40ec9f8c4b6b2c1bd01333842582a9c34b3c7db5aa86c
"""Template interpolation for ${...} and {input:...} expressions.

Shared runtime library — resolves template expressions against
a context dict. Preserves types for whole-expression templates.

Used by: state-graph walker, agent thread hooks, safety harness.
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/runtimes/python/lib"
__description__ = "Template interpolation for parameter expressions"

import json as _json
import logging
import re
from typing import Any, Dict

from condition_evaluator import resolve_path

logger = logging.getLogger(__name__)

_INTERPOLATION_RE = re.compile(r"\$\{([^}]+)\}")
_INPUT_REF_RE = re.compile(r"\{input:(\w+)(\?|[:|][^}]*)?\}")
_WHOLE_EXPR_RE = re.compile(r"^\$\{([^}]+)\}$")

_PIPES: Dict[str, Any] = {
    "json": lambda v: _json.dumps(v, default=str),
    "from_json": lambda v: _json.loads(v) if isinstance(v, str) else v,
    "length": lambda v: len(v) if v is not None else 0,
    "keys": lambda v: list(v.keys()) if isinstance(v, dict) else [],
    "upper": lambda v: str(v).upper(),
    "lower": lambda v: str(v).lower(),
}


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


def _apply_pipes(value: Any, pipe_str: str) -> Any:
    """Apply ``| pipe`` filters to a resolved value."""
    for name in pipe_str.split("|"):
        name = name.strip()
        if not name:
            continue
        fn = _PIPES.get(name)
        if fn is None:
            logger.warning("unknown interpolation pipe '%s'", name)
            continue
        value = fn(value)
    return value


def _resolve_expr(context: Dict, expr: str) -> Any:
    """Resolve an expression, supporting ``||`` fallback chains and ``| pipe`` filters.

    ``a.b || c.d`` tries each path left-to-right, returning the first
    non-None value.  Plain paths (no ``||``) are resolved directly.

    ``a.b | json`` resolves ``a.b`` then applies the ``json`` pipe.
    Pipes bind tighter than fallbacks: ``a || b | json`` →
    resolve(a || b) then pipe(json).
    """
    # Split trailing pipes: "state.x | json" → path_expr="state.x", pipes="json"
    # Distinguish from fallback "||" by splitting on single "|" not preceded/followed by "|"
    pipe_parts = re.split(r"(?<!\|)\|(?!\|)", expr, maxsplit=1)
    path_expr = pipe_parts[0].strip()
    pipes = pipe_parts[1] if len(pipe_parts) > 1 else None

    if "||" not in path_expr:
        value = resolve_path(context, path_expr)
    else:
        value = None
        for part in path_expr.split("||"):
            value = resolve_path(context, part.strip())
            if value is not None:
                break

    if pipes and value is not None:
        value = _apply_pipes(value, pipes)
    return value


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
    Preserves: primary, kind (action dict key).
    """
    result = dict(action)
    if "item_id" in result:
        result["item_id"] = interpolate(result["item_id"], context)
    if "params" in result:
        result["params"] = interpolate(result["params"], context)
    return result
