"""Sign tool - validate and sign items.

Validates using schema-driven approach:
1. Loads VALIDATION_SCHEMA from extractor for item type
2. Validates all required fields and their types
3. Validates filename/path matching
4. Signs content with integrity hash

The integrity hash is computed from content which includes
metadata. Moving an item without updating metadata will cause
verification to fail.
"""

import logging
from pathlib import Path
from typing import Any, Dict, List, Optional

from rye.constants import AI_DIR, ItemType
from rye.utils.metadata_manager import MetadataManager
from rye.utils.parser_router import ParserRouter
from rye.utils.path_utils import (
    extract_filename,
    get_project_type_path,
    get_system_spaces,
    get_user_type_path,
)
from rye.utils.extensions import get_tool_extensions, get_item_extensions, get_parsers_map
from rye.utils.resolvers import get_user_space
from rye.utils.validators import apply_field_mapping, validate_parsed_data

logger = logging.getLogger(__name__)


def _load_exclude_dirs(project_path: Optional[Path] = None) -> frozenset:
    """Load exclude_dirs from collect.yaml via 3-tier resolution.

    Uses the same config the bundler uses so signing and bundling
    skip the same directories (node_modules, __pycache__, etc.).
    """
    import yaml as _yaml

    config_name = "rye/core/bundler/collect.yaml"
    search_order = []
    if project_path:
        search_order.append(Path(project_path) / AI_DIR / "tools" / config_name)
    search_order.append(get_user_space() / AI_DIR / "tools" / config_name)
    for bundle in get_system_spaces():
        search_order.append(bundle.root_path / AI_DIR / "tools" / config_name)

    for path in search_order:
        if path.exists():
            try:
                data = _yaml.safe_load(path.read_text(encoding="utf-8"))
                if isinstance(data, dict) and "exclude_dirs" in data:
                    return frozenset(data["exclude_dirs"])
            except Exception:
                continue

    return frozenset()


