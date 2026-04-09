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
    get_project_kind_path,
    get_system_spaces,
    get_kind_folder,
    get_user_kind_path,
)
from rye.utils.extensions import (
    get_item_extensions,
    get_parsers_map,
    get_tool_extensions,
)
from rye.utils.resolvers import get_user_space
from rye.utils.validators import apply_field_mapping, validate_parsed_data

logger = logging.getLogger(__name__)


def _load_collect_config(
    project_path: Optional[Path] = None,
) -> tuple[frozenset, frozenset]:
    """Load exclude_dirs and exclude_files from collect.yaml via 3-tier resolution.

    Uses the same config the bundler uses so signing and bundling
    skip the same directories and files (node_modules, package.json, etc.).

    Returns:
        (exclude_dirs, exclude_files) as frozensets
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
                if isinstance(data, dict):
                    dirs = frozenset(data.get("exclude_dirs", []))
                    files = frozenset(data.get("exclude_files", []))
                    return dirs, files
            except Exception:
                continue

    return frozenset(), frozenset()


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
        self, kind: str, project_path: str, source: str
    ) -> Optional[Path]:
        """Get the base directory for computing relative path IDs."""
        if source == "project":
            return get_project_kind_path(Path(project_path), kind)
        elif source == "user":
            return get_user_kind_path(kind)
        elif source == "system":
            kind_folder = get_kind_folder(kind)
            for bundle in get_system_spaces():
                p = bundle.root_path / AI_DIR / kind_folder
                if p.exists():
                    return p
            return None
        return None

    def _resolve_glob_items(
        self, kind: str, pattern: str, project_path: str, source: str
    ) -> List[Path]:
        """Resolve glob pattern to list of item file paths."""
        kind_dir = ItemType.SIGNABLE_KINDS.get(kind)
        if not kind_dir:
            return []

        if source == "project":
            base_dir = get_project_kind_path(Path(project_path), kind)
        elif source == "user":
            base_dir = get_user_kind_path(kind)
        elif source == "system":
            all_items = []
            kind_folder = ItemType.SIGNABLE_KINDS.get(kind, kind)
            for bundle in get_system_spaces():
                sys_dir = bundle.root_path / AI_DIR / kind_folder
                if not sys_dir.exists():
                    continue
                all_items.extend(
                    self._glob_in_dir(sys_dir, kind, pattern, project_path)
                )
            return all_items
        else:
            return []

        if not base_dir.exists():
            return []

        return self._glob_in_dir(base_dir, kind, pattern, project_path)

    def _glob_in_dir(
        self, base_dir: Path, kind: str, pattern: str, project_path: str
    ) -> List[Path]:
        """Glob for items inside a single base directory.

        Skips paths matching collect.yaml's exclude_dirs and exclude_files
        to avoid picking up third-party vendored files that aren't Rye items.
        """
        exclude_dirs, exclude_files = _load_collect_config(
            Path(project_path) if project_path else None
        )

        def _is_included(path: Path) -> bool:
            return (
                path.is_file()
                and not (exclude_dirs & set(path.parts))
                and path.name not in exclude_files
            )

        proj = Path(project_path) if project_path else None
        if kind == ItemType.TOOL:
            extensions = get_tool_extensions(proj)
        elif kind == ItemType.CONFIG:
            extensions = get_item_extensions(ItemType.CONFIG, proj)
        elif kind in (ItemType.DIRECTIVE, ItemType.KNOWLEDGE):
            extensions = get_item_extensions(kind, proj)
        else:
            raise ValueError(f"Unknown item type for glob: {kind}")

        items = []
        for ext in extensions:
            if "/" in pattern:
                glob_pattern = (
                    f"{pattern}{ext}" if not pattern.endswith(ext) else pattern
                )
            else:
                glob_pattern = f"**/{pattern}{ext}" if pattern != "*" else f"**/*{ext}"
            for path in base_dir.glob(glob_pattern):
                if _is_included(path):
                    items.append(path)
        return items

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle sign request."""
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        source = kwargs.get("source", "project")

        # Parse canonical ref (e.g. "tool:rye/bash/bash" → kind="tool", bare_id="rye/bash/bash")
        ref_kind, bare_id = ItemType.parse_canonical_ref(item_id)

        # Derive kind strictly from canonical ref
        kind: Optional[str] = ref_kind
        if ref_kind:
            item_id = bare_id

        if not kind:
            return {
                "status": "error",
                "error": (
                    "item_id must use a canonical ref prefix to identify the type "
                    "(e.g. 'tool:my/item', 'directive:my/workflow', 'knowledge:my/entry', "
                    "'config:my/config')."
                ),
                "item_id": item_id,
            }

        logger.debug(f"Sign: kind={kind}, item_id={item_id}, source={source}")

        if source == "system":
            return {
                "status": "error",
                "error": "System space items are immutable and cannot be signed. Copy to project or user space first.",
                "item_id": item_id,
            }

        try:
            # Check for batch signing (glob pattern)
            if self._is_glob_pattern(item_id):
                return await self._sign_batch(kind, item_id, project_path, source)

            if kind == ItemType.DIRECTIVE:
                return await self._sign_directive(item_id, project_path, source)
            elif kind == ItemType.TOOL:
                return await self._sign_tool(item_id, project_path, source)
            elif kind == ItemType.KNOWLEDGE:
                return await self._sign_knowledge(item_id, project_path, source)
            elif kind == "config":
                return await self._sign_config(item_id, project_path, source)
            else:
                return {"status": "error", "error": f"Unknown item type: {kind}"}

        except Exception as e:
            logger.error(f"Sign error: {e}")
            return {"status": "error", "error": str(e), "item_id": item_id}

    async def _sign_batch(
        self, kind: str, pattern: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Sign multiple items matching a glob pattern."""
        items = self._resolve_glob_items(kind, pattern, project_path, source)

        if not items:
            return {
                "status": "error",
                "error": f"No {kind}s found matching pattern: {pattern}",
                "searched_in": str(
                    get_project_kind_path(Path(project_path), kind)
                    if source == "project"
                    else get_user_kind_path(kind)
                ),
                "hint": f"Ensure project_path is the parent of the {AI_DIR}/ directory containing this item.",
            }

        results: Dict[str, Any] = {"signed": [], "failed": [], "total": len(items)}

        base_dir = self._get_batch_base_dir(kind, project_path, source)
        for file_path in items:
            if base_dir:
                item_id = str(file_path.relative_to(base_dir).with_suffix(""))
            else:
                item_id = file_path.stem
            try:
                if kind == ItemType.DIRECTIVE:
                    result = await self._sign_directive(item_id, project_path, source)
                elif kind == ItemType.TOOL:
                    result = await self._sign_tool(item_id, project_path, source)
                elif kind == ItemType.KNOWLEDGE:
                    result = await self._sign_knowledge(item_id, project_path, source)
                elif kind == "config":
                    result = await self._sign_config(item_id, project_path, source)
                else:
                    result = {
                        "status": "error",
                        "error": f"Unknown item type: {kind}",
                    }

                if result.get("status") == "error":
                    results["failed"].append(
                        {
                            "item": item_id,
                            "error": result.get("error"),
                            "details": result.get("issues", [])[:2],
                        }
                    )
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
                "searched_in": str(Path(project_path) / AI_DIR / "directives"),
                "hint": f"Ensure project_path is the parent of the {AI_DIR}/ directory containing this item.",
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
            kind=ItemType.DIRECTIVE,
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
                "searched_in": str(Path(project_path) / AI_DIR / "tools"),
                "hint": f"Ensure project_path is the parent of the {AI_DIR}/ directory containing this item.",
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
            kind=ItemType.TOOL,
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
                "searched_in": str(Path(project_path) / AI_DIR / "knowledge"),
                "hint": f"Ensure project_path is the parent of the {AI_DIR}/ directory containing this item.",
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
            kind=ItemType.KNOWLEDGE,
            parsed_data=parsed,
            file_path=file_path,
            location=source,
            project_path=Path(project_path) if project_path else None,
        )

        if not validation_result["valid"]:
            issues = validation_result["issues"]
            all_missing = issues and all("Missing required field" in i for i in issues)
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

    async def _sign_config(
        self, item_id: str, project_path: str, source: str
    ) -> Dict[str, Any]:
        """Validate and sign a config file.

        Two-phase validation:
        1. Metadata validation via config_extractor validation_schema
           (name, category, version, description)
        2. Content validation via .config-schema.yaml tools
           (structural shape of config values)
        """
        proj = Path(project_path) if project_path else None
        if source == "project" and proj:
            base = proj / AI_DIR / "config"
        elif source == "user":
            base = Path(get_user_space()) / AI_DIR / "config"
        else:
            return {
                "status": "error",
                "error": f"Config signing not supported for source: {source}",
            }

        # item_id is the relative path under .ai/config/ without extension
        config_extensions = get_item_extensions("config", proj)
        file_path = None
        for ext in config_extensions:
            candidate = base / f"{item_id}{ext}"
            if candidate.is_file():
                file_path = candidate
                break

        if not file_path:
            return {
                "status": "error",
                "error": f"Config not found: {item_id}",
                "searched_in": str(base),
            }

        content = file_path.read_text(encoding="utf-8")
        if not content.strip():
            return {
                "status": "error",
                "error": "Config file is empty",
                "path": str(file_path),
            }

        # Parse via parser router — data-driven dispatch by extension
        parsers_map = get_parsers_map(proj)
        parser_name = parsers_map.get(file_path.suffix)
        if not parser_name:
            # Fallback: YAML for .yaml/.yml, TOML for .toml
            fallback = {".yaml": "yaml/yaml", ".yml": "yaml/yaml", ".toml": "toml/toml"}
            parser_name = fallback.get(file_path.suffix)
        if not parser_name:
            return {
                "status": "error",
                "error": f"No parser registered for extension: {file_path.suffix}",
                "path": str(file_path),
            }

        warnings: list = []
        parsed = self.parser_router.parse(parser_name, content)
        if "error" in parsed:
            return {
                "status": "error",
                "error": "Failed to parse config file",
                "details": parsed.get("error"),
                "path": str(file_path),
            }
        parsed = parsed.get("data", parsed)

        # Add name from filename
        parsed["name"] = extract_filename(file_path)

        # Apply extraction rules (category, version, description from top-level keys)
        parsed = apply_field_mapping(
            "config",
            parsed,
            project_path=proj,
        )

        # Phase 1: Metadata validation (name, category, version, description)
        metadata_result = validate_parsed_data(
            kind="config",
            parsed_data=parsed,
            file_path=file_path,
            location=source,
            project_path=proj,
        )

        # Phase 2: Content validation (structural shape)
        from rye.utils.config_validators import validate_config_content

        content_result = validate_config_content(
            config_id=item_id,
            config_data=parsed,
            project_path=proj,
        )

        issues = metadata_result["issues"] + content_result["issues"]
        warnings = metadata_result.get("warnings", []) + content_result.get(
            "warnings", []
        )

        if issues:
            return {
                "status": "error",
                "error": "Validation failed",
                "issues": issues,
                "path": str(file_path),
            }

        # Sign content
        signed_content = MetadataManager.sign_content(
            "config",
            content,
            file_path=file_path,
            project_path=proj,
        )
        file_path.write_text(signed_content)

        sig_info = MetadataManager.get_signature_info(
            "config",
            signed_content,
            file_path=file_path,
            project_path=proj,
        )

        return {
            "status": "signed",
            "item_id": item_id,
            "path": str(file_path),
            "location": source,
            "signature": sig_info,
            "warnings": warnings,
            "message": "Config validated and signed.",
        }

    def _find_item(
        self, project_path: str, source: str, kind: str, item_id: str
    ) -> Optional[Path]:
        """Find item file by relative path ID in specified source location.

        Args:
            item_id: Relative path from .ai/<type>/ without extension.
                    e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
        """
        kind_dir = ItemType.SIGNABLE_KINDS.get(kind)
        if not kind_dir:
            return None

        if source == "project":
            base = get_project_kind_path(Path(project_path), kind)
        elif source == "user":
            base = get_user_kind_path(kind)
        elif source == "system":
            extensions = get_item_extensions(
                kind, Path(project_path) if project_path else None
            )

            kind_folder = ItemType.SIGNABLE_KINDS.get(kind, kind)
            for bundle in get_system_spaces():
                base = bundle.root_path / AI_DIR / kind_folder
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

        extensions = get_item_extensions(
            kind, Path(project_path) if project_path else None
        )

        for ext in extensions:
            file_path = base / f"{item_id}{ext}"
            if file_path.is_file():
                return file_path

        return None
