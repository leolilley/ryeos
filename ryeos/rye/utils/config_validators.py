"""Config content validation — loads .config-schema.yaml tools and validates config dicts.

Discovers .config-schema.yaml files via 3-tier resolution, matches them
to config files by `target_config`, and recursively validates content
structure against the schema.

Separate from validators.py which handles extractor-driven METADATA validation.
This module handles config CONTENT schema validation.

Schema tools live at:
    .ai/tools/rye/agent/config-schemas/{name}.config-schema.yaml
"""

import logging
import threading
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

_schemas_lock = threading.RLock()
_schemas_cache: Optional[Dict[str, Dict[str, Any]]] = None


def _get_config_schema_search_paths(
    project_path: Optional[Path] = None,
) -> List[Path]:
    """Get search paths for .config-schema.yaml tools in precedence order.

    Returns paths in order: system → user → project (last wins for overrides).
    """
    from rye.constants import AI_DIR
    from rye.utils.path_utils import get_system_spaces, get_user_ai_path

    schema_subdir = Path("tools") / "rye" / "agent" / "config-schemas"
    paths: List[Path] = []

    # System (all bundles, lowest priority)
    for bundle in get_system_spaces():
        p = bundle.root_path / AI_DIR / schema_subdir
        if p.is_dir():
            paths.append(p)

    # User
    user_p = get_user_ai_path() / schema_subdir
    if user_p.is_dir():
        paths.append(user_p)

    # Project (highest priority)
    if project_path:
        proj_p = project_path / AI_DIR / schema_subdir
        if proj_p.is_dir():
            paths.append(proj_p)

    return paths


def _load_config_schemas(
    project_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Load all .config-schema.yaml files via 3-tier resolution.

    Returns dict keyed by target_config filename (e.g., "agent.yaml").
    Later entries override earlier ones (project > user > system).
    """
    import yaml

    schemas: Dict[str, Dict[str, Any]] = {}

    for d in _get_config_schema_search_paths(project_path):
        for f in sorted(d.glob("*.config-schema.yaml")):
            try:
                data = yaml.safe_load(f.read_text(encoding="utf-8"))
                if not isinstance(data, dict):
                    continue
                target = data.get("target_config")
                schema = data.get("schema")
                if target and isinstance(schema, dict):
                    schemas[target] = schema
            except Exception:
                logger.debug("Failed to load config schema %s", f, exc_info=True)

    return schemas


def get_config_content_schema(
    config_name: str,
    project_path: Optional[Path] = None,
) -> Optional[Dict[str, Any]]:
    """Get the content schema for a config file (thread-safe).

    Args:
        config_name: Config filename, e.g., "agent.yaml"
        project_path: Project root for 3-tier resolution

    Returns:
        Schema dict or None if no schema defined for this config.
    """
    global _schemas_cache

    with _schemas_lock:
        if _schemas_cache is None:
            _schemas_cache = _load_config_schemas(project_path)
        return _schemas_cache.get(config_name)


def validate_config_content(
    config_name: str,
    config_data: Dict[str, Any],
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Validate a config dict against its content schema.

    Args:
        config_name: Config filename (e.g., "agent.yaml")
        config_data: Config dict (merged or single-source)
        project_path: Project root for schema discovery

    Returns:
        {"valid": bool, "issues": [...], "warnings": [...]}
    """
    schema = get_config_content_schema(config_name, project_path)
    if schema is None:
        return {"valid": True, "issues": [], "warnings": []}

    issues = _validate_object(config_data, schema, path=config_name)
    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "warnings": [],
    }


def clear_config_schemas_cache() -> None:
    """Clear the schema cache (for testing)."""
    global _schemas_cache
    with _schemas_lock:
        _schemas_cache = None


# ---------------------------------------------------------------------------
# Recursive schema validation
# ---------------------------------------------------------------------------


def _validate_type(value: Any, expected_type: str) -> bool:
    """Check if value matches expected type."""
    type_map = {
        "object": dict,
        "string": str,
        "integer": int,
        "number": (int, float),
        "boolean": bool,
        "array": list,
    }
    # null is always acceptable for optional fields
    if value is None:
        return True
    expected = type_map.get(expected_type)
    if expected is None:
        return True
    return isinstance(value, expected)


def _validate_object(
    obj: Any, schema: Dict[str, Any], path: str = "",
) -> List[str]:
    """Recursively validate an object against a schema."""
    errors: List[str] = []

    if obj is None:
        return errors

    expected_type = schema.get("type", "object")
    if not _validate_type(obj, expected_type):
        errors.append(
            f"{path}: expected {expected_type}, got {type(obj).__name__}"
        )
        return errors

    if expected_type != "object" or not isinstance(obj, dict):
        return errors

    properties = schema.get("properties", {})
    for key, field_schema in properties.items():
        if key in obj:
            field_path = f"{path}.{key}" if path else key
            field_type = field_schema.get("type", "object")
            value = obj[key]

            if not _validate_type(value, field_type):
                errors.append(
                    f"{field_path}: expected {field_type}, got {type(value).__name__}"
                )
            elif field_type == "object" and isinstance(value, dict) and "properties" in field_schema:
                errors.extend(_validate_object(value, field_schema, field_path))

    return errors
