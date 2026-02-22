"""Centralized extension registry.

Loads extensions from data-driven extractor tools across 3-tier space:
  1. Project: {project}/.ai/tools/rye/core/extractors/
  2. User: {USER_SPACE}/.ai/tools/rye/core/extractors/
  3. System: site-packages/rye/.ai/tools/rye/core/extractors/
"""

import ast
import logging
from pathlib import Path
from typing import Dict, List, Optional

from rye.utils.path_utils import get_extractor_search_paths

logger = logging.getLogger(__name__)

# Global cache - single source of truth
_extensions_cache: Optional[List[str]] = None
_type_extensions_cache: Dict[str, List[str]] = {}
_parsers_map_cache: Optional[Dict[str, str]] = None


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

        for file_path in list(extractors_dir.glob("**/*_extractor.yaml")) + list(
            extractors_dir.glob("**/*_extractor.py")
        ):
            if file_path.name.startswith("_"):
                continue

            ext_list = _extract_extensions_from_file(file_path)
            extensions.update(ext_list)

    _extensions_cache = list(extensions) if extensions else [".py"]
    logger.debug(f"Loaded tool extensions: {_extensions_cache}")
    return _extensions_cache


_TYPE_EXTRACTOR_GLOB = {
    "tool": "tool/*_extractor.*",
    "directive": "directive/*_extractor.*",
    "knowledge": "knowledge/*_extractor.*",
}

_TYPE_DEFAULTS = {
    "tool": [".py"],
    "directive": [".md"],
    "knowledge": [".md"],
}


def get_item_extensions(
    item_type: str,
    project_path: Optional[Path] = None,
    force_reload: bool = False,
) -> List[str]:
    """Get supported file extensions for an item type from its extractor.

    Reads the `extensions` field from the type-specific extractor YAML
    (e.g., knowledge/knowledge_extractor.yaml) across the 3-tier space.
    """
    if item_type in _type_extensions_cache and not force_reload:
        return _type_extensions_cache[item_type]

    glob_pattern = _TYPE_EXTRACTOR_GLOB.get(item_type)
    if not glob_pattern:
        return _TYPE_DEFAULTS.get(item_type, [".md"])

    extensions = set()
    for extractors_dir in get_extractor_search_paths(project_path):
        if not extractors_dir.exists():
            continue
        for file_path in extractors_dir.glob(glob_pattern):
            if file_path.name.startswith("_"):
                continue
            extensions.update(_extract_extensions_from_file(file_path))

    result = list(extensions) if extensions else _TYPE_DEFAULTS.get(item_type, [".md"])
    _type_extensions_cache[item_type] = result
    return result


def _extract_extensions_from_file(file_path: Path) -> List[str]:
    """Extract EXTENSIONS list from an extractor file."""
    if file_path.suffix in (".yaml", ".yml"):
        import yaml

        try:
            data = yaml.safe_load(file_path.read_text())
            return data.get("extensions", []) if data else []
        except Exception as e:
            logger.warning(f"Failed to load YAML extensions from {file_path}: {e}")
            return []

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


def get_parsers_map(
    project_path: Optional[Path] = None, force_reload: bool = False
) -> Dict[str, str]:
    """Get extension-to-parser mapping from tool extractor config.

    Reads the ``parsers`` field from the tool extractor YAML across
    the 3-tier space (project > user > system).  Returns a dict like
    ``{".py": "python/ast", ".ts": "javascript/javascript", ...}``.
    """
    global _parsers_map_cache

    if _parsers_map_cache is not None and not force_reload:
        return _parsers_map_cache

    parsers_map: Dict[str, str] = {}
    search_paths = get_extractor_search_paths(project_path)

    for extractors_dir in search_paths:
        if not extractors_dir.exists():
            continue

        for file_path in extractors_dir.glob("**/tool/*_extractor.yaml"):
            if file_path.name.startswith("_"):
                continue

            try:
                import yaml

                data = yaml.safe_load(file_path.read_text())
                if data and isinstance(data.get("parsers"), dict):
                    # First-found wins (project > user > system)
                    for ext, parser_name in data["parsers"].items():
                        if ext not in parsers_map:
                            parsers_map[ext] = parser_name
            except Exception as e:
                logger.warning(
                    f"Failed to load parsers map from {file_path}: {e}"
                )

    _parsers_map_cache = parsers_map
    return _parsers_map_cache


def clear_extensions_cache():
    """Clear all extensions caches. Useful for testing."""
    global _extensions_cache, _parsers_map_cache
    _extensions_cache = None
    _parsers_map_cache = None
    _type_extensions_cache.clear()
