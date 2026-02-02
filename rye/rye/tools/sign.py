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

from rye.utils.resolvers import get_user_space
from rye.utils.metadata_manager import MetadataManager
from rye.utils.parser_router import ParserRouter
from rye.utils.path_utils import (
    extract_filename,
    get_system_space,
    get_project_type_path,
    get_user_type_path,
    get_system_type_path,
)
from rye.utils.validators import validate_parsed_data
from rye.constants import ItemType

logger = logging.getLogger(__name__)


class SignTool:
    """Validate and sign directives, tools, or knowledge entries."""

    def __init__(self, user_space: Optional[str] = None):
        """Initialize sign tool."""
        self.user_space = user_space or str(get_user_space())
        self.parser_router = ParserRouter()

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle sign request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        source = kwargs.get("source", "project")

        logger.debug(f"Sign: item_type={item_type}, item_id={item_id}, source={source}")

        try:
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
        parsed = self.parser_router.parse("markdown_xml", content)
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

        # For tools: name is derived from filename
        filename = extract_filename(file_path)
        parsed = {"name": filename}

        # Schema-driven validation (minimal for tools)
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
        parsed = self.parser_router.parse("markdown_frontmatter", content)
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
            return {
                "status": "error",
                "error": "Validation failed",
                "issues": validation_result["issues"],
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
        """Find item file in specified source location."""
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return None

        if source == "project":
            base = get_project_type_path(Path(project_path), item_type)
        elif source == "user":
            base = get_user_type_path(item_type)
        elif source == "system":
            base = get_system_type_path(item_type)
        else:
            return None

        if not base.exists():
            return None

        for file_path in base.rglob(f"{item_id}*"):
            if file_path.is_file() and file_path.stem == item_id:
                return file_path

        return None
