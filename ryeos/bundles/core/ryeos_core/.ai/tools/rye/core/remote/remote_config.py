# rye:signed:2026-04-07T02:45:54Z:efbbeba928b401b081582cde1178db47cd95f5cf46d07ff3514438b676826467:3iFPDTmBvN2uQ9oEsGQpXd-o7UVhdIzd0Ybjtt81wI5aA7RNVCPXFdtTBA67oGGZ7xFvJ-1krHwMVHIPwlr7Bw:4b987fd4e40303ac
"""Named remote resolution for multi-remote execution.

Resolves remote connection details (URL + API key) via 3-tier config
resolution (system → user → project).

Config path: .ai/config/remotes/remotes.yaml
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/remote"
__tool_description__ = "Named remote config resolution library"

import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Dict, Optional

logger = logging.getLogger(__name__)

_CONFIG_REL_PATH = "remotes/remotes.yaml"


@dataclass(frozen=True)
class RemoteConfig:
    """Resolved remote connection details."""

    name: str
    url: str
    timeout: int
    node_id: str = ""  # fp:<fingerprint> of the remote node (audience for request signing)


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
    map from ``remotes/remotes.yaml``.

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
            f"Remote '{name}' not found in remotes/remotes.yaml. "
            f"Available remotes: {list(remotes.keys()) or ['(none)']}"
        )

    entry = remotes[name]
    if not isinstance(entry, dict):
        raise ValueError(
            f"Remote '{name}' must be a mapping (url + key_env), "
            f"got {type(entry).__name__}"
        )
    url = entry.get("url", "")
    if not url:
        raise ValueError(
            f"Remote '{name}' has no url configured in remotes/remotes.yaml"
        )
    node_id = entry.get("node_id", "")
    defaults = config.get("defaults", {})
    timeout = entry.get("timeout", defaults.get("timeout"))
    if timeout is None:
        raise ValueError(
            f"Remote '{name}' has no timeout configured and no defaults.timeout "
            f"in remotes/remotes.yaml"
        )
    return RemoteConfig(name=name, url=url, timeout=timeout, node_id=node_id)


def get_project_path(project_path: Optional[Path] = None) -> str:
    """Get stable project path identifier from config, falling back to dir basename."""
    config = _load_remote_config(project_path)
    name = config.get("project_path")
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
        result[rname] = {
            "url": entry.get("url", ""),
            "node_id": entry.get("node_id", ""),
        }
    return result
