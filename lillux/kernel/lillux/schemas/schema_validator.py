"""Schema validation for tool parameters (Phase 5.1).

Validates parameters against JSON Schema specifications.
"""

import re
from dataclasses import dataclass
from typing import Any, Dict, List, Optional, Union


@dataclass
class ValidationResult:
    """Result of parameter validation.
    
    Attributes:
        valid: True if parameters valid according to schema.
        errors: List of error messages if validation failed.
    """

    valid: bool
    errors: List[str]


def validate_parameters(
    parameters: Any,
    schema: Dict[str, Any],
) -> ValidationResult:
    """Validate parameters against JSON Schema.
    
    Supports:
    - type: string, number, integer, boolean, array, object, null
    - required: list of required field names
    - properties: nested object schemas
    - enum: allowed values
    - minimum/maximum: numeric bounds
    - minItems/maxItems: array length bounds
    - pattern: regex for strings
    - items: array item schema
    - default: default values
    
    Args:
        parameters: Parameters to validate.
        schema: JSON Schema dict.
    
    Returns:
        ValidationResult with valid flag and error list.
    """
    errors: List[str] = []

    # Empty schema accepts anything
    if not schema:
        return ValidationResult(valid=True, errors=[])

    # Validate against schema
    _validate_value(parameters, schema, errors, path="")

    return ValidationResult(valid=len(errors) == 0, errors=errors)


def _validate_value(
    value: Any,
    schema: Dict[str, Any],
    errors: List[str],
    path: str = "",
) -> None:
    """Validate a value against schema recursively.
    
    Args:
        value: Value to validate.
        schema: Schema dict.
        errors: List to accumulate errors.
        path: JSON path for error messages.
    """
    # Check type
    schema_type = schema.get("type")
    if schema_type:
        _validate_type(value, schema_type, errors, path)

    # Check enum
    enum_values = schema.get("enum")
    if enum_values is not None:
        if value not in enum_values:
            errors.append(f"{path}: must be one of {enum_values}, got {value!r}")

    # For objects, check properties and required
    if isinstance(value, dict):
        _validate_object(value, schema, errors, path)

    # For arrays, check items and bounds
    if isinstance(value, list):
        _validate_array(value, schema, errors, path)

    # For strings, check pattern
    if isinstance(value, str):
        _validate_string(value, schema, errors, path)

    # For numbers, check bounds
    if isinstance(value, (int, float)) and not isinstance(value, bool):
        _validate_number(value, schema, errors, path)


def _validate_type(
    value: Any,
    schema_type: Union[str, List[str]],
    errors: List[str],
    path: str,
) -> None:
    """Validate value type.
    
    Args:
        value: Value to check.
        schema_type: Expected type(s).
        errors: Error list.
        path: JSON path.
    """
    # Handle multiple types
    if isinstance(schema_type, list):
        types = schema_type
    else:
        types = [schema_type]

    # Check if value matches any type
    for t in types:
        if _matches_type(value, t):
            return

    # Type mismatch
    type_names = " or ".join(types)
    actual_type = type(value).__name__
    errors.append(f"{path}: expected {type_names}, got {actual_type}")


def _matches_type(value: Any, schema_type: str) -> bool:
    """Check if value matches schema type.
    
    Args:
        value: Value to check.
        schema_type: Schema type string.
    
    Returns:
        True if value matches type.
    """
    if schema_type == "null":
        return value is None
    elif schema_type == "boolean":
        return isinstance(value, bool)
    elif schema_type == "integer":
        return isinstance(value, int) and not isinstance(value, bool)
    elif schema_type == "number":
        return isinstance(value, (int, float)) and not isinstance(value, bool)
    elif schema_type == "string":
        return isinstance(value, str)
    elif schema_type == "array":
        return isinstance(value, list)
    elif schema_type == "object":
        return isinstance(value, dict)
    else:
        return True


def _validate_object(
    value: Dict[str, Any],
    schema: Dict[str, Any],
    errors: List[str],
    path: str,
) -> None:
    """Validate object properties.
    
    Args:
        value: Object to validate.
        schema: Object schema.
        errors: Error list.
        path: JSON path.
    """
    properties = schema.get("properties", {})
    required = schema.get("required", [])

    # Check required fields
    for field in required:
        if field not in value:
            field_path = f"{path}.{field}" if path else field
            errors.append(f"{field_path}: required field missing")

    # Validate each property
    for prop_name, prop_schema in properties.items():
        if prop_name in value:
            prop_value = value[prop_name]
            prop_path = f"{path}.{prop_name}" if path else prop_name
            _validate_value(prop_value, prop_schema, errors, prop_path)
        elif "default" in prop_schema:
            # Default value exists but not applied here
            # (defaults applied by caller if needed)
            pass


def _validate_array(
    value: List[Any],
    schema: Dict[str, Any],
    errors: List[str],
    path: str,
) -> None:
    """Validate array items and bounds.
    
    Args:
        value: Array to validate.
        schema: Array schema.
        errors: Error list.
        path: JSON path.
    """
    # Check minItems
    min_items = schema.get("minItems")
    if min_items is not None and len(value) < min_items:
        errors.append(
            f"{path}: must have at least {min_items} items, got {len(value)}"
        )

    # Check maxItems
    max_items = schema.get("maxItems")
    if max_items is not None and len(value) > max_items:
        errors.append(
            f"{path}: must have at most {max_items} items, got {len(value)}"
        )

    # Validate items
    items_schema = schema.get("items")
    if items_schema:
        for i, item in enumerate(value):
            item_path = f"{path}[{i}]"
            _validate_value(item, items_schema, errors, item_path)


def _validate_string(
    value: str,
    schema: Dict[str, Any],
    errors: List[str],
    path: str,
) -> None:
    """Validate string constraints.
    
    Args:
        value: String to validate.
        schema: String schema.
        errors: Error list.
        path: JSON path.
    """
    # Check pattern
    pattern = schema.get("pattern")
    if pattern:
        try:
            if not re.match(pattern, value):
                errors.append(f"{path}: does not match pattern {pattern}")
        except re.error:
            errors.append(f"{path}: invalid regex pattern {pattern}")


def _validate_number(
    value: Union[int, float],
    schema: Dict[str, Any],
    errors: List[str],
    path: str,
) -> None:
    """Validate numeric constraints.
    
    Args:
        value: Number to validate.
        schema: Number schema.
        errors: Error list.
        path: JSON path.
    """
    # Check minimum
    minimum = schema.get("minimum")
    if minimum is not None and value < minimum:
        errors.append(f"{path}: must be >= {minimum}, got {value}")

    # Check maximum
    maximum = schema.get("maximum")
    if maximum is not None and value > maximum:
        errors.append(f"{path}: must be <= {maximum}, got {value}")
