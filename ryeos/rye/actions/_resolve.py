"""Item resolution by ID — find, read, copy, extract metadata."""

import logging
import re
import shutil
from pathlib import Path
from typing import Any, Dict, Optional

from rye.constants import ItemType, AI_DIR
from rye.utils.execution_context import ExecutionContext
from rye.utils.path_utils import get_project_kind_path, get_system_spaces
from rye.utils.extensions import get_tool_extensions, get_item_extensions
from rye.utils.integrity import verify_item, IntegrityError
from rye.utils.registry_providers import get_registry_provider

logger = logging.getLogger(__name__)


async def resolve_item(user_space: str, **kwargs) -> Dict[str, Any]:
    """Resolve an item by ref — find, read, optionally copy.

    This is the main entry point for ID-based resolution.

    Args:
        user_space: Path to the user's home directory.
        **kwargs: item_ref, project_path, source, destination, version.
    """
    item_ref: str = kwargs["item_ref"]
    kind, bare_id = ItemType.require_canonical_ref(item_ref)
    project_path = kwargs["project_path"]
    source = kwargs.get("source")
    destination = kwargs.get("destination")

    logger.debug(f"Load: kind={kind}, bare_id={bare_id}, source={source}")

    try:
        # Remote source — delegate to provider
        if source == "registry":
            return await _load_from_remote(
                user_space, "registry", kind, bare_id, project_path, destination,
                version=kwargs.get("version"),
            )

        if source:
            # Explicit local source — search only that space
            source_path = find_item(user_space, project_path, source, kind, bare_id)
            resolved_source = source
        else:
            # No source specified — cascade project → user → system
            source_path = None
            resolved_source = "project"
            for try_source in ("project", "user", "system"):
                source_path = find_item(user_space, project_path, try_source, kind, bare_id)
                if source_path and source_path.exists():
                    resolved_source = try_source
                    break
                source_path = None

        if not source_path or not source_path.exists():
            return {
                "status": "error",
                "error": f"Item not found: {bare_id}",
                "item_ref": item_ref,
            }

        verify_item(
            source_path, kind,
            ctx=ExecutionContext.from_env(
                project_path=Path(project_path) if project_path else None,
            ),
        )

        content = source_path.read_text(encoding="utf-8")
        metadata = _extract_metadata(source_path, content)

        result = {
            "status": "success",
            "content": content,
            "metadata": metadata,
            "path": str(source_path),
            "source": resolved_source,
        }

        # Copy if destination differs from source
        if destination and destination != resolved_source:
            dest_path = _resolve_destination(
                user_space, project_path, destination, kind, source_path,
                bare_id=bare_id,
            )
            if dest_path:
                dest_path.parent.mkdir(parents=True, exist_ok=True)
                shutil.copy(source_path, dest_path)
                result["copied_to"] = destination
                result["destination_path"] = str(dest_path)

        return result

    except IntegrityError as e:
        logger.error(f"Integrity error: {e}")
        return {"status": "error", "error": str(e), "error_type": "integrity", "item_ref": item_ref}
    except Exception as e:
        logger.error(f"Load error: {e}")
        return {"status": "error", "error": str(e), "item_ref": item_ref}


async def _load_from_remote(
    user_space: str,
    provider_id: str,
    kind: str,
    bare_id: str,
    project_path: str,
    destination: Optional[str] = None,
    version: Optional[str] = None,
) -> Dict[str, Any]:
    """Load item from a remote space provider.

    Pulls content and metadata from the remote provider. If a local
    destination is specified (project/user), writes the content to disk.
    """
    provider = get_registry_provider(provider_id)
    item_ref = f"{kind}:{bare_id}"
    if not provider:
        return {
            "status": "error",
            "error": f"Remote provider not found: {provider_id}",
            "item_ref": item_ref,
        }

    result = await provider.pull(
        kind=kind,
        bare_id=bare_id,
        version=version,
    )

    if result.get("error"):
        return {
            "status": "error",
            "error": result["error"],
            "item_ref": item_ref,
        }

    content = result.get("content", "")
    metadata = result.get("metadata", {})

    load_result = {
        "status": "success",
        "content": content,
        "metadata": metadata,
        "source": provider_id,
        "item_ref": item_ref,
        "version": result.get("version", ""),
    }

    # Write to local destination if requested
    if destination in ("project", "user"):
        dest_path = _resolve_remote_destination(
            user_space, project_path, destination, kind, bare_id,
        )
        if dest_path:
            dest_path.parent.mkdir(parents=True, exist_ok=True)
            dest_path.write_text(content, encoding="utf-8")
            load_result["copied_to"] = destination
            load_result["destination_path"] = str(dest_path)

    return load_result


