"""Path utilities for extracting category and validating file locations.

Provides functions to:
- Extract category path from file location (relative to .ai/{type}/)
- Validate filename matches metadata name/id
- Validate path structure
- Ensure directories exist (filesystem helpers)
- Discover and validate bundle manifests via entry points
"""

import importlib.metadata
import logging
import os
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple, Union

from rye.constants import AI_DIR, ItemType

logger = logging.getLogger(__name__)

_system_spaces_cache: Optional[List["BundleInfo"]] = None


@dataclass(frozen=True)
class BundleInfo:
    """Information about a registered bundle.

    Bundles are discovered via the `rye.bundles` entry point group.
    Each bundle provides a manifest that describes the directives, tools,
    and knowledge items it contains, along with their hashes for integrity.

    Attributes:
        bundle_id: Unique identifier for the bundle (e.g., "ryeos-core", "ryeos-mcp")
        version: Semantic version of the bundle
        root_path: Path to the bundle root directory containing .ai/
        manifest_path: Path to the bundle manifest.yaml (if exists)
        source: Entry point name that registered this bundle
    """

    bundle_id: str
    version: str
    root_path: Path
    manifest_path: Optional[Path]
    source: str
    categories: Optional[List[str]] = None

    def get_type_paths(self, item_type: str) -> List[Path]:
        """Get item type directories for this bundle.

        If categories is set, returns one path per category
        (e.g., .ai/tools/rye/core/). Otherwise returns the top-level
        type directory (e.g., .ai/tools/).
        """
        folder_name = ItemType.TYPE_DIRS.get(item_type, item_type)
        base = self.root_path / AI_DIR / folder_name
        if self.categories:
            return [base / cat for cat in self.categories]
        return [base]

    def has_manifest(self) -> bool:
        """Check if this bundle has a valid manifest file."""
        return self.manifest_path is not None and self.manifest_path.exists()

    def __repr__(self) -> str:
        return f"BundleInfo({self.bundle_id}@{self.version}, source={self.source})"


def ensure_directory(path: Path) -> Path:
    """Ensure directory exists, creating it and all parents if necessary.

    Args:
        path: Directory path to ensure exists

    Returns:
        The path (for chaining)

    Raises:
        OSError: If directory cannot be created
    """
    path = Path(path)
    path.mkdir(parents=True, exist_ok=True)
    return path


def ensure_parent_directory(file_path: Path) -> Path:
    """Ensure parent directory of file path exists.

    Args:
        file_path: File path whose parent should exist

    Returns:
        The file path (for chaining)
    """
    return ensure_directory(file_path.parent)


def get_user_space() -> Path:
    """Get user space base directory from env var or default to home directory.

    Returns the base path (home dir or $USER_SPACE), not including .ai folder.
    AI_DIR is appended by get_user_ai_path() and get_user_type_path().
    """
    user_space = os.getenv("USER_SPACE")
    if user_space:
        return Path(user_space).expanduser()
    return Path.home()


def get_user_ai_path() -> Path:
    """Get .ai directory in user space (e.g., ~/.ai).

    This is the working directory for user space.
    Item types are in subdirectories: .ai/tools/, .ai/directives/, .ai/knowledge/
    """
    return get_user_space() / AI_DIR


def _parse_bundle_entry_point(ep_name: str, result: Any) -> Optional[BundleInfo]:
    """Parse entry point result into BundleInfo.

    Entry points must return a dict with:
        - bundle_id: Unique identifier for the bundle
        - root_path: Path to bundle root directory containing .ai/
        - version: Semantic version (optional, defaults to "0.0.0")
        - manifest_path: Path to manifest.yaml (optional, auto-discovered)
        - categories: List of category prefixes to include (optional, None = all)
    """
    if isinstance(result, dict):
        bundle_id = result.get("bundle_id", ep_name)
        version = result.get("version", "0.0.0")
        root_path = Path(result.get("root_path", "."))
        categories = result.get("categories")

        # Manifest path can be explicit or auto-discovered
        manifest_path = result.get("manifest_path")
        if manifest_path:
            manifest_path = Path(manifest_path)
        else:
            # Auto-discover: .ai/bundles/{bundle_id}/manifest.yaml
            manifest_path = root_path / AI_DIR / "bundles" / bundle_id / "manifest.yaml"
            if not manifest_path.exists():
                manifest_path = None

        return BundleInfo(
            bundle_id=bundle_id,
            version=version,
            root_path=root_path,
            manifest_path=manifest_path,
            source=ep_name,
            categories=categories,
        )

    logger.warning(
        "Entry point %r returned unsupported type %r, expected dict",
        ep_name,
        type(result).__name__,
    )
    return None


