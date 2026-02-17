"""Load tool - load item content for inspection or copy between locations."""

import logging
import shutil
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import ItemType, AI_DIR
from rye.utils.path_utils import get_project_type_path, get_system_spaces, get_user_space
from rye.utils.extensions import get_tool_extensions, get_item_extensions
from rye.utils.integrity import verify_item

logger = logging.getLogger(__name__)


class LoadTool:
    """Load item content or copy items between locations."""

    def __init__(self, user_space: Optional[str] = None):
        """Initialize load tool."""
        self.user_space = user_space or str(get_user_space())

    async def handle(self, **kwargs) -> Dict[str, Any]:
        """Handle load request."""
        item_type: str = kwargs["item_type"]
        item_id: str = kwargs["item_id"]
        project_path = kwargs["project_path"]
        source = kwargs.get("source")
        destination = kwargs.get("destination")

        logger.debug(f"Load: item_type={item_type}, item_id={item_id}, source={source}")

        try:
            if source:
                # Explicit source — search only that space
                source_path = self._find_item(project_path, source, item_type, item_id)
                resolved_source = source
            else:
                # No source specified — cascade project → user → system
                source_path = None
                resolved_source = "project"
                for try_source in ("project", "user", "system"):
                    source_path = self._find_item(project_path, try_source, item_type, item_id)
                    if source_path and source_path.exists():
                        resolved_source = try_source
                        break
                    source_path = None

            if not source_path or not source_path.exists():
                return {
                    "status": "error",
                    "error": f"Item not found: {item_id}",
                    "item_type": item_type,
                    "item_id": item_id,
                }

            verify_item(
                source_path, item_type,
                project_path=Path(project_path) if project_path else None,
            )

            content = source_path.read_text(encoding="utf-8")
            metadata = self._extract_metadata(source_path, content)

            result = {
                "status": "success",
                "content": content,
                "metadata": metadata,
                "path": str(source_path),
                "source": resolved_source,
            }

            # Copy if destination differs from source
            if destination and destination != resolved_source:
                dest_path = self._resolve_destination(
                    project_path, destination, item_type, source_path,
                    item_id=item_id,
                )
                if dest_path:
                    dest_path.parent.mkdir(parents=True, exist_ok=True)
                    shutil.copy(source_path, dest_path)
                    result["copied_to"] = destination
                    result["destination_path"] = str(dest_path)

            return result

        except Exception as e:
            logger.error(f"Load error: {e}")
            return {"status": "error", "error": str(e), "item_id": item_id}

    def _find_item(
        self, project_path: str, source: str, item_type: str, item_id: str
    ) -> Optional[Path]:
        """Find item file by relative path ID in specified source.
        
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
            base = Path(self.user_space) / AI_DIR / type_dir
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

    def _resolve_destination(
        self, project_path: str, destination: str, item_type: str,
        source_path: Path, item_id: str = "",
    ) -> Optional[Path]:
        """Resolve destination path preserving item_id structure.

        Uses item_id (e.g. "rye/core/system") to reconstruct the full
        path under the destination type root, preserving category dirs.
        Falls back to filename-only if item_id is not provided.
        """
        type_dir = ItemType.TYPE_DIRS.get(item_type)
        if not type_dir:
            return None

        if destination == "project":
            base = get_project_type_path(Path(project_path), item_type)
        elif destination == "user":
            base = Path(self.user_space) / AI_DIR / type_dir
        else:
            return None

        if item_id:
            return base / f"{item_id}{source_path.suffix}"
        return base / source_path.name

    def _extract_metadata(self, file_path: Path, content: str) -> Dict[str, Any]:
        """Extract basic metadata from file."""
        import re

        metadata = {
            "name": file_path.stem,
            "path": str(file_path),
            "extension": file_path.suffix,
        }

        # Extract version if present
        if "__version__" in content:
            match = re.search(r'__version__\s*=\s*["\']([^"\']+)["\']', content)
            if match:
                metadata["version"] = match.group(1)
        elif 'version="' in content:
            match = re.search(r'version="([^"]+)"', content)
            if match:
                metadata["version"] = match.group(1)

        return metadata
