"""Config snapshot — hash resolved config for cache keys.

Computes a deterministic hash of resolved agent configs after 3-tier merge.
Used as part of node execution cache keys to invalidate on config changes.
"""

from __future__ import annotations

import hashlib
import json
import logging
from pathlib import Path
from typing import Any, Dict, Tuple

import yaml

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_ai_path, get_system_spaces

logger = logging.getLogger(__name__)


def compute_config_hash(resolved_configs: Dict[str, Any]) -> str:
    """Compute a deterministic hash of resolved config state.

    Args:
        resolved_configs: Dict mapping config names to their merged values.
            e.g., {"agent.yaml": {...}, "resilience.yaml": {...}, ...}

    Returns:
        SHA256 hex digest of canonical JSON serialization.
    """
    canonical = json.dumps(resolved_configs, sort_keys=True, separators=(",", ":"), default=str)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def compute_agent_config_snapshot(project_path) -> Tuple[str, Dict[str, Any]]:
    """Compute a unified config snapshot hash for all agent configs.

    Loads agent.yaml, resilience.yaml, coordination.yaml, and hooks.yaml
    via 3-tier merge (system → user → project), combines them, and hashes.

    Args:
        project_path: Path to the project root.

    Returns:
        (snapshot_hash, resolved_configs_dict)
    """
    config_names = ["agent.yaml", "resilience.yaml", "coordination.yaml", "hooks.yaml"]
    resolved: Dict[str, Any] = {}

    for name in config_names:
        config = _load_config_by_name(name, Path(project_path))
        if config:
            resolved[name] = config

    return compute_config_hash(resolved), resolved


def _load_config_by_name(config_name: str, project_path: Path) -> Dict[str, Any]:
    """Load a single config file via 3-tier resolution.

    Self-contained loader that mirrors ConfigLoader.load() merge order
    without depending on the bundle module.
    """
    config: Dict[str, Any] = {}

    # System (all bundles)
    for bundle in get_system_spaces():
        system_path = bundle.root_path / AI_DIR / "config" / "agent" / config_name
        if system_path.exists():
            try:
                with open(system_path) as f:
                    layer = yaml.safe_load(f) or {}
                config = _deep_merge(config, layer)
            except Exception:
                logger.debug("Failed to load config %s from %s", config_name, system_path, exc_info=True)

    # User
    user_path = get_user_ai_path() / "config" / "agent" / config_name
    if user_path.exists():
        try:
            with open(user_path) as f:
                layer = yaml.safe_load(f) or {}
            config = _deep_merge(config, layer)
        except Exception:
            logger.debug("Failed to load config %s from %s", config_name, user_path, exc_info=True)

    # Project
    project_config_path = project_path / AI_DIR / "config" / "agent" / config_name
    if project_config_path.exists():
        try:
            with open(project_config_path) as f:
                layer = yaml.safe_load(f) or {}
            config = _deep_merge(config, layer)
        except Exception:
            logger.debug("Failed to load config %s from %s", config_name, project_config_path, exc_info=True)

    return config


def _deep_merge(base: Dict, override: Dict) -> Dict:
    """Deep merge override into base."""
    result = dict(base)
    for key, value in override.items():
        if key in result and isinstance(result[key], dict) and isinstance(value, dict):
            result[key] = _deep_merge(result[key], value)
        else:
            result[key] = value
    return result
