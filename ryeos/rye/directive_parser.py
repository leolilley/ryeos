"""Standalone directive parsing: validate inputs, interpolate placeholders.

Extracted from ExecuteTool._run_directive() so that both execute.py and
thread_directive.py can reuse the same logic without circular imports.
"""

import os
import re
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import ItemType
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.parser_router import ParserRouter

# {input:key}          — required, kept as-is if missing
# {input:key?}         — optional, empty string if missing
# {input:key:default}  — fallback to default if missing (colon separator)
# {input:key|default}  — fallback to default if missing (pipe separator)
_INPUT_REF = re.compile(r"\{input:(\w+)(\?|[:|][^}]*)?\}")
_DOLLAR_INPUT_RE = re.compile(r"\$\{inputs\.(\w+)\}")

# {env:VAR}            — required, kept as-is if missing
# {env:VAR:default}    — fallback to default if env var not set
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
    # Also resolve ${inputs.name} syntax
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


def parse_and_validate_directive(
    *,
    file_path: Path,
    item_id: str,
    parameters: Dict[str, Any],
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Parse a directive file, validate inputs, and interpolate placeholders.

    Returns a dict with ``status`` set to ``"success"`` or ``"error"``.

    On success the dict also contains:
        parsed          – the parsed directive data (with placeholders resolved)
        inputs          – the final validated input values (params + defaults)
        declared_inputs – the raw input declarations from the directive

    On error the dict contains an ``error`` message and, where applicable,
    ``item_id`` and ``declared_inputs`` for caller diagnostics.
    """
    # 1. Integrity check
    try:
        verify_item(file_path, ItemType.DIRECTIVE, project_path=project_path)
    except IntegrityError as exc:
        return {"status": "error", "error": str(exc), "item_id": item_id}

    # 2. Read and parse
    content = file_path.read_text(encoding="utf-8")
    parsed = ParserRouter().parse("markdown/xml", content)

    if "error" in parsed:
        return {"status": "error", "error": parsed.get("error"), "item_id": item_id}

    # 3. Input validation
    inputs = dict(parameters)
    declared_inputs: List[Dict] = parsed.get("inputs", [])
    declared_names = {inp["name"] for inp in declared_inputs}

    # Reject unknown parameters early so the caller can correct
    unknown = [k for k in parameters if k not in declared_names]
    if unknown and declared_inputs:
        return {
            "status": "error",
            "error": f"Unknown parameters: {', '.join(unknown)}. "
                     f"Valid inputs: {', '.join(declared_names)}",
            "item_id": item_id,
            "declared_inputs": declared_inputs,
        }

    # Apply defaults
    for inp in declared_inputs:
        if inp["name"] not in inputs and "default" in inp:
            inputs[inp["name"]] = inp["default"]

    # Check required inputs
    missing = [
        inp["name"]
        for inp in declared_inputs
        if inp.get("required") and inp["name"] not in inputs
    ]
    if missing:
        return {
            "status": "error",
            "error": f"Missing required inputs: {', '.join(missing)}",
            "item_id": item_id,
            "declared_inputs": declared_inputs,
        }

    # 4. Interpolate placeholders
    _interpolate_parsed(parsed, inputs)

    return {
        "status": "success",
        "parsed": parsed,
        "inputs": inputs,
        "declared_inputs": declared_inputs,
    }