def get_system_spaces() -> List[BundleInfo]:
    """Get all system bundle spaces discovered via entry points.

    Returns a list of BundleInfo objects sorted alphabetically by entry point name.
    All bundles are discovered via the ``rye.bundles`` entry point group.

    Entry points must return a dict with:
        - bundle_id: Unique identifier for the bundle
        - root_path: Path to bundle root directory containing .ai/
        - version: Semantic version (optional, defaults to "0.0.0")
        - manifest_path: Path to manifest.yaml (optional, auto-discovered)

    Results are cached at module level after first computation.
    """
    global _system_spaces_cache
    if _system_spaces_cache is not None:
        return _system_spaces_cache

    # Discover all bundles via entry points
    bundles: List[BundleInfo] = []
    eps = importlib.metadata.entry_points(group="rye.bundles")
    for ep in sorted(eps, key=lambda e: e.name):
        try:
            fn = ep.load()
            result = fn()
            bundle_info = _parse_bundle_entry_point(ep.name, result)
            if bundle_info:
                bundles.append(bundle_info)
        except Exception:
            logger.warning(
                "Failed to load rye.bundles entry point %r",
                ep.name,
                exc_info=True,
            )

    _system_spaces_cache = bundles
    return _system_spaces_cache


def get_project_ai_path(project_path: Path) -> Path:
    """Get .ai directory in project space."""
    return project_path / AI_DIR


def get_project_type_path(project_path: Path, item_type: str) -> Path:
    """Get item type directory in project space (e.g., {project}/.ai/tools/)."""
    folder_name = get_type_folder(item_type)
    return project_path / AI_DIR / folder_name


def get_user_type_path(item_type: str) -> Path:
    """Get item type directory in user space (e.g., ~/.ai/tools/)."""
    folder_name = get_type_folder(item_type)
    return get_user_space() / AI_DIR / folder_name


def get_system_type_paths(item_type: str) -> List[Tuple[str, Path]]:
    """Get item type directories across all system bundles.

    Each bundle may contribute multiple paths if it has categories set.
    """
    result: List[Tuple[str, Path]] = []
    for bundle in get_system_spaces():
        for path in bundle.get_type_paths(item_type):
            result.append((bundle.bundle_id, path))
    return result


def get_extractor_search_paths(project_path: Optional[Path] = None) -> List[Path]:
    """Get search paths for extractor tools in precedence order.

    Returns paths in order: project > user > system

    Args:
        project_path: Optional project root for extractor discovery

    Returns:
        List of extractor directories in precedence order
    """
    paths = []

    # Project extractors (highest priority)
    if project_path:
        project_extractors = (
            project_path / AI_DIR / "tools" / "rye" / "core" / "extractors"
        )
        if project_extractors.exists():
            paths.append(project_extractors)

    # User extractors
    user_extractors = get_user_ai_path() / "tools" / "rye" / "core" / "extractors"
    if user_extractors.exists():
        paths.append(user_extractors)

    # System extractors from all roots (lowest priority)
    for bundle in get_system_spaces():
        system_extractors = (
            bundle.root_path / AI_DIR / "tools" / "rye" / "core" / "extractors"
        )
        if system_extractors.exists():
            paths.append(system_extractors)

    return paths


def get_type_folder(item_type: str) -> str:
    """Get folder name for item type."""
    return ItemType.TYPE_DIRS.get(item_type, item_type)