def _resolve_remote_destination(
    user_space: str,
    project_path: str,
    destination: str,
    kind: str,
    bare_id: str,
) -> Optional[Path]:
    """Resolve destination path for a remote item.

    Uses the bare_id's last segment as filename and category from
    the middle segments to reconstruct the local path.
    """
    type_dir = ItemType.KIND_DIRS.get(kind)
    if not type_dir:
        return None

    if destination == "project":
        base = get_project_kind_path(Path(project_path), kind)
    elif destination == "user":
        base = Path(user_space) / AI_DIR / type_dir
    else:
        return None

    # Determine extension from item type
    ext = ".md" if kind in ("directive", "knowledge") else ".py"

    # For registry items, bare_id is namespace/category/name —
    # strip the namespace prefix and use category/name for local path
    segments = bare_id.split("/")
    if len(segments) >= 3:
        # Drop namespace, keep category/name
        local_path = "/".join(segments[1:])
    else:
        local_path = bare_id

    return base / f"{local_path}{ext}"


def find_item(
    user_space: str, project_path: str, source: str, kind: str, bare_id: str
) -> Optional[Path]:
    """Find item file by relative path ID in specified source.

    Args:
        user_space: Path to the user's home directory.
        project_path: Path to the project root.
        source: Space to search in (project, user, system).
        kind: Type of item (directive, tool, knowledge).
        bare_id: Relative path from .ai/<type>/ without extension.
                e.g., "rye/core/registry/registry" -> .ai/tools/rye/core/registry/registry.py
    """
    type_dir = ItemType.KIND_DIRS.get(kind)
    if not type_dir:
        return None

    if source == "project":
        base = get_project_kind_path(Path(project_path), kind)
    elif source == "user":
        base = Path(user_space) / AI_DIR / type_dir
    elif source == "system":
        extensions = get_item_extensions(kind, Path(project_path) if project_path else None)

        type_folder = ItemType.KIND_DIRS.get(kind, kind)
        for bundle in get_system_spaces():
            base = bundle.root_path / AI_DIR / type_folder
            if not base.exists():
                continue
            for ext in extensions:
                file_path = base / f"{bare_id}{ext}"
                if file_path.is_file():
                    return file_path
        return None
    else:
        return None

    if not base.exists():
        return None

    extensions = get_item_extensions(kind, Path(project_path) if project_path else None)

    for ext in extensions:
        file_path = base / f"{bare_id}{ext}"
        if file_path.is_file():
            return file_path

    return None


def _resolve_destination(
    user_space: str, project_path: str, destination: str, kind: str,
    source_path: Path, bare_id: str = "",
) -> Optional[Path]:
    """Resolve destination path preserving bare_id structure.

    Uses bare_id (e.g. "rye/core/system") to reconstruct the full
    path under the destination type root, preserving category dirs.
    Falls back to filename-only if bare_id is not provided.
    """
    type_dir = ItemType.KIND_DIRS.get(kind)
    if not type_dir:
        return None

    if destination == "project":
        base = get_project_kind_path(Path(project_path), kind)
    elif destination == "user":
        base = Path(user_space) / AI_DIR / type_dir
    else:
        return None

    if bare_id:
        return base / f"{bare_id}{source_path.suffix}"
    return base / source_path.name


def _extract_metadata(file_path: Path, content: str) -> Dict[str, Any]:
    """Extract basic metadata from file."""
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
