"""
Tool handler for RYE.

Routes tool operations and manages executor chain resolution.
"""

import logging
from pathlib import Path
from typing import Any, Dict, Optional

from rye.utils.resolvers import get_user_space
from rye.utils.extensions import get_tool_extensions, get_parsers_map
from rye.utils.parser_router import ParserRouter
from rye.constants import AI_DIR, ItemType
from rye.utils.path_utils import (
    get_project_type_path,
    get_user_type_path,
    get_system_spaces,
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
        self.parser_router = ParserRouter(project_path=self.project_path)

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

        # System tools (type roots, not category-scoped)
        type_folder = ItemType.TYPE_DIRS.get("tool", "tools")
        for bundle in get_system_spaces():
            system_tools = bundle.root_path / AI_DIR / type_folder
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
        """Extract metadata from tool file via data-driven parsers.

        Routes to the appropriate parser based on the extension-to-parser
        mapping in tool_extractor.yaml.
        """
        from rye.executor.primitive_executor import PrimitiveExecutor

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

            parsers_map = get_parsers_map(self.project_path)
            parser_name = parsers_map.get(file_path.suffix)
            if not parser_name:
                return metadata

            parsed = self.parser_router.parse(parser_name, content)
            if "error" in parsed:
                return metadata

            extracted = PrimitiveExecutor._extract_metadata_from_parsed(parsed)
            metadata.update(
                {k: v for k, v in extracted.items() if v is not None}
            )

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
