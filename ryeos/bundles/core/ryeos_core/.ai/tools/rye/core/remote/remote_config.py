"""Named remote resolution for multi-remote execution.

Resolves remote connection details (URL + API key) via 3-tier config
resolution (system → user → project).

Config path: .ai/config/cas/remote.yaml
"""

import logging
import os
from dataclasses import dataclass, field as dataclass_field
from pathlib import Path
from typing import Dict, Optional

logger = logging.getLogger(__name__)

_CONFIG_REL_PATH = "cas/remote.yaml"


@dataclass(frozen=True)
class RemoteConfig:
    """Resolved remote connection details."""

    name: str
    url: str
    api_key: str = dataclass_field(repr=False)


def _load_remote_config(project_path: Optional[Path] = None) -> Dict:
    """Load remote.yaml via 3-tier resolution."""
    from rye.cas.manifest import _load_config_3tier
    return _load_config_3tier(_CONFIG_REL_PATH, project_path)


def resolve_remote(
    name: Optional[str] = None,
    project_path: Optional[Path] = None,
) -> RemoteConfig:
    """Resolve a named remote to connection details.

    Looks up *name* (default: ``"default"``) in the merged ``remotes:``
    map from ``cas/remote.yaml``.

    Raises:
        ValueError: If remote cannot be resolved.
    """
    name = name or "default"
    config = _load_remote_config(project_path)
    remotes = config.get("remotes", {})
    if not isinstance(remotes, dict):
        remotes = {}

    if name not in remotes:
        raise ValueError(
            f"Remote '{name}' not found in cas/remote.yaml. "
            f"Available remotes: {list(remotes.keys()) or ['(none)']}"
        )

    entry = remotes[name]
    if not isinstance(entry, dict):
        raise ValueError(
            f"Remote '{name}' must be a mapping (url + key_env), "
            f"got {type(entry).__name__}"
        )
    url = entry.get("url", "")
    key_env = entry.get("key_env", "")
    if not url:
        raise ValueError(
            f"Remote '{name}' has no url configured in cas/remote.yaml"
        )
    api_key = os.environ.get(key_env, "") if key_env else ""
    if not api_key:
        raise ValueError(
            f"Remote '{name}': env var '{key_env}' is not set. "
            f"Export it via: export {key_env}=your_key"
        )
    return RemoteConfig(name=name, url=url, api_key=api_key)


def get_project_name(project_path: Optional[Path] = None) -> str:
    """Get stable project name from config, falling back to dir basename."""
    config = _load_remote_config(project_path)
    name = config.get("project_name")
    if name and isinstance(name, str):
        return name
    if project_path:
        return project_path.resolve().name
    return "unknown"


def list_remotes(project_path: Optional[Path] = None) -> Dict[str, Dict]:
    """List all configured remotes with their URLs (keys redacted)."""
    config = _load_remote_config(project_path)
    remotes = config.get("remotes", {})
    if not isinstance(remotes, dict):
        remotes = {}
    result = {}
    for rname, entry in remotes.items():
        if not isinstance(entry, dict):
            continue
        key_env = entry.get("key_env", "")
        result[rname] = {
            "url": entry.get("url", ""),
            "key_env": key_env,
            "key_set": bool(os.environ.get(key_env)) if key_env else False,
        }
    return result
