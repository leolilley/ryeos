"""Config content validation — loads .config-schema.yaml tools and validates config dicts.

Schema tools live under .ai/tools/ and declare which config they validate
via ``target_config`` (a path relative to .ai/config/, e.g. ``cas/remote.yaml``).
Schemas are discovered once via rglob across 3-tier tools roots and cached.

Separate from validators.py which handles extractor-driven METADATA validation.
This module handles config CONTENT schema validation.
"""

import fnmatch
import logging
import threading
from pathlib import Path
from typing import Any, Dict, List, Optional

logger = logging.getLogger(__name__)

_schemas_lock = threading.RLock()
_schemas_cache: Dict[tuple, Dict[str, Dict[str, Any]]] = {}


def _get_tools_roots(
    project_path: Optional[Path] = None,
) -> List[Path]:
    """Get .ai/tools/ roots in precedence order (system → user → project)."""
    from rye.constants import AI_DIR, ItemType
    from rye.utils.path_utils import get_system_spaces, get_user_ai_path

    tools_dir = ItemType.TYPE_DIRS[ItemType.TOOL]
    roots: List[Path] = []

    for bundle in get_system_spaces():
        p = bundle.root_path / AI_DIR / tools_dir
        if p.is_dir():
            roots.append(p)

    user_p = get_user_ai_path() / tools_dir
    if user_p.is_dir():
        roots.append(user_p)

    if project_path:
        proj_p = project_path / AI_DIR / tools_dir
        if proj_p.is_dir():
            roots.append(proj_p)

    return roots


def _load_config_schemas(
    project_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Discover all config schemas across tools roots.

    Scans for ``*.config-schema.yaml`` under each ``.ai/tools/`` root,
    indexes by ``target_config`` field.  Later tiers override earlier
    (project > user > system).
    """
    import yaml

    schemas: Dict[str, Dict[str, Any]] = {}

    for root in _get_tools_roots(project_path):
        for f in sorted(root.rglob("*.config-schema.yaml")):
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


def _get_schemas(
    project_path: Optional[Path] = None,
) -> Dict[str, Dict[str, Any]]:
    """Get cached schema index (thread-safe, keyed by resolved roots)."""
    roots = _get_tools_roots(project_path)
    cache_key = tuple(str(p.resolve()) for p in roots)
    with _schemas_lock:
        if cache_key not in _schemas_cache:
            _schemas_cache[cache_key] = _load_config_schemas(project_path)
        return _schemas_cache[cache_key]


def validate_config_content(
    config_id: str,
    config_data: Dict[str, Any],
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Validate a config dict against its content schema.

    Args:
        config_id: Config item ID (relative path under .ai/config/
            without extension), e.g. ``"agent/agent"``, ``"cas/remote"``.
        config_data: Config dict to validate.
        project_path: Project root for schema discovery.

    Returns:
        {"valid": bool, "issues": [...], "warnings": [...]}
    """
    schemas = _get_schemas(project_path)
    # Build candidate target keys from config_id + known extensions
    from rye.utils.extensions import get_item_extensions
    try:
        extensions = get_item_extensions("config", project_path)
    except ValueError:
        extensions = [".yaml", ".yml"]
    candidate_keys = [f"{config_id}{ext}" for ext in extensions]

    # Try exact match first
    schema = None
    for key in candidate_keys:
        schema = schemas.get(key)
        if schema is not None:
            break
    # Fall back to glob patterns in target_config keys (e.g. "keys/trusted/*.toml")
    if schema is None:
        for key in candidate_keys:
            for pattern, s in schemas.items():
                if "*" in pattern or "?" in pattern:
                    if fnmatch.fnmatch(key, pattern):
                        schema = s
                        break
            if schema is not None:
                break
    if schema is None:
        return {"valid": True, "issues": [], "warnings": []}

    issues = _validate_object(config_data, schema, path=config_id)
    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "warnings": [],
    }


def clear_config_schemas_cache() -> None:
    """Clear the schema cache (for testing)."""
    global _schemas_cache
    with _schemas_lock:
        _schemas_cache = {}


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
