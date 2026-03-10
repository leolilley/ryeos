"""Source manifest builder.

Builds SourceManifest objects from project or user spaces.
Walks .ai/ to produce `items` (wrapped as item_source objects),
and optionally walks non-.ai/ directories to produce `files` (raw blobs).

Exclusion policy and default sync config are loaded from
.ai/config/cas/manifest.yaml and .ai/config/cas/remote.yaml
via 3-tier resolution (system → user → project, deep merge).
Hard excludes are a floor — projects can add patterns but never remove them.
"""

from __future__ import annotations

import fnmatch
import logging
import os
import re
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple

import yaml

from lillux.primitives import cas

from rye.cas.objects import SourceManifest
from rye.cas.store import cas_root, ingest_item, _guess_item_type
from rye.constants import AI_DIR
from rye.utils.path_utils import get_system_spaces, get_user_space

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# 3-tier config resolution (standalone, no PrimitiveExecutor dependency)
# ---------------------------------------------------------------------------


def _deep_merge(base: Dict, override: Dict) -> Dict:
    """Deep merge override into base. Dicts merge recursively, else override."""
    result = dict(base)
    for key, value in override.items():
        if key in result and isinstance(result[key], dict) and isinstance(value, dict):
            result[key] = _deep_merge(result[key], value)
        elif key in result and isinstance(result[key], list) and isinstance(value, list):
            # Lists: union (add new items, preserve existing)
            merged = list(result[key])
            for item in value:
                if item not in merged:
                    merged.append(item)
            result[key] = merged
        else:
            result[key] = value
    return result


