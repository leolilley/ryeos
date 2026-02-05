"""Centralized tool extension registry.

Loads extensions from data-driven extractor tools across 3-tier space:
  1. Project: {project}/.ai/tools/rye/core/extractors/
  2. User: ~/.ai/tools/rye/core/extractors/
  3. System: site-packages/rye/.ai/tools/rye/core/extractors/
"""

from pathlib import Path
from typing import List, Optional
import logging
import ast

from rye.utils.path_utils import (
    get_user_space,
    get_system_space,
    get_extractor_search_paths,
)

logger = logging.getLogger(__name__)

# Global cache - single source of truth
_extensions_cache: Optional[List[str]] = None


def get_tool_extensions(
    project_path: Optional[Path] = None, force_reload: bool = False
) -> List[str]:
    """
    Get supported tool file extensions from extractors.

    Loads EXTENSIONS from extractor tools across all 3 spaces.

    Args:
        project_path: Optional project path for extractor discovery
        force_reload: Force reload extractors (useful for testing)

    Returns:
        List of supported extensions (e.g., ['.py', '.js', '.yaml'])
    """
    global _extensions_cache

    if _extensions_cache is not None and not force_reload:
        return _extensions_cache

    extensions = set()
    search_paths = get_extractor_search_paths(project_path)

    for extractors_dir in search_paths:
        if not extractors_dir.exists():
            continue

        for file_path in extractors_dir.glob("**/*_extractor.py"):
            if file_path.name.startswith("_"):
                continue

            ext_list = _extract_extensions_from_file(file_path)
            extensions.update(ext_list)

    _extensions_cache = list(extensions) if extensions else [".py"]
    logger.debug(f"Loaded tool extensions: {_extensions_cache}")
    return _extensions_cache


def _extract_extensions_from_file(file_path: Path) -> List[str]:
    """Extract EXTENSIONS list from an extractor file using AST."""
    try:
        content = file_path.read_text()
        tree = ast.parse(content)

        for node in tree.body:
            if isinstance(node, ast.Assign) and len(node.targets) == 1:
                target = node.targets[0]
                if isinstance(target, ast.Name) and target.id == "EXTENSIONS":
                    if isinstance(node.value, ast.List):
                        return [
                            elt.value
                            for elt in node.value.elts
                            if isinstance(elt, ast.Constant)
                            and isinstance(elt.value, str)
                        ]
        return []
    except Exception as e:
        logger.warning(f"Failed to extract extensions from {file_path}: {e}")
        return []


def clear_extensions_cache():
    """Clear the extensions cache. Useful for testing."""
    global _extensions_cache
    _extensions_cache = None
