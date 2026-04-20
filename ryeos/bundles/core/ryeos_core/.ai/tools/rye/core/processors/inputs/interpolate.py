# rye:signed:2026-04-20T05:37:46Z:7bf5a2f4543968bdd091b04861016b1e8a72fedc687b6a245926079a2dcd642f:DiSBeDPUzksXcgJ6L94wJNg9ojhhFBMfJ1RjjjP5lQXTcqlFojvWRd52248TlTWI80eWEd-QCcwjJO3xSMDiBA:4b987fd4e40303ac
"""Input interpolation processor.

Resolves placeholder references in parsed directive content:
- ``{input:key}``           — required, kept as-is if missing
- ``{input:key?}``          — optional, empty string if missing
- ``{input:key:default}``   — fallback to default (colon separator)
- ``{input:key|default}``   — fallback to default (pipe separator)
- ``${inputs.name}``        — alternate syntax
- ``{env:VAR}``             — environment variable, kept as-is if missing
- ``{env:VAR:default}``     — environment variable with fallback
"""

__version__ = "1.0.0"
__tool_type__ = "processor"
__category__ = "rye/core/processors/inputs"
__tool_description__ = (
    "Input interpolation processor - resolves placeholder references"
)

import os
import re
from typing import Any, Dict

_INPUT_REF = re.compile(r"\{input:(\w+)(\?|[:|][^}]*)?\}")
_DOLLAR_INPUT_RE = re.compile(r"\$\{inputs\.(\w+)\}")
_ENV_REF = re.compile(r"\{env:(\w+)(?::([^}]*))?\}")


def _resolve_env_refs(value: str) -> str:
    """Resolve {env:VAR} and {env:VAR:default} placeholders from os.environ."""

    def _replace(m: re.Match) -> str:
        var = m.group(1)
        default = m.group(2)
        env_val = os.environ.get(var)
        if env_val is not None:
            return env_val
        if default is not None:
            return default
        return m.group(0)

    return _ENV_REF.sub(_replace, value)


def _resolve_input_refs(value: str, inputs: Dict[str, Any]) -> str:
    """Resolve {input:name} and ${inputs.name} placeholders in a string."""

    def _replace(m: re.Match) -> str:
        key = m.group(1)
        modifier = m.group(2)
        if key in inputs:
            return str(inputs[key])
        if modifier == "?":
            return ""
        if modifier and modifier[0] in (":", "|"):
            return modifier[1:]
        return m.group(0)

    result = _INPUT_REF.sub(_replace, value)
    if "${inputs." in result:
        result = _DOLLAR_INPUT_RE.sub(
            lambda m: str(inputs[m.group(1)]) if m.group(1) in inputs else m.group(0),
            result,
        )
    return result


def _interpolate_parsed(parsed: Dict[str, Any], inputs: Dict[str, Any]) -> None:
    """Interpolate {input:name} and {env:VAR} refs in body, actions, and content fields."""
    for key in ("body", "content", "raw"):
        if isinstance(parsed.get(key), str):
            parsed[key] = _resolve_env_refs(parsed[key])
            parsed[key] = _resolve_input_refs(parsed[key], inputs)

    for action in parsed.get("actions", []):
        for k, v in list(action.items()):
            if isinstance(v, str):
                action[k] = _resolve_input_refs(v, inputs)
        for pk, pv in list(action.get("params", {}).items()):
            if isinstance(pv, str):
                action["params"][pk] = _resolve_input_refs(pv, inputs)


def process(parsed: Dict[str, Any], inputs: Dict[str, Any]) -> Dict[str, Any]:
    """Interpolate all placeholder references in parsed directive data.

    Args:
        parsed: Parsed directive data (mutated in-place).
        inputs: Validated input values to substitute.

    Returns:
        The mutated ``parsed`` dict.
    """
    _interpolate_parsed(parsed, inputs)
    return parsed