def _load_config_3tier(
    config_rel_path: str,
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Load a config file via 3-tier resolution: system → user → project.

    Each layer is deep-merged. Lists are unioned (project can add but
    items from lower layers are preserved).

    Args:
        config_rel_path: Path relative to .ai/config/ (e.g., "cas/manifest.yaml")
        project_path: Project root for project-space config.

    Returns:
        Merged config dict.
    """
    config: Dict[str, Any] = {}

    # System space (all bundles, lowest priority)
    for bundle in get_system_spaces():
        system_path = bundle.root_path / AI_DIR / "config" / config_rel_path
        if system_path.exists():
            try:
                with open(system_path) as f:
                    layer = yaml.safe_load(f) or {}
                config = _deep_merge(config, layer)
            except Exception:
                logger.warning("Failed to load system config %s", system_path, exc_info=True)

    # User space
    user_path = get_user_space() / AI_DIR / "config" / config_rel_path
    if user_path.exists():
        try:
            with open(user_path) as f:
                layer = yaml.safe_load(f) or {}
            config = _deep_merge(config, layer)
        except Exception:
            logger.warning("Failed to load user config %s", user_path, exc_info=True)

    # Project space (highest priority)
    if project_path:
        project_config_path = project_path / AI_DIR / "config" / config_rel_path
        if project_config_path.exists():
            try:
                with open(project_config_path) as f:
                    layer = yaml.safe_load(f) or {}
                config = _deep_merge(config, layer)
            except Exception:
                logger.warning(
                    "Failed to load project config %s",
                    project_config_path,
                    exc_info=True,
                )

    return config


# ---------------------------------------------------------------------------
# Manifest policy — loaded from config, enforced as floor
# ---------------------------------------------------------------------------


def _load_manifest_policy(
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Load manifest exclusion policy from .ai/config/cas/manifest.yaml.

    Returns the 'manifest' section with skip_dirs, hard_exclude.names,
    hard_exclude.patterns resolved via 3-tier merge.
    """
    config = _load_config_3tier("cas/manifest.yaml", project_path)
    return config.get("manifest", {})


def _get_skip_dirs(policy: Dict[str, Any]) -> set:
    """Get skip dirs from policy, falling back to empty set."""
    return set(policy.get("skip_dirs", []))


def _get_hard_exclude_names(policy: Dict[str, Any]) -> set:
    """Get hard exclude names from policy."""
    hard = policy.get("hard_exclude", {})
    return set(hard.get("names", []))


def _get_hard_exclude_patterns(policy: Dict[str, Any]) -> list:
    """Get compiled hard exclude patterns from policy."""
    hard = policy.get("hard_exclude", {})
    patterns = hard.get("patterns", [])
    compiled = []
    for p in patterns:
        try:
            compiled.append(re.compile(p))
        except re.error:
            logger.warning("Invalid exclusion pattern: %s", p)
    return compiled


def _is_hard_excluded(
    filename: str,
    exclude_names: set,
    exclude_patterns: list,
) -> bool:
    """Check if a filename matches hard exclusion rules."""
    if filename in exclude_names:
        return True
    return any(p.match(filename) for p in exclude_patterns)


def _load_sync_config(project_path: Optional[Path] = None) -> Dict[str, List[str]]:
    """Load sync config from .ai/config/cas/remote.yaml via 3-tier resolution."""
    config = _load_config_3tier("cas/remote.yaml", project_path)
    return config.get("sync", {})


# ---------------------------------------------------------------------------
# Glob matching
# ---------------------------------------------------------------------------


def _matches_any_glob(rel_path: str, patterns: List[str]) -> bool:
    """Check if rel_path matches any of the glob patterns.

    Patterns ending in / match directory prefixes.
    """
    for pattern in patterns:
        if pattern.endswith("/"):
            if rel_path.startswith(pattern) or rel_path + "/" == pattern:
                return True
        elif fnmatch.fnmatch(rel_path, pattern):
            return True
    return False


# ---------------------------------------------------------------------------
# Tree walkers
# ---------------------------------------------------------------------------


def _walk_ai_items(
    space_root: Path,
    project_path: Path,
) -> Dict[str, str]:
    """Walk .ai/ tree, ingest all files as item_source objects.

    Returns {relative_path: item_source_object_hash}.
    Skips dirs and hard-excluded files per manifest policy.
    """
    ai_path = space_root / AI_DIR
    if not ai_path.is_dir():
        return {}

    policy = _load_manifest_policy(project_path)
    skip_dirs = _get_skip_dirs(policy)
    exclude_names = _get_hard_exclude_names(policy)
    exclude_patterns = _get_hard_exclude_patterns(policy)

    items: Dict[str, str] = {}

    for dirpath, dirnames, filenames in os.walk(ai_path):
        rel_dir = Path(dirpath).relative_to(space_root)

        # Skip runtime directories at .ai/ level
        if rel_dir == Path(AI_DIR):
            dirnames[:] = [d for d in dirnames if d not in skip_dirs]

        for filename in filenames:
            if _is_hard_excluded(filename, exclude_names, exclude_patterns):
                continue

            file_path = Path(dirpath) / filename
            rel_path = str(file_path.relative_to(space_root))
            item_type = _guess_item_type(rel_path)

            try:
                ref = ingest_item(item_type, file_path, project_path)
                items[rel_path] = ref.object_hash
            except Exception:
                logger.warning("Failed to ingest %s", rel_path, exc_info=True)

    return items


def _walk_project_files(
    space_root: Path,
    project_path: Path,
    include: List[str],
    exclude: List[str],
) -> Dict[str, str]:
    """Walk non-.ai/ directories per include/exclude, store as raw blobs.

    Returns {relative_path: blob_hash}.
    """
    root = cas_root(project_path)

    policy = _load_manifest_policy(project_path)
    exclude_names = _get_hard_exclude_names(policy)
    exclude_patterns = _get_hard_exclude_patterns(policy)

    files: Dict[str, str] = {}

    # Filter to non-.ai/ include patterns
    file_includes = [p for p in include if not p.startswith(f"{AI_DIR}/") and p != f"{AI_DIR}/"]

    if not file_includes:
        return files

    # Always exclude .ai/ from file walking (handled separately as items)
    always_exclude = [f"{AI_DIR}/"]
    effective_exclude = always_exclude + exclude

    for dirpath, dirnames, filenames in os.walk(space_root):
        rel_dir_str = str(Path(dirpath).relative_to(space_root))
        if rel_dir_str == ".":
            rel_dir_str = ""

        # Prune excluded directories
        dirnames[:] = [
            d for d in dirnames
            if not _matches_any_glob(
                (f"{rel_dir_str}/{d}/" if rel_dir_str else f"{d}/"),
                effective_exclude,
            )
        ]

        for filename in filenames:
            if _is_hard_excluded(filename, exclude_names, exclude_patterns):
                continue

            file_path = Path(dirpath) / filename
            rel_path = str(file_path.relative_to(space_root))

            # Must match an include pattern
            if not _matches_any_glob(rel_path, file_includes):
                continue

            # Must not match an exclude pattern
            if _matches_any_glob(rel_path, effective_exclude):
                continue

            try:
                blob_hash = cas.store_blob(file_path.read_bytes(), root)
                files[rel_path] = blob_hash
            except Exception:
                logger.warning("Failed to store file %s", rel_path, exc_info=True)

    return files


# ---------------------------------------------------------------------------
# Public API
# ---------------------------------------------------------------------------


def build_manifest(
    space_root: Path,
    space: str,
    project_path: Optional[Path] = None,
) -> Tuple[str, dict]:
    """Build a source manifest for a space.

    Args:
        space_root: Root directory to walk (project root or user home).
        space: "project" or "user".
        project_path: Project path for CAS storage. Defaults to space_root.

    Returns:
        (manifest_hash, manifest_dict)
    """
    if project_path is None:
        project_path = space_root

    root = cas_root(project_path)

    # Always walk .ai/ for items
    items = _walk_ai_items(space_root, project_path)

    # For project manifests, optionally walk non-.ai/ files
    files: Dict[str, str] = {}
    if space == "project":
        sync_config = _load_sync_config(project_path)
        include = sync_config.get("include", [])
        exclude = sync_config.get("exclude", [])

        if include:
            files = _walk_project_files(space_root, project_path, include, exclude)

    manifest = SourceManifest(space=space, items=items, files=files)
    manifest_dict = manifest.to_dict()
    manifest_hash = cas.store_object(manifest_dict, root)

    return manifest_hash, manifest_dict
