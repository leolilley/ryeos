"""Server-side validation using rye validators.

This module imports and uses the same validation pipeline as the client-side
rye package, ensuring consistent validation rules.

The rye package provides:
- ParserRouter: Data-driven parsing (markdown_xml, python_ast, etc.)
- MetadataManager: Signature handling per item type
- validators: Schema-driven validation using extractors
- constants: ItemType definitions and mappings
"""

import logging
from typing import Any, Dict, Optional, Tuple

# Import directly from rye package - single source of truth
from rye.constants import ItemType
from rye.utils.metadata_manager import (
    MetadataManager,
    compute_content_hash,
    generate_timestamp,
)
from rye.utils.parser_router import ParserRouter
from rye.utils.validators import apply_field_mapping, validate_parsed_data

logger = logging.getLogger(__name__)

# Parser router instance (reused across requests)
_parser_router: Optional[ParserRouter] = None


def get_parser_router() -> ParserRouter:
    """Get or create parser router instance."""
    global _parser_router
    if _parser_router is None:
        _parser_router = ParserRouter()
    return _parser_router


# Item type to parser type mapping
# This follows the extractor pattern - each item type has a parser
PARSER_TYPES = {
    ItemType.DIRECTIVE: "markdown_xml",
    ItemType.TOOL: "python_ast",
    ItemType.KNOWLEDGE: "markdown_yaml",
}


def strip_signature(content: str, item_type: str) -> str:
    """Remove existing signature from content.

    Uses MetadataManager.get_strategy() which handles item-type-specific
    signature formats (HTML comments for directives/knowledge, line comments for tools).
    """
    strategy = MetadataManager.get_strategy(item_type)
    return strategy.remove_signature(content)


def validate_content(
    content: str,
    item_type: str,
    item_id: str,
) -> Tuple[bool, Dict[str, Any]]:
    """Validate content using rye validators.

    Uses the same validation pipeline as the client-side sign tool:
    1. Parse with ParserRouter (data-driven)
    2. Apply field mapping from extractors
    3. Validate against VALIDATION_SCHEMA from extractors

    Args:
        content: File content (signature already stripped)
        item_type: Type of item (directive, tool, knowledge)
        item_id: Item identifier (used for name validation)

    Returns:
        Tuple of (is_valid, result_dict)
        result_dict contains either parsed_data or issues list
    """
    parser_router = get_parser_router()
    parser_type = PARSER_TYPES.get(item_type)

    if not parser_type:
        return False, {"issues": [f"Unknown item type: {item_type}"]}

    # Parse content using data-driven parser
    try:
        parsed = parser_router.parse(parser_type, content)
    except Exception as e:
        logger.warning(f"Parse error for {item_type}/{item_id}: {e}")
        return False, {"issues": [f"Failed to parse content: {str(e)}"]}

    if "error" in parsed:
        return False, {"issues": [f"Parse error: {parsed['error']}"]}

    # For tools, add name from item_id (matches client behavior)
    if item_type == ItemType.TOOL:
        parsed["name"] = item_id

    # Apply field mapping from extractors (e.g., __version__ -> version)
    parsed = apply_field_mapping(item_type, parsed)

    # Validate using VALIDATION_SCHEMA from extractors
    validation_result = validate_parsed_data(
        item_type=item_type,
        parsed_data=parsed,
        file_path=None,  # No file path on server
        location="registry",
        project_path=None,
    )

    if not validation_result["valid"]:
        return False, {"issues": validation_result["issues"]}

    return True, {
        "parsed_data": parsed,
        "warnings": validation_result.get("warnings", []),
    }


def sign_with_registry(
    content: str,
    item_type: str,
    username: str,
) -> Tuple[str, Dict[str, str]]:
    """Sign content with registry provenance.

    Adds the |registry@username suffix that can only be added server-side.
    Uses MetadataManager for item-type-specific signature formatting.

    Args:
        content: Content to sign (should have any existing signature stripped)
        item_type: Type of item (directive, tool, knowledge)
        username: Authenticated user's username

    Returns:
        Tuple of (signed_content, signature_info)
        signature_info contains timestamp, hash, registry_username
    """
    # Use rye's hash/timestamp functions for consistency
    content_hash = compute_content_hash(content)
    timestamp = generate_timestamp()

    # Get strategy for this item type
    strategy = MetadataManager.get_strategy(item_type)

    # Format the base signature using MetadataManager
    base_signature = strategy.format_signature(timestamp, content_hash)

    # Inject registry suffix into the signature
    # For HTML comments: "<!-- rye:validated:T:H -->" -> "<!-- rye:validated:T:H|registry@user -->"
    # For line comments: "# rye:validated:T:H\n" -> "# rye:validated:T:H|registry@user\n"
    if base_signature.endswith(" -->\n"):
        registry_signature = base_signature.replace(
            " -->", f"|registry@{username} -->"
        )
    elif base_signature.endswith("\n"):
        registry_signature = base_signature.rstrip("\n") + f"|registry@{username}\n"
    else:
        registry_signature = base_signature + f"|registry@{username}"

    # Insert signature into content
    signed_content = strategy.insert_signature(content, registry_signature)

    signature_info = {
        "timestamp": timestamp,
        "hash": content_hash,
        "registry_username": username,
    }

    return signed_content, signature_info


def verify_registry_signature(
    content: str,
    item_type: str,
    expected_author: str,
) -> Tuple[bool, Optional[str], Optional[Dict[str, str]]]:
    """Verify a registry signature on pulled content.

    Args:
        content: Content with registry signature
        item_type: Type of item
        expected_author: Author username from database

    Returns:
        Tuple of (is_valid, error_message, signature_info)
    """
    strategy = MetadataManager.get_strategy(item_type)

    # Extract signature using MetadataManager
    sig_info = strategy.extract_signature(content)
    if not sig_info:
        return False, "No signature found", None

    # Verify it's a registry signature
    registry_username = sig_info.get("registry_username")
    if not registry_username:
        return False, "Not a registry signature (missing |registry@username)", sig_info

    # Verify username matches expected author
    if registry_username != expected_author:
        return (
            False,
            f"Username mismatch: signature says {registry_username}, expected {expected_author}",
            sig_info,
        )

    # Verify content hash
    content_without_sig = strategy.remove_signature(content)
    computed_hash = compute_content_hash(content_without_sig)

    if computed_hash != sig_info["hash"]:
        return False, "Content integrity check failed: hash mismatch", sig_info

    return True, None, sig_info
