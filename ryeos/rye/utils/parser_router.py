"""
Parser Router - Routes content parsing to data-driven parsers.

Parsers are loaded from .ai/tools/rye/core/parsers/ (system) and can be
overridden by project or user space at .ai/parsers/.

Each parser module exports a `parse(content: str) -> Dict[str, Any]` function.
"""

import importlib.util
import logging
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space, get_system_spaces

logger = logging.getLogger(__name__)


class ParserRouter:
    """Routes parsing requests to the appropriate data-driven parser."""

    def __init__(self, project_path: Optional[Path] = None):
        """Initialize parser router."""
        self.project_path = project_path
        self._parsers: Dict[str, Any] = {}

    def get_search_paths(self) -> List[Path]:
        """Get parser search paths in precedence order."""
        paths = []

        # Project parsers (highest priority)
        if self.project_path:
            project_parsers = self.project_path / AI_DIR / "parsers"
            if project_parsers.exists():
                paths.append(project_parsers)

        # User parsers
        user_parsers = get_user_space() / AI_DIR / "parsers"
        if user_parsers.exists():
            paths.append(user_parsers)

        # System parsers from all roots (lowest priority)
        for bundle in get_system_spaces():
            system_parsers = (
                bundle.root_path / AI_DIR / "tools" / "rye" / "core" / "parsers"
            )
            if system_parsers.exists():
                paths.append(system_parsers)

        return paths

    def _load_parser(self, parser_name: str) -> Optional[Any]:
        """Load a parser module by name."""
        if parser_name in self._parsers:
            return self._parsers[parser_name]

        for search_path in self.get_search_paths():
            parser_file = search_path / f"{parser_name}.py"
            if parser_file.exists():
                try:
                    spec = importlib.util.spec_from_file_location(
                        parser_name, parser_file
                    )
                    if spec and spec.loader:
                        module = importlib.util.module_from_spec(spec)
                        spec.loader.exec_module(module)
                        self._parsers[parser_name] = module
                        logger.debug(f"Loaded parser: {parser_name} from {parser_file}")
                        return module
                except Exception as e:
                    logger.warning(f"Failed to load parser {parser_name}: {e}")
                    continue

        logger.warning(f"Parser not found: {parser_name}")
        return None

    def parse(self, parser_name: str, content: str) -> Dict[str, Any]:
        """
        Parse content using the specified parser.

        Args:
            parser_name: Name of parser (e.g., "markdown/xml", "markdown/frontmatter")
            content: Content to parse

        Returns:
            Parsed data dict, or dict with "error" key on failure
        """
        parser = self._load_parser(parser_name)
        if not parser:
            return {"error": f"Parser not found: {parser_name}"}

        if not hasattr(parser, "parse"):
            return {"error": f"Parser {parser_name} has no parse() function"}

        try:
            return parser.parse(content)
        except Exception as e:
            logger.error(f"Parser {parser_name} failed: {e}")
            return {"error": str(e)}

    def list_parsers(self) -> List[str]:
        """List available parser names."""
        parsers = set()
        for search_path in self.get_search_paths():
            for file_path in search_path.rglob("*.py"):
                if not file_path.name.startswith("_"):
                    rel = file_path.relative_to(search_path).with_suffix("")
                    parsers.add(str(rel))
        return sorted(parsers)