def extract_category_path(
    file_path: Path,
    item_type: str,
    location: str,
    project_path: Optional[Path] = None,
) -> str:
    """Extract category path from file location as slash-separated string.

    Args:
        file_path: Full path to the file
        item_type: "directive", "tool", or "knowledge"
        location: "project", "user", or "system"
        project_path: Project root (for project location)

    Returns:
        Category path as string: "core/api" or "" if in base directory

    Example:
        .ai/tools/utility/git.py -> "utility"
        .ai/directives/core/api/research.md -> "core/api"
        .ai/knowledge/patterns/api.md -> "patterns"
    """
    folder_name = get_type_folder(item_type)
    expected_base: Optional[Path] = None

    if location == "project":
        if not project_path:
            return ""
        expected_base = Path(project_path) / AI_DIR / folder_name
    elif location == "user":
        expected_base = get_user_ai_path() / folder_name
    elif location.startswith("system"):
        # Try all system roots to find which one contains this file
        for bundle in get_system_spaces():
            expected_base = bundle.root_path / AI_DIR / folder_name
            try:
                relative = file_path.relative_to(expected_base)
                parts = list(relative.parent.parts)
                return "/".join(parts) if parts else ""
            except ValueError:
                continue
        return ""

    if expected_base is None:
        return ""

    try:
        relative = file_path.relative_to(expected_base)
        # Remove filename, get directory parts
        parts = list(relative.parent.parts)
        # Join with slashes to create category path string
        return "/".join(parts) if parts else ""
    except ValueError:
        return ""


def extract_filename(file_path: Path) -> str:
    """Extract filename without extension."""
    return file_path.stem


def validate_name_matches_filename(
    metadata_name: str,
    file_path: Path,
) -> Dict[str, Any]:
    """Validate that metadata name matches filename.

    Args:
        metadata_name: Name from metadata (directive name, tool_id, knowledge id)
        file_path: Path to the file

    Returns:
        {"valid": bool, "issues": List[str], "filename": str, "metadata_name": str}
    """
    filename = extract_filename(file_path)
    issues = []

    if metadata_name != filename:
        issues.append(
            f"Name mismatch: metadata says '{metadata_name}' but filename is '{filename}'"
        )

    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "filename": filename,
        "metadata_name": metadata_name,
    }


def validate_category_matches_path(
    metadata_category: Optional[str],
    file_path: Path,
    item_type: str,
    location: str,
    project_path: Optional[Path] = None,
) -> Dict[str, Any]:
    """Validate that metadata category matches file location.

    Args:
        metadata_category: Category from metadata
        file_path: Path to the file
        item_type: "directive", "tool", or "knowledge"
        location: "project", "user", or "system"
        project_path: Project root (for project location)

    Returns:
        {"valid": bool, "issues": List[str], "path_category": str, "metadata_category": str}
    """
    path_category = extract_category_path(file_path, item_type, location, project_path)
    issues = []

    # Normalize: None or empty string both mean "root"
    meta_cat = metadata_category or ""

    if meta_cat != path_category:
        issues.append(
            f"Category mismatch: metadata says '{meta_cat}' but file is in '{path_category}'"
        )

    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "path_category": path_category,
        "metadata_category": meta_cat,
    }


def validate_path_structure(
    file_path: Path,
    item_type: str,
    location: str,
    project_path: Optional[Path] = None,
    metadata_name: Optional[str] = None,
    metadata_category: Optional[str] = None,
) -> Dict[str, Any]:
    """Full validation of path structure, name, and category.

    Args:
        file_path: Path to validate
        item_type: "directive", "tool", or "knowledge"
        location: "project", "user", or "system"
        project_path: Project root (for project location)
        metadata_name: Name from metadata to validate against filename
        metadata_category: Category from metadata to validate against path

    Returns:
        {
            "valid": bool,
            "issues": List[str],
            "filename": str,
            "category": str,
            "location": str
        }
    """
    issues: List[str] = []
    filename = extract_filename(file_path)
    category = extract_category_path(file_path, item_type, location, project_path)

    # Validate name if provided
    if metadata_name is not None:
        name_result = validate_name_matches_filename(metadata_name, file_path)
        issues.extend(name_result["issues"])

    # Validate category if provided
    if metadata_category is not None:
        cat_result = validate_category_matches_path(
            metadata_category, file_path, item_type, location, project_path
        )
        issues.extend(cat_result["issues"])

    return {
        "valid": len(issues) == 0,
        "issues": issues,
        "filename": filename,
        "category": category,
        "location": location,
    }
