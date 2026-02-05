"""Integrity hashing functions (Phase 1.2).

Provides deterministic SHA256 hashing for tools, directives, and knowledge.
Uses canonical JSON serialization (sorted keys, no whitespace) to ensure
the same input always produces the same hash.
"""

import hashlib
import json
from typing import Any, Dict, List, Optional


def _canonical_json(data: Any) -> str:
    """Serialize data to canonical JSON.

    Canonical form: sorted keys, no whitespace, consistent formatting.

    Args:
        data: Data to serialize.

    Returns:
        Canonical JSON string.
    """
    return json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=True)


def compute_tool_integrity(
    tool_id: str,
    version: str,
    manifest: Dict[str, Any],
    files: Optional[List[Dict[str, Any]]] = None,
) -> str:
    """Compute deterministic hash for a tool.

    The manifest should include 'name' and 'category' fields which
    are validated against the file path during verification.

    Args:
        tool_id: Tool identifier.
        version: Tool version (semver).
        manifest: Tool manifest dict (should include name, category).
        files: Optional list of file dicts with 'path' and 'sha256' keys.
               Files are sorted by path for deterministic hashing.

    Returns:
        SHA256 hex digest (64 chars).
    """
    data: Dict[str, Any] = {
        "tool_id": tool_id,
        "version": version,
        "manifest": manifest,
    }
    if files is not None:
        # Sort files by path for deterministic ordering
        sorted_files = sorted(files, key=lambda f: f.get("path", ""))
        data["files"] = sorted_files

    canonical = _canonical_json(data)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def compute_directive_integrity(
    directive_name: str,
    version: str,
    xml_content: str,
    metadata: Optional[Dict[str, Any]] = None,
) -> str:
    """Compute deterministic hash for a directive.

    The metadata should include 'name' and 'category' fields which
    are validated against the file path during verification.

    Args:
        directive_name: Directive name/ID.
        version: Directive version (semver).
        xml_content: XML content of the directive.
        metadata: Optional metadata dict (should include name, category).

    Returns:
        SHA256 hex digest (64 chars).
    """
    data: Dict[str, Any] = {
        "directive_name": directive_name,
        "version": version,
        "xml_content": xml_content,
    }
    if metadata is not None:
        data["metadata"] = metadata

    canonical = _canonical_json(data)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()


def compute_knowledge_integrity(
    id: str,
    version: str,
    content: str,
    metadata: Optional[Dict[str, Any]] = None,
) -> str:
    """Compute deterministic hash for knowledge entry.

    The metadata should include 'id' and 'category' fields which
    are validated against the file path during verification.

    Args:
        id: Knowledge entry ID (id).
        version: Entry version (semver).
        content: Entry content (markdown or text).
        metadata: Optional metadata dict (should include id, category).

    Returns:
        SHA256 hex digest (64 chars).
    """
    data: Dict[str, Any] = {
        "id": id,
        "version": version,
        "content": content,
    }
    if metadata is not None:
        data["metadata"] = metadata

    canonical = _canonical_json(data)
    return hashlib.sha256(canonical.encode("utf-8")).hexdigest()
