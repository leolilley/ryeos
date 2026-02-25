"""
Path Resolution Utilities

Finds directives, tools, and knowledge entries across 3-tier space system:
  1. Project space: {project}/.ai/ (highest priority)
  2. User space: {$USER_SPACE or ~}/.ai/
  3. System space: all installed bundles via rye.bundles entry points (lowest priority)

System space supports multiple bundles — each installed package (ryeos, ryeos-web,
ryeos-code, etc.) registers its own bundle with categories that scope which .ai/
subdirectories it contributes. All bundles are discovered and searched.

USER_SPACE env var sets the base path (home dir), not the .ai folder itself.
"""

from pathlib import Path
from typing import Optional, Tuple, List
import logging

from rye.utils.extensions import get_tool_extensions, get_item_extensions
from rye.utils.path_utils import (
    get_user_space,
    get_system_spaces,
    get_project_type_path,
    get_user_type_path,
)
from rye.constants import ItemType

logger = logging.getLogger(__name__)


class DirectiveResolver:
    """Resolve directive file paths across 3-tier space."""

    def __init__(self, project_path: Optional[Path] = None):
        self.project_path = project_path or Path.cwd()
        self.user_space = get_user_space()

    def get_search_paths(self) -> List[Tuple[Path, str]]:
        """Get search paths in precedence order with space labels."""
        paths = []

        # Project space (highest priority)
        project_dir = get_project_type_path(self.project_path, ItemType.DIRECTIVE)
        if project_dir.exists():
            paths.append((project_dir, "project"))

        # User space
        user_dir = get_user_type_path(ItemType.DIRECTIVE)
        if user_dir.exists():
            paths.append((user_dir, "user"))

        # System space — all installed bundles (lowest priority)
        for bundle in get_system_spaces():
            for type_path in bundle.get_type_paths(ItemType.DIRECTIVE):
                if type_path.exists():
                    paths.append((type_path, f"system:{bundle.bundle_id}"))

        return paths

    def resolve(self, directive_id: str) -> Optional[Path]:
        """Find directive file by relative path ID in project > user > system order.
        
        Args:
            directive_id: Relative path from .ai/directives/ without extension.
                         e.g., "core/build" -> .ai/directives/core/build.md
        """
        for search_dir, _ in self.get_search_paths():
            file_path = search_dir / f"{directive_id}.md"
            if file_path.is_file():
                return file_path
        return None

    def resolve_with_space(self, directive_id: str) -> Optional[Tuple[Path, str]]:
        """Find directive by relative path ID and return (path, space) tuple."""
        for search_dir, space in self.get_search_paths():
            file_path = search_dir / f"{directive_id}.md"
            if file_path.is_file():
                return (file_path, space)
        return None


class ToolResolver:
    """Resolve tool file paths across 3-tier space."""

    def __init__(self, project_path: Optional[Path] = None):
        self.project_path = project_path or Path.cwd()
        self.user_space = get_user_space()

    def get_search_paths(self) -> List[Tuple[Path, str]]:
        """Get search paths in precedence order with space labels."""
        paths = []

        # Project space (highest priority)
        project_dir = get_project_type_path(self.project_path, ItemType.TOOL)
        if project_dir.exists():
            paths.append((project_dir, "project"))

        # User space
        user_dir = get_user_type_path(ItemType.TOOL)
        if user_dir.exists():
            paths.append((user_dir, "user"))

        # System space — all installed bundles (lowest priority)
        for bundle in get_system_spaces():
            for type_path in bundle.get_type_paths(ItemType.TOOL):
                if type_path.exists():
                    paths.append((type_path, f"system:{bundle.bundle_id}"))

        return paths

    def resolve(self, tool_id: str) -> Optional[Path]:
        """Find tool file by relative path ID in project > user > system order.
        
        Args:
            tool_id: Relative path from .ai/tools/ without extension.
                    e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
        """
        extensions = get_tool_extensions(self.project_path)

        for search_dir, _ in self.get_search_paths():
            for ext in extensions:
                file_path = search_dir / f"{tool_id}{ext}"
                if file_path.is_file():
                    return file_path
        return None

    def resolve_with_space(self, tool_id: str) -> Optional[Tuple[Path, str]]:
        """Find tool by relative path ID and return (path, space) tuple."""
        extensions = get_tool_extensions(self.project_path)

        for search_dir, space in self.get_search_paths():
            for ext in extensions:
                file_path = search_dir / f"{tool_id}{ext}"
                if file_path.is_file():
                    return (file_path, space)
        return None


class KnowledgeResolver:
    """Resolve knowledge entry file paths across 3-tier space."""

    def __init__(self, project_path: Optional[Path] = None):
        self.project_path = project_path or Path.cwd()
        self.user_space = get_user_space()

    def get_search_paths(self) -> List[Tuple[Path, str]]:
        """Get search paths in precedence order with space labels."""
        paths = []

        # Project space (highest priority)
        project_dir = get_project_type_path(self.project_path, ItemType.KNOWLEDGE)
        if project_dir.exists():
            paths.append((project_dir, "project"))

        # User space
        user_dir = get_user_type_path(ItemType.KNOWLEDGE)
        if user_dir.exists():
            paths.append((user_dir, "user"))

        # System space — all installed bundles (lowest priority)
        for bundle in get_system_spaces():
            for type_path in bundle.get_type_paths(ItemType.KNOWLEDGE):
                if type_path.exists():
                    paths.append((type_path, f"system:{bundle.bundle_id}"))

        return paths

    def resolve(self, entry_id: str) -> Optional[Path]:
        """Find knowledge entry by relative path ID in project > user > system order.
        
        Args:
            entry_id: Relative path from .ai/knowledge/ without extension.
                     e.g., "patterns/singleton" -> .ai/knowledge/patterns/singleton.md
        """
        for search_dir, _ in self.get_search_paths():
            for ext in get_item_extensions("knowledge"):
                file_path = search_dir / f"{entry_id}{ext}"
                if file_path.is_file():
                    return file_path
        return None

    def resolve_with_space(self, entry_id: str) -> Optional[Tuple[Path, str]]:
        """Find knowledge entry by relative path ID and return (path, space) tuple."""
        for search_dir, space in self.get_search_paths():
            for ext in get_item_extensions("knowledge"):
                file_path = search_dir / f"{entry_id}{ext}"
                if file_path.is_file():
                    return (file_path, space)
        return None
