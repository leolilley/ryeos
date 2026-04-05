"""Configuration for ryeos-node server."""

import logging
import os
from functools import lru_cache
from pathlib import Path
from typing import Any, Dict, Optional

import yaml
from pydantic import ConfigDict, model_validator
from pydantic_settings import BaseSettings

logger = logging.getLogger(__name__)


def _load_node_yaml(node_config_dir: str, cas_base_path: str) -> Dict[str, Any]:
    """Load node.yaml from node config space, return flattened settings dict.

    Maps nested node.yaml fields to flat Settings field names.
    Returns empty dict if file doesn't exist or fails to parse.
    """
    ai_dir = os.environ.get("AI_DIR", ".ai")
    config_root = Path(node_config_dir) if node_config_dir else Path(cas_base_path) / "config"
    node_yaml_path = config_root / ai_dir / "config" / "node" / "node.yaml"

    if not node_yaml_path.is_file():
        return {}

    try:
        text = node_yaml_path.read_text(encoding="utf-8")
        # Skip rye:signed header line
        lines = text.splitlines(keepends=True)
        if lines and lines[0].startswith("# rye:signed:"):
            text = "".join(lines[1:])
        data = yaml.safe_load(text)
    except Exception:
        logger.warning("Failed to load node.yaml from %s", node_yaml_path, exc_info=True)
        return {}

    if not isinstance(data, dict):
        return {}

    result: Dict[str, Any] = {}

    identity = data.get("identity")
    if isinstance(identity, dict):
        if "name" in identity:
            result["rye_remote_name"] = identity["name"]
        if "signing_key_dir" in identity:
            result["signing_key_dir"] = identity["signing_key_dir"]

    features = data.get("features")
    if isinstance(features, dict):
        if "registry" in features:
            result["registry_enabled"] = features["registry"]
        if "require_namespaces" in features:
            result["require_namespaces"] = features["require_namespaces"]

    limits = data.get("limits")
    if isinstance(limits, dict):
        if "max_concurrent" in limits:
            result["max_concurrent"] = limits["max_concurrent"]
        if "max_request_bytes" in limits:
            result["max_request_bytes"] = limits["max_request_bytes"]
        if "max_user_storage_bytes" in limits:
            result["max_user_storage_bytes"] = limits["max_user_storage_bytes"]

    gc = data.get("gc")
    if isinstance(gc, dict):
        _gc_map = {
            "retention_days": "gc_retention_days",
            "max_manual_pushes": "gc_max_manual_pushes",
            "max_executions_per_graph": "gc_max_executions",
            "cache_max_age_hours": "gc_cache_max_age_hours",
            "auto_gc_enabled": "gc_auto_enabled",
            "auto_gc_cooldown_seconds": "gc_auto_cooldown",
            "grace_window_seconds": "gc_grace_window",
        }
        for yaml_key, settings_key in _gc_map.items():
            if yaml_key in gc:
                result[settings_key] = gc[yaml_key]

    return result


class Settings(BaseSettings):
    model_config = ConfigDict(env_file=".env", env_file_encoding="utf-8")

    # CAS storage
    cas_base_path: str = "/cas"

    # Remote signing key
    signing_key_dir: str = "/cas/signing"

    # Node config (authorized keys, node identity)
    node_config_dir: str = ""  # defaults to <cas_base_path>/config/

    # Remote identity (server-asserted, set via RYE_REMOTE_NAME env var)
    rye_remote_name: str = "default"

    # Registry
    registry_enabled: bool = False
    require_namespaces: bool = False

    # Concurrency
    max_concurrent: int = 8

    # Server
    host: str = "0.0.0.0"
    port: int = 8000

    # Limits
    max_request_bytes: int = 50 * 1024 * 1024  # 50MB
    max_user_storage_bytes: int = 10 * 1024 * 1024 * 1024  # 10GB

    # GC
    gc_retention_days: int = 7
    gc_max_manual_pushes: int = 3
    gc_max_executions: int = 10
    gc_cache_max_age_hours: int = 24
    gc_auto_enabled: bool = True
    gc_auto_cooldown: int = 600  # seconds between auto-GC runs
    gc_grace_window: int = 3600  # sweep grace period seconds

    @model_validator(mode="before")
    @classmethod
    def _apply_node_yaml_defaults(cls, values: Dict[str, Any]) -> Dict[str, Any]:
        """Load node.yaml values as defaults — env vars override."""
        node_config_dir = values.get("node_config_dir", "")
        cas_base_path = values.get("cas_base_path", "/cas")
        node_defaults = _load_node_yaml(node_config_dir, cas_base_path)

        # node.yaml provides defaults: only set if not already provided
        for key, val in node_defaults.items():
            if key not in values or values[key] is None:
                values[key] = val

        return values

    def _node_config(self) -> Path:
        if self.node_config_dir:
            return Path(self.node_config_dir)
        return Path(self.cas_base_path) / "config"

    def authorized_keys_dir(self) -> Path:
        return self._node_config() / "authorized_keys"

    def node_yaml_path(self) -> Path:
        ai_dir = os.environ.get("AI_DIR", ".ai")
        return self._node_config() / ai_dir / "config" / "node" / "node.yaml"

    def hardware_descriptors(self) -> Dict[str, Any]:
        """Read hardware section from node.yaml. Returns empty dict if unavailable."""
        path = self.node_yaml_path()
        if not path.is_file():
            return {}
        try:
            text = path.read_text(encoding="utf-8")
            lines = text.splitlines(keepends=True)
            if lines and lines[0].startswith("# rye:signed:"):
                text = "".join(lines[1:])
            data = yaml.safe_load(text)
            if isinstance(data, dict) and isinstance(data.get("hardware"), dict):
                return data["hardware"]
        except Exception:
            logger.debug("Failed to read hardware from node.yaml", exc_info=True)
        return {}

    def user_root(self, fingerprint: str) -> Path:
        """Per-user top-level directory (cache, executions, refs, locks)."""
        return Path(self.cas_base_path) / fingerprint

    def user_cas_root(self, fingerprint: str) -> Path:
        ai_dir = os.environ.get("AI_DIR", ".ai")
        return Path(self.cas_base_path) / fingerprint / ai_dir / "objects"

    def cache_root(self, fingerprint: str) -> Path:
        return Path(self.cas_base_path) / fingerprint / "cache"

    def exec_root(self, fingerprint: str) -> Path:
        return Path(self.cas_base_path) / fingerprint / "executions"


@lru_cache
def get_settings() -> Settings:
    return Settings()