class SignTool:
    """Validate and sign directives, tools, or knowledge entries."""

    def __init__(self, user_space: Optional[str] = None):
        """Initialize sign tool."""
        self.user_space = user_space or str(get_user_space())
        self.parser_router = ParserRouter()

    def _is_glob_pattern(self, item_id: str) -> bool:
        """Check if item_id is a glob pattern."""
        return "*" in item_id or "?" in item_id

    def _get_batch_base_dir(
        self, item_type: str, project_path: str, source: str
    ) -> Optional[Path]:
        """Get the base directory for computing relative path IDs."""
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return None
        if source == "project":
            return get_project_type_path(Path(project_path), item_type)
        elif source == "user":
            return get_user_type_path(item_type)
        elif source == "system":
            type_folder = ItemType.TYPE_DIRS.get(item_type, item_type)
            for bundle in get_system_spaces():
                p = bundle.root_path / AI_DIR / type_folder
                if p.exists():
                    return p
            return None
        return None

    def _resolve_glob_items(
        self, item_type: str, pattern: str, project_path: str, source: str
    ) -> List[Path]:
        """Resolve glob pattern to list of item file paths."""
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return []

        if source == "project":
            base_dir = get_project_type_path(Path(project_path), item_type)
        elif source == "user":
            base_dir = get_user_type_path(item_type)
        elif source == "system":
            all_items = []
            type_folder = ItemType.TYPE_DIRS.get(item_type, item_type)
            for bundle in get_system_spaces():
                sys_dir = bundle.root_path / AI_DIR / type_folder
                if not sys_dir.exists():
                    continue
                all_items.extend(self._glob_in_dir(sys_dir, item_type, pattern, project_path))
            return all_items
        else:
            return []

        if not base_dir.exists():
            return []

        return self._glob_in_dir(base_dir, item_type, pattern, project_path)

    def _glob_in_dir(
        self, base_dir: Path, item_type: str, pattern: str, project_path: str
    ) -> List[Path]:
        """Glob for items inside a single base directory.

        Skips paths containing directories listed in collect.yaml's
        exclude_dirs (node_modules, __pycache__, .venv, etc.) to avoid
        picking up third-party vendored files that aren't Rye items.
        """
        exclude = _load_exclude_dirs(Path(project_path) if project_path else None)

        if item_type == ItemType.TOOL:
            tool_extensions = get_tool_extensions(
                Path(project_path) if project_path else None
            )
            items = []
            for tool_ext in tool_extensions:
                if "/" in pattern:
                    glob_pattern = f"{pattern}{tool_ext}" if not pattern.endswith(tool_ext) else pattern
                else:
                    glob_pattern = f"**/{pattern}{tool_ext}" if pattern != "*" else f"**/*{tool_ext}"
                for path in base_dir.glob(glob_pattern):
                    if path.is_file() and not (exclude & set(path.parts)):
                        items.append(path)
            return items
        else:
            ext = ".md"
            if "/" in pattern:
                glob_pattern = f"{pattern}{ext}" if not pattern.endswith(ext) else pattern
            else:
                glob_pattern = f"**/{pattern}{ext}" if pattern != "*" else f"**/*{ext}"
            items = []
            for path in base_dir.glob(glob_pattern):
                if path.is_file() and not (exclude & set(path.parts)):
                    items.append(path)
            return items

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle sign request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        source = kwargs.get("source", "project")

        logger.debug(f"Sign: item_type={item_type}, item_id={item_id}, source={source}")

        if source == "system":
            return {
                "status": "error",
                "error": "System space items are immutable and cannot be signed. Copy to project or user space first.",
                "item_id": item_id,
            }

        try:
            # Check for batch signing (glob pattern)
            if self._is_glob_pattern(item_id):
                return await self._sign_batch(item_type, item_id, project_path, source)

            if item_type == ItemType.DIRECTIVE:
                return await self._sign_directive(item_id, project_path, source)
            elif item_type == ItemType.TOOL:
                return await self._sign_tool(item_id, project_path, source)
            elif item_type == ItemType.KNOWLEDGE:
                return await self._sign_knowledge(item_id, project_path, source)
            else:
                return {"status": "error", "error": f"Unknown item type: {item_type}"}

        except Exception as e:
            logger.error(f"Sign error: {e}")
            return {"status": "error", "error": str(e), "item_id": item_id}

    async def _sign_batch(
        self, item_type: str, pattern: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Sign multiple items matching a glob pattern."""
        items = self._resolve_glob_items(item_type, pattern, project_path, source)

        if not items:
            return {
                "status": "error",
                "error": f"No {item_type}s found matching pattern: {pattern}",
                "searched_in": str(
                    get_project_type_path(Path(project_path), item_type)
                    if source == "project"
                    else get_user_type_path(item_type)
                ),
            }

        results: Dict[str, Any] = {"signed": [], "failed": [], "total": len(items)}

        base_dir = self._get_batch_base_dir(item_type, project_path, source)
        for file_path in items:
            if base_dir:
                item_id = str(file_path.relative_to(base_dir).with_suffix(""))
            else:
                item_id = file_path.stem
            try:
                if item_type == ItemType.DIRECTIVE:
                    result = await self._sign_directive(item_id, project_path, source)
                elif item_type == ItemType.TOOL:
                    result = await self._sign_tool(item_id, project_path, source)
                elif item_type == ItemType.KNOWLEDGE:
                    result = await self._sign_knowledge(item_id, project_path, source)
                else:
                    result = {"status": "error", "error": f"Unknown item type: {item_type}"}

                if result.get("status") == "error":
                    results["failed"].append({
                        "item": item_id,
                        "error": result.get("error"),
                        "details": result.get("issues", [])[:2],
                    })
                else:
                    results["signed"].append(item_id)
            except Exception as e:
                results["failed"].append({"item": item_id, "error": str(e)})

        results["summary"] = f"Signed {len(results['signed'])}/{results['total']} items"
        if results["failed"]:
            results["summary"] += f", {len(results['failed'])} failed"
        results["status"] = "completed"

        return results

    async def _sign_directive(
        self, item_id: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Validate and sign a directive."""
        file_path = self._find_item(project_path, source, ItemType.DIRECTIVE, item_id)
        if not file_path:
            return {
                "status": "error",
                "error": f"Directive not found: {item_id}",
                "hint": f"Create file at .ai/directives/{item_id}.md",
            }

        content = file_path.read_text(encoding="utf-8")

        # Parse content
        parsed = self.parser_router.parse("markdown/xml", content)
        if "error" in parsed:
            return {
                "status": "error",
                "error": "Invalid directive structure",
                "details": parsed.get("error"),
                "path": str(file_path),
            }

        # Schema-driven validation
        validation_result = validate_parsed_data(
            item_type=ItemType.DIRECTIVE,
            parsed_data=parsed,
            file_path=file_path,
            location=source,
            project_path=Path(project_path) if project_path else None,
        )

        if not validation_result["valid"]:
            return {
                "status": "error",
                "error": "Validation failed",
                "issues": validation_result["issues"],
                "path": str(file_path),
            }

        # Sign content
        signed_content = MetadataManager.sign_content(
            ItemType.DIRECTIVE,
            content,
            file_path=file_path,
            project_path=Path(project_path),
        )
        file_path.write_text(signed_content)

        sig_info = MetadataManager.get_signature_info(
            ItemType.DIRECTIVE, signed_content
        )

        return {
            "status": "signed",
            "item_id": item_id,
            "path": str(file_path),
            "location": source,
            "signature": sig_info,
            "warnings": validation_result.get("warnings", []),
            "message": "Directive validated and signed.",
        }

    async def _sign_tool(
        self, item_id: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Validate and sign a tool."""
        file_path = self._find_item(project_path, source, ItemType.TOOL, item_id)
        if not file_path:
            return {
                "status": "error",
                "error": f"Tool not found: {item_id}",
                "hint": f"Create file at .ai/tools/<category>/{item_id}.py",
            }

        content = file_path.read_text(encoding="utf-8")

        # Basic validation - file is not empty
        if not content.strip():
            return {
                "status": "error",
                "error": "Tool file is empty",
                "path": str(file_path),
            }

        # Parse tool file to extract metadata — data-driven dispatch by extension
        parsers_map = get_parsers_map(Path(project_path) if project_path else None)
        parser_name = parsers_map.get(file_path.suffix)
        if not parser_name:
            return {
                "status": "error",
                "error": f"No parser registered for extension: {file_path.suffix}",
                "path": str(file_path),
            }
        parsed = self.parser_router.parse(parser_name, content)
        if "error" in parsed:
            return {
                "status": "error",
                "error": "Failed to parse tool file",
                "details": parsed.get("error"),
                "path": str(file_path),
            }
        if file_path.suffix in (".yaml", ".yml"):
            parsed = parsed.get("data", parsed)

        # Add name from filename (required field derived from path)
        parsed["name"] = extract_filename(file_path)

        # Apply extraction rules to map dunder vars to standard field names
        parsed = apply_field_mapping(
            ItemType.TOOL,
            parsed,
            project_path=Path(project_path) if project_path else None,
        )

        # Schema-driven validation
        validation_result = validate_parsed_data(
            item_type=ItemType.TOOL,
            parsed_data=parsed,
            file_path=file_path,
            location=source,
            project_path=Path(project_path) if project_path else None,
        )

        if not validation_result["valid"]:
            return {
                "status": "error",
                "error": "Validation failed",
                "issues": validation_result["issues"],
                "path": str(file_path),
            }

        # Sign content
        signed_content = MetadataManager.sign_content(
            ItemType.TOOL, content, file_path=file_path, project_path=Path(project_path)
        )
        file_path.write_text(signed_content)

        # Invalidate stale lockfile — signing changes the integrity hash
        version = parsed.get("version", "0.0.0")
        try:
            from rye.executor.lockfile_resolver import LockfileResolver
            resolver = LockfileResolver(
                project_path=Path(project_path) if project_path else None,
            )
            if resolver.delete_lockfile(item_id, version):
                logger.info(f"Deleted stale lockfile for {item_id}@{version}")
        except Exception as e:
            logger.debug(f"Lockfile cleanup skipped: {e}")

        sig_info = MetadataManager.get_signature_info(
            ItemType.TOOL,
            signed_content,
            file_path=file_path,
            project_path=Path(project_path),
        )

        return {
            "status": "signed",
            "item_id": item_id,
            "path": str(file_path),
            "location": source,
            "signature": sig_info,
            "warnings": validation_result.get("warnings", []),
            "message": "Tool validated and signed.",
        }

    async def _sign_knowledge(
        self, item_id: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Validate and sign a knowledge entry."""
        file_path = self._find_item(project_path, source, ItemType.KNOWLEDGE, item_id)
        if not file_path:
            return {
                "status": "error",
                "error": f"Knowledge entry not found: {item_id}",
                "hint": f"Create file at .ai/knowledge/<category>/{item_id}.md",
            }

        content = file_path.read_text(encoding="utf-8")

        # Parse content
        parsed = self.parser_router.parse("markdown/frontmatter", content)
        if "error" in parsed:
            return {
                "status": "error",
                "error": "Invalid knowledge structure",
                "details": parsed.get("error"),
                "path": str(file_path),
            }

        # Schema-driven validation
        validation_result = validate_parsed_data(
            item_type=ItemType.KNOWLEDGE,
            parsed_data=parsed,
            file_path=file_path,
            location=source,
            project_path=Path(project_path) if project_path else None,
        )

        if not validation_result["valid"]:
            issues = validation_result["issues"]
            all_missing = issues and all(
                "Missing required field" in i for i in issues
            )
            error_msg = "Validation failed"
            if all_missing:
                error_msg = (
                    "No metadata found. Knowledge entries require a "
                    "```yaml fenced code block with name, title, version, "
                    "and entry_type fields."
                )
            return {
                "status": "error",
                "error": error_msg,
                "issues": issues,
                "path": str(file_path),
            }

        # Sign content
        signed_content = MetadataManager.sign_content(ItemType.KNOWLEDGE, content)
        file_path.write_text(signed_content)

        sig_info = MetadataManager.get_signature_info(
            ItemType.KNOWLEDGE, signed_content
        )

        return {
            "status": "signed",
            "item_id": item_id,
            "path": str(file_path),
            "location": source,
            "signature": sig_info,
            "warnings": validation_result.get("warnings", []),
            "message": "Knowledge entry validated and signed.",
        }

    def _find_item(
        self, project_path: str, source: str, item_type: str, item_id: str
    ) -> Optional[Path]:
        """Find item file by relative path ID in specified source location.
        
        Args:
            item_id: Relative path from .ai/<type>/ without extension.
                    e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
        """
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return None

        if source == "project":
            base = get_project_type_path(Path(project_path), item_type)
        elif source == "user":
            base = get_user_type_path(item_type)
        elif source == "system":
            extensions = get_item_extensions(item_type, Path(project_path) if project_path else None)

            type_folder = ItemType.TYPE_DIRS.get(item_type, item_type)
            for bundle in get_system_spaces():
                base = bundle.root_path / AI_DIR / type_folder
                if not base.exists():
                    continue
                for ext in extensions:
                    file_path = base / f"{item_id}{ext}"
                    if file_path.is_file():
                        return file_path
            return None
        else:
            return None

        if not base.exists():
            return None

        extensions = get_item_extensions(item_type, Path(project_path) if project_path else None)

        for ext in extensions:
            file_path = base / f"{item_id}{ext}"
            if file_path.is_file():
                return file_path

        return None
