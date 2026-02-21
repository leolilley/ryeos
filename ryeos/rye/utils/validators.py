"""
Schema-Driven Validation Pipeline

Loads VALIDATION_SCHEMA from extractors and validates parsed data against it.
Follows RYE's data-driven architecture pattern.
"""

import ast
import logging
import re
import threading
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.utils.path_utils import get_extractor_search_paths

logger = logging.getLogger(__name__)

# Compiled patterns
SEMVER_PATTERN = re.compile(r"^\d+\.\d+\.\d+$")
SNAKE_CASE_PATTERN = re.compile(r"^[a-z][a-z0-9_]*$")

# Thread-safe global caches with locks
_validation_lock = threading.RLock()
_extraction_lock = threading.RLock()

# Global cache: item_type -> validation schema
_validation_schemas: Optional[Dict[str, Dict[str, Any]]] = None
# Global cache: item_type -> extraction rules
_extraction_rules: Optional[Dict[str, Dict[str, Any]]] = None


def _load_validation_schemas(
    project_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Load validation schemas from all extractors."""
    schemas = {}
    search_paths = get_extractor_search_paths(project_path)

    for extractors_dir in search_paths:
        if not extractors_dir.exists():
            continue

        for file_path in list(extractors_dir.rglob("*_extractor.yaml")) + list(
            extractors_dir.rglob("*_extractor.py")
        ):
            if file_path.name.startswith("_"):
                continue

            # Extract item type from filename (e.g., directive_extractor.yaml -> directive)
            item_type = file_path.stem.replace("_extractor", "")

            # Only set if not already set (precedence: project > user > system)
            if item_type in schemas:
                continue

            schema = _extract_schema_from_file(file_path)
            if schema:
                schemas[item_type] = schema

    logger.debug(f"Loaded validation schemas for: {list(schemas.keys())}")
    return schemas


def _extract_schema_from_file(file_path: Path) -> Optional[Dict[str, Any]]:
    """Extract VALIDATION_SCHEMA from an extractor file."""
    if file_path.suffix in (".yaml", ".yml"):
        import yaml

        try:
            data = yaml.safe_load(file_path.read_text())
            return data.get("validation_schema") if data else None
        except Exception as e:
            logger.warning(f"Failed to load YAML schema from {file_path}: {e}")
            return None

    content = file_path.read_text()

    # Try AST parsing first
    try:
        tree = ast.parse(content)
        for node in tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1:
                target = node.targets[0]
                if isinstance(target, ast.Name) and target.id == "VALIDATION_SCHEMA":
                    if isinstance(node.value, ast.Dict):
                        return ast.literal_eval(node.value)
        return None
    except SyntaxError as e:
        logger.warning(f"Syntax error in {file_path}, using regex fallback: {e}")
        return _extract_schema_regex(content)
    except Exception as e:
        logger.warning(f"Failed to extract schema from {file_path}: {e}")
        return _extract_schema_regex(content)


def _extract_schema_regex(content: str) -> Optional[Dict[str, Any]]:
    """Fallback regex-based schema extraction for malformed files."""
    # Look for VALIDATION_SCHEMA = {...}
    # Match simple dict patterns with optional nesting
    match = re.search(r"VALIDATION_SCHEMA\s*=\s*\{", content)
    if match:
        try:
            # Extract from the match position to end of content
            start = match.end() - 1  # Include opening brace
            # Try to find matching closing brace with basic nesting
            brace_count = 0
            for i, char in enumerate(content[start:]):
                if char == "{":
                    brace_count += 1
                elif char == "}":
                    brace_count -= 1
                    if brace_count == 0:
                        schema_str = content[start : start + i + 1]
                        return ast.literal_eval(schema_str)
        except Exception:
            pass
    return None


def get_validation_schema(
    item_type: str, project_path: Optional[Path] = None
) -> Optional[Dict[str, Any]]:
    """Get validation schema for an item type (thread-safe)."""
    global _validation_schemas

    with _validation_lock:
        if _validation_schemas is None:
            _validation_schemas = _load_validation_schemas(project_path)
        return _validation_schemas.get(item_type)


def clear_validation_schemas_cache():
    """Clear the validation schemas cache (thread-safe)."""
    global _validation_schemas, _extraction_rules
    with _validation_lock:
        _validation_schemas = None
    with _extraction_lock:
        _extraction_rules = None


def _load_extraction_rules(
    project_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Load extraction rules from all extractors."""
    rules = {}
    search_paths = get_extractor_search_paths(project_path)

    for extractors_dir in search_paths:
        if not extractors_dir.exists():
            continue

        for file_path in list(extractors_dir.rglob("*_extractor.yaml")) + list(
            extractors_dir.rglob("*_extractor.py")
        ):
            if file_path.name.startswith("_"):
                continue

            # Extract item type from filename (e.g., directive_extractor.yaml -> directive)
            item_type = file_path.stem.replace("_extractor", "")

            # Only set if not already set (precedence: project > user > system)
            if item_type in rules:
                continue

            extraction_rules = _extract_rules_from_file(file_path)
            if extraction_rules:
                rules[item_type] = extraction_rules

    logger.debug(f"Loaded extraction rules for: {list(rules.keys())}")
    return rules


def _extract_rules_from_file(file_path: Path) -> Optional[Dict[str, Any]]:
    """Extract EXTRACTION_RULES from an extractor file."""
    if file_path.suffix in (".yaml", ".yml"):
        import yaml

        try:
            data = yaml.safe_load(file_path.read_text())
            return data.get("extraction_rules") if data else None
        except Exception as e:
            logger.warning(f"Failed to load YAML rules from {file_path}: {e}")
            return None

    content = file_path.read_text()

    # Try AST parsing first
    try:
        tree = ast.parse(content)
        for node in tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1:
                target = node.targets[0]
                if isinstance(target, ast.Name) and target.id == "EXTRACTION_RULES":
                    if isinstance(node.value, ast.Dict):
                        return ast.literal_eval(node.value)
        return None
    except SyntaxError as e:
        logger.warning(f"Syntax error in {file_path}, using regex fallback: {e}")
        return _extract_rules_regex(content)
    except Exception as e:
        logger.warning(f"Failed to extract rules from {file_path}: {e}")
        return _extract_rules_regex(content)


def _extract_rules_regex(content: str) -> Optional[Dict[str, Any]]:
    """Fallback regex-based rules extraction for malformed files."""
    # Look for EXTRACTION_RULES = {...}
    match = re.search(r"EXTRACTION_RULES\s*=\s*\{", content)
    if match:
        try:
            # Extract from the match position to end of content
            start = match.end() - 1  # Include opening brace
            # Try to find matching closing brace with basic nesting
            brace_count = 0
            for i, char in enumerate(content[start:]):
                if char == "{":
                    brace_count += 1
                elif char == "}":
                    brace_count -= 1
                    if brace_count == 0:
                        rules_str = content[start : start + i + 1]
                        return ast.literal_eval(rules_str)
        except Exception:
            pass
    return None


def get_extraction_rules(
    item_type: str, project_path: Optional[Path] = None
) -> Optional[Dict[str, Any]]:
    """Get extraction rules for an item type (thread-safe)."""
    global _extraction_rules

    with _extraction_lock:
        if _extraction_rules is None:
            _extraction_rules = _load_extraction_rules(project_path)
        return _extraction_rules.get(item_type)


def apply_field_mapping(
    item_type: str,
    parsed_data: Dict[str, Any],
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """
    Apply extraction rules to map parsed fields to standard field names.

    Uses EXTRACTION_RULES from the item type's extractor to map
    e.g., __version__ -> version, __tool_type__ -> tool_type.

    Args:
        item_type: "directive", "tool", or "knowledge"
        parsed_data: Raw parsed data from parser
        project_path: Project root path for loading extractors

    Returns:
        Parsed data with fields mapped to standard names
    """
    rules = get_extraction_rules(item_type, project_path)
    if not rules:
        return parsed_data

    result = dict(parsed_data)

    for standard_name, rule in rules.items():
        if standard_name in result:
            # Already has the standard name
            continue

        rule_type = rule.get("type")
        if rule_type == "path":
            # Map from source key to standard name
            source_key = rule.get("key")
            if source_key and source_key in result:
                result[standard_name] = result[source_key]

    return result


def validate_field(
    field_name: str,
    value: Any,
    field_schema: Dict[str, Any],
    file_path: Optional[Path] = None,
    item_type: Optional[str] = None,
    location: str = "project",
    project_path: Optional[Path] = None,
) -> List[str]:
    """
    Validate a single field against its schema.

    Returns list of validation issues (empty if valid).
    """
    issues = []
    field_type = field_schema.get("type", "string")
    required = field_schema.get("required", False)
    nullable = field_schema.get("nullable", False)

    # For match_path fields, empty string is valid if it matches the path category
    # So we check match_path before rejecting empty strings
    has_match_path = field_schema.get("match_path", False)

    # Check required (but allow None for nullable fields, empty string for match_path)
    if required and value is None and not nullable:
        issues.append(f"Missing required field: {field_name}")
        return issues
    if required and value == [] and not nullable:
        issues.append(f"Missing required field: {field_name}")
        return issues
    if required and value == "" and not has_match_path:
        issues.append(f"Missing required field: {field_name}")
        return issues

    # Skip further validation if value is None (and nullable) or empty and not required
    if value is None:
        return issues
    if value == "" and not has_match_path:
        return issues

    # Type validation
    if field_type == "string":
        if not isinstance(value, str):
            issues.append(
                f"Field '{field_name}' must be a string, got {type(value).__name__}"
            )
            return issues

        # Format validation
        fmt = field_schema.get("format")
        if fmt == "snake_case" and not SNAKE_CASE_PATTERN.match(value):
            issues.append(
                f"Field '{field_name}' must be snake_case "
                f"(lowercase letters, numbers, underscores, starting with letter), got '{value}'"
            )

    elif field_type == "integer":
        if not isinstance(value, int) or isinstance(value, bool):
            issues.append(
                f"Field '{field_name}' must be an integer, got {type(value).__name__}"
            )
        else:
            minimum = field_schema.get("minimum")
            maximum = field_schema.get("maximum")
            if minimum is not None and value < minimum:
                issues.append(f"Field '{field_name}' must be >= {minimum}, got {value}")
            if maximum is not None and value > maximum:
                issues.append(f"Field '{field_name}' must be <= {maximum}, got {value}")

    elif field_type == "number":
        if not isinstance(value, (int, float)) or isinstance(value, bool):
            issues.append(
                f"Field '{field_name}' must be a number, got {type(value).__name__}"
            )
        else:
            minimum = field_schema.get("minimum")
            maximum = field_schema.get("maximum")
            if minimum is not None and value < minimum:
                issues.append(f"Field '{field_name}' must be >= {minimum}, got {value}")
            if maximum is not None and value > maximum:
                issues.append(f"Field '{field_name}' must be <= {maximum}, got {value}")

    elif field_type == "boolean":
        if not isinstance(value, bool):
            issues.append(
                f"Field '{field_name}' must be a boolean, got {type(value).__name__}"
            )

    elif field_type == "semver":
        if not isinstance(value, str):
            issues.append(
                f"Field '{field_name}' must be a string (semver), got {type(value).__name__}"
            )
        elif not SEMVER_PATTERN.match(value):
            issues.append(
                f"Field '{field_name}' must be semver format (X.Y.Z), got '{value}'"
            )

    elif field_type == "enum":
        valid_values = field_schema.get("values", [])
        if value not in valid_values:
            issues.append(
                f"Field '{field_name}' must be one of {valid_values}, got '{value}'"
            )

    elif field_type == "object":
        if not isinstance(value, dict):
            issues.append(
                f"Field '{field_name}' must be an object, got {type(value).__name__}"
            )
        else:
            # Validate nested fields
            nested_schema = field_schema.get("nested", {})
            for nested_name, nested_field_schema in nested_schema.items():
                nested_value = value.get(nested_name)
                nested_issues = validate_field(
                    f"{field_name}.{nested_name}",
                    nested_value,
                    nested_field_schema,
                    file_path,
                    item_type,
                    location,
                    project_path,
                )
                issues.extend(nested_issues)

    elif field_type == "array":
        if not isinstance(value, list):
            issues.append(
                f"Field '{field_name}' must be an array, got {type(value).__name__}"
            )
        else:
            item_type_schema = field_schema.get("item_type")
            item_required = field_schema.get("item_required", [])

            for i, item in enumerate(value):
                if item_type_schema == "object":
                    if not isinstance(item, dict):
                        issues.append(f"Field '{field_name}[{i}]' must be an object")
                    else:
                        for req_field in item_required:
                            if req_field not in item or not item[req_field]:
                                issues.append(
                                    f"Field '{field_name}[{i}]' missing required key '{req_field}'"
                                )

    # Path matching validation
    if field_schema.get("match_filename") and file_path:
        filename = file_path.stem
        if value != filename:
            issues.append(
                f"Field '{field_name}' value '{value}' must match filename '{filename}'"
            )

    if field_schema.get("match_path") and file_path and item_type:
        from rye.utils.path_utils import extract_category_path

        path_category = extract_category_path(
            file_path, item_type, location, project_path
        )
        if value != path_category:
            issues.append(
                f"Field '{field_name}' value '{value}' must match path category '{path_category}'"
            )

    return issues


def validate_parsed_data(
    item_type: str,
    parsed_data: Dict[str, Any],
    file_path: Path,
    location: str = "project",
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """
    Validate parsed data against schema loaded from extractor.

    Args:
        item_type: "directive", "tool", or "knowledge"
        parsed_data: Data from parser
        file_path: Path to the item file
        location: "project", "user", or "system"
        project_path: Project root path

    Returns:
        {"valid": bool, "issues": List[str], "warnings": List[str]}
    """
    issues = []
    warnings = []

    schema = get_validation_schema(item_type, project_path)
    if not schema:
        raise ValueError(
            f"Extractor not found for tool type: {item_type}. Extractors should be packaged with their tools."
        )

    fields_schema = schema.get("fields", {})

    for field_name, field_schema in fields_schema.items():
        value = parsed_data.get(field_name)
        field_issues = validate_field(
            field_name,
            value,
            field_schema,
            file_path,
            item_type,
            location,
            project_path,
        )
        issues.extend(field_issues)

    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "warnings": warnings,
    }
