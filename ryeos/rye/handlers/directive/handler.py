"""
Directive handler for RYE.

Routes directive operations to the appropriate tools and parsers.
"""

import logging
from pathlib import Path
from typing import Any, Dict, Optional

from rye.utils.resolvers import get_user_space
from rye.utils.parser_router import ParserRouter
from rye.constants import AI_DIR, ItemType
from rye.utils.path_utils import (
    get_project_type_path,
    get_user_type_path,
    get_system_spaces,
)

logger = logging.getLogger(__name__)


class DirectiveHandler:
    """Handler for directive operations."""

    def __init__(
        self, project_path: Optional[str] = None, user_space: Optional[str] = None
    ):
        """Initialize handler."""
        self.project_path = Path(project_path) if project_path else Path.cwd()
        self.user_space = Path(user_space) if user_space else get_user_space()
        self.parser_router = ParserRouter()

    def get_search_paths(self) -> list[Path]:
        """Get directive search paths in precedence order."""
        paths = []

        # Project directives
        project_directives = get_project_type_path(self.project_path, "directive")
        if project_directives.exists():
            paths.append(project_directives)

        # User directives
        user_directives = get_user_type_path("directive")
        if user_directives.exists():
            paths.append(user_directives)

        # System directives (type roots, not category-scoped)
        type_folder = ItemType.TYPE_DIRS.get("directive", "directives")
        for bundle in get_system_spaces():
            system_dir = bundle.root_path / AI_DIR / type_folder
            if system_dir.exists():
                paths.append(system_dir)

        return paths

    def resolve(self, directive_name: str) -> Optional[Path]:
        """Find directive file by name."""
        for search_path in self.get_search_paths():
            for file_path in search_path.rglob(f"{directive_name}.md"):
                if file_path.is_file():
                    return file_path
        return None

    def parse(self, file_path: Path) -> Dict[str, Any]:
        """Parse directive file."""
        content = file_path.read_text(encoding="utf-8")
        return self.parser_router.parse("markdown_xml", content)

    def validate(self, file_path: Path) -> Dict[str, Any]:
        """Validate directive structure."""
        try:
            parsed = self.parse(file_path)
            if "error" in parsed:
                return {"valid": False, "issues": [parsed["error"]]}

            issues = []

            # Check required fields
            if not parsed.get("name"):
                issues.append("Missing directive name attribute")
            if not parsed.get("version"):
                issues.append("Missing version attribute")

            return {"valid": len(issues) == 0, "issues": issues}
        except Exception as e:
            return {"valid": False, "issues": [str(e)]}
