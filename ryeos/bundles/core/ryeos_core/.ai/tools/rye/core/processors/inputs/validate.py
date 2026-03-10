# rye:signed:2026-03-10T04:07:14Z:102fb350100af41557411bc008607293f603daae96ccb0cf095332194d05e41e:cPUq7y5cnnZYFk0uqF_J8ajA4jX-omfULuTMrJIMp6TzIkk8PTSZzBe2Td5303JgTzHUXJK_ZhI4xQdTI_xBDA==:4b987fd4e40303ac
"""Input validation processor.

Validates directive parameters against declared input schemas.
Handles unknown parameter rejection, default application, and
required field enforcement.
"""

__version__ = "1.0.0"
__tool_type__ = "processor"
__category__ = "rye/core/processors/inputs"
__tool_description__ = (
    "Input validation processor - validates parameters against declared inputs"
)

from typing import Any, Dict, List


def process(parsed: Dict[str, Any], parameters: Dict[str, Any]) -> Dict[str, Any]:
    """Validate parameters against parsed input declarations.

    Args:
        parsed: Parsed directive data containing an ``inputs`` list.
        parameters: User-supplied parameters to validate.

    Returns:
        On success::

            {"status": "success", "inputs": {...}, "declared_inputs": [...]}

        On error::

            {"status": "error", "error": "...", "declared_inputs": [...]}
    """
    inputs = dict(parameters)
    declared_inputs: List[Dict] = parsed.get("inputs", [])
    declared_names = {inp["name"] for inp in declared_inputs}

    # Reject unknown parameters
    unknown = [k for k in parameters if k not in declared_names]
    if unknown and declared_inputs:
        return {
            "status": "error",
            "error": (
                f"Unknown parameters: {', '.join(unknown)}. "
                f"Valid inputs: {', '.join(declared_names)}"
            ),
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
            "declared_inputs": declared_inputs,
        }

    return {
        "status": "success",
        "inputs": inputs,
        "declared_inputs": declared_inputs,
    }
