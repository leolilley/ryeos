"""Path utilities for extracting category and validating file locations.

Provides functions to:
- Extract category path from file location (relative to .ai/{type}/)
- Validate filename matches metadata name/id
- Validate path structure
- Ensure directories exist (filesystem helpers)
"""

import os
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import ItemType


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
    """Get user space directory from env var or default to ~/.ai."""
    user_space = os.getenv("USER_SPACE")
    if user_space:
        return Path(user_space).expanduser()
    return Path.home() / ".ai"


def get_system_space() -> Path:
    """Get system space directory (bundled with rye package)."""
    return Path(__file__).parent.parent / ".ai"


def get_project_ai_path(project_path: Path) -> Path:
    """Get .ai directory in project space."""
    return project_path / ".ai"


def get_project_type_path(project_path: Path, item_type: str) -> Path:
    """Get item type directory in project space (e.g., {project}/.ai/tools/)."""
    folder_name = get_type_folder(item_type)
    return project_path / ".ai" / folder_name


def get_user_type_path(item_type: str) -> Path:
    """Get item type directory in user space (e.g., ~/.ai/tools/)."""
    folder_name = get_type_folder(item_type)
    return get_user_space() / folder_name


def get_system_type_path(item_type: str) -> Path:
    """Get item type directory in system space (e.g., site-packages/rye/.ai/tools/)."""
    folder_name = get_type_folder(item_type)
    return get_system_space() / folder_name


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
            project_path / ".ai" / "tools" / "rye" / "core" / "extractors"
        )
        if project_extractors.exists():
            paths.append(project_extractors)

    # User extractors
    user_extractors = get_user_space() / "tools" / "rye" / "core" / "extractors"
    if user_extractors.exists():
        paths.append(user_extractors)

    # System extractors (lowest priority)
    system_extractors = get_system_space() / "tools" / "rye" / "core" / "extractors"
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

    if location == "project":
        if not project_path:
            return ""
        expected_base = Path(project_path) / ".ai" / folder_name
    elif location == "user":
        expected_base = get_user_space() / folder_name
    elif location == "system":
        # System is bundled with rye package
        rye_pkg = Path(__file__).parent.parent
        expected_base = rye_pkg / ".ai" / folder_name
    else:
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
