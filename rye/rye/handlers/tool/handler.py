"""
Tool handler for RYE.

Routes tool operations and manages executor chain resolution.
"""

import logging
from pathlib import Path
from typing import Any, Dict, Optional

from rye.utils.resolvers import get_user_space
from rye.utils.extensions import get_tool_extensions
from rye.utils.path_utils import (
    get_project_type_path,
    get_user_type_path,
    get_system_type_paths,
)

logger = logging.getLogger(__name__)


class ToolHandler:
    """Handler for tool operations."""

    def __init__(
        self, project_path: Optional[str] = None, user_space: Optional[str] = None
    ):
        """Initialize handler."""
        self.project_path = Path(project_path) if project_path else Path.cwd()
        self.user_space = Path(user_space) if user_space else get_user_space()

    def get_search_paths(self) -> list[Path]:
        """Get tool search paths in precedence order."""
        paths = []

        # Project tools
        project_tools = get_project_type_path(self.project_path, "tool")
        if project_tools.exists():
            paths.append(project_tools)

        # User tools
        user_tools = get_user_type_path("tool")
        if user_tools.exists():
            paths.append(user_tools)

        # System tools
        for _root_id, system_tools in get_system_type_paths("tool"):
            if system_tools.exists():
                paths.append(system_tools)

        return paths

    def resolve(self, tool_name: str) -> Optional[Path]:
        """Find tool file by name."""
        extensions = get_tool_extensions(self.project_path)

        for search_path in self.get_search_paths():
            for ext in extensions:
                for file_path in search_path.rglob(f"{tool_name}{ext}"):
                    if file_path.is_file():
                        return file_path
        return None

    def extract_metadata(self, file_path: Path) -> Dict[str, Any]:
        """Extract metadata from tool file using AST parsing."""
        import ast
        import re

        metadata = {
            "name": file_path.stem,
            "path": str(file_path),
            "extension": file_path.suffix,
            "version": None,
            "tool_type": None,
            "executor_id": None,
            "category": None,
        }

        try:
            content = file_path.read_text(encoding="utf-8")

            if file_path.suffix == ".py":
                tree = ast.parse(content)

                for node in tree.body:
                    if isinstance(node, ast.Assign) and len(node.targets) == 1:
                        target = node.targets[0]
                        if isinstance(target, ast.Name) and isinstance(
                            node.value, ast.Constant
                        ):
                            name = target.id
                            value = node.value.value
                            if name == "__version__":
                                metadata["version"] = value
                            elif name == "__tool_type__":
                                metadata["tool_type"] = value
                            elif name == "__executor_id__":
                                metadata["executor_id"] = value
                            elif name == "__category__":
                                metadata["category"] = value

            elif file_path.suffix in (".yaml", ".yml"):
                import yaml

                data = yaml.safe_load(content)
                if isinstance(data, dict):
                    metadata["version"] = data.get("version")
                    metadata["tool_type"] = data.get("tool_type")
                    metadata["executor_id"] = data.get("executor_id")
                    metadata["category"] = data.get("category")

        except Exception as e:
            logger.warning(f"Failed to extract metadata from {file_path}: {e}")

        return metadata

    def validate(self, file_path: Path) -> Dict[str, Any]:
        """Validate tool structure."""
        try:
            content = file_path.read_text(encoding="utf-8")
            issues = []

            if not content.strip():
                issues.append("Tool file is empty")

            metadata = self.extract_metadata(file_path)
            if not metadata.get("version"):
                issues.append("Missing __version__")

            return {"valid": len(issues) == 0, "issues": issues, "metadata": metadata}
        except Exception as e:
            return {"valid": False, "issues": [str(e)]}
