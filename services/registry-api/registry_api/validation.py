"""Server-side validation using rye validators.

This module imports and uses the same validation pipeline as the client-side
rye package, ensuring consistent validation rules.

The rye package provides:
- ParserRouter: Data-driven parsing (markdown_xml, python_ast, etc.)
- MetadataManager: Signature handling per item type
- validators: Schema-driven validation using extractors
- constants: ItemType definitions and mappings

Signature format: rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
Registry appends provenance: |rye-registry@username
"""

import logging
from pathlib import Path
from typing import Any, Dict, Optional, Tuple

from rye.constants import ItemType
from rye.utils.metadata_manager import (
    MetadataManager,
    compute_content_hash,
    generate_timestamp,
)
from rye.utils.parser_router import ParserRouter
from rye.utils.validators import apply_field_mapping, validate_parsed_data

logger = logging.getLogger(__name__)

_parser_router: Optional[ParserRouter] = None


def get_parser_router() -> ParserRouter:
    """Get or create parser router instance."""
    global _parser_router
    if _parser_router is None:
        _parser_router = ParserRouter()
    return _parser_router


PARSER_TYPES = {
    ItemType.DIRECTIVE: "markdown/xml",
    ItemType.TOOL: "python/ast",
    ItemType.KNOWLEDGE: "markdown/frontmatter",
}


def strip_signature(content: str, item_type: str) -> str:
    """Remove existing signature from content."""
    strategy = MetadataManager.get_strategy(item_type)
    return strategy.remove_signature(content)


def validate_content(
    content: str,
    item_type: str,
    item_id: str,
) -> Tuple[bool, Dict[str, Any]]:
    """Validate content using rye validators."""
    parser_router = get_parser_router()
    parser_type = PARSER_TYPES.get(item_type)

    if not parser_type:
        return False, {"issues": [f"Unknown item type: {item_type}"]}

    try:
        parsed = parser_router.parse(parser_type, content)
    except Exception as e:
        logger.warning(f"Parse error for {item_type}/{item_id}: {e}")
        return False, {"issues": [f"Failed to parse content: {str(e)}"]}

    if "error" in parsed:
        return False, {"issues": [f"Parse error: {parsed['error']}"]}

    if item_type == ItemType.TOOL:
        parsed["name"] = item_id

    parsed = apply_field_mapping(item_type, parsed)

    validation_result = validate_parsed_data(
        item_type=item_type,
        parsed_data=parsed,
        file_path=None,
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
    """Sign content with registry Ed25519 key and provenance.

    The registry server has its own Ed25519 keypair. On push, the server
    strips the client's signature, re-signs with the registry key, and
    appends |rye-registry@username provenance.

    Key directory from REGISTRY_KEY_DIR env var (default: /etc/rye-registry/keys).

    Args:
        content: Content to sign (should have any existing signature stripped)
        item_type: Type of item (directive, tool, knowledge)
        username: Authenticated user's username

    Returns:
        Tuple of (signed_content, signature_info)
    """
    import os

    from lilux.primitives.signing import (
        ensure_keypair,
        sign_hash,
        compute_key_fingerprint,
    )

    strategy = MetadataManager.get_strategy(item_type)
    content_for_hash = strategy.extract_content_for_hash(content)
    content_hash = compute_content_hash(content_for_hash)
    timestamp = generate_timestamp()

    registry_key_dir = Path(os.environ.get(
        "REGISTRY_KEY_DIR", "/etc/rye-registry/keys"
    ))

    private_pem, public_pem = ensure_keypair(registry_key_dir)
    ed25519_sig = sign_hash(content_hash, private_pem)
    pubkey_fp = compute_key_fingerprint(public_pem)

    base_signature = strategy.format_signature(
        timestamp, content_hash, ed25519_sig, pubkey_fp
    )

    if base_signature.endswith(" -->\n"):
        registry_signature = base_signature.replace(
            " -->", f"|rye-registry@{username} -->"
        )
    elif base_signature.endswith("\n"):
        registry_signature = (
            base_signature.rstrip("\n") + f"|rye-registry@{username}\n"
        )
    else:
        registry_signature = base_signature + f"|rye-registry@{username}"

    signed_content = strategy.insert_signature(content, registry_signature)

    signature_info = {
        "timestamp": timestamp,
        "hash": content_hash,
        "ed25519_sig": ed25519_sig,
        "pubkey_fp": pubkey_fp,
        "registry_username": username,
    }

    return signed_content, signature_info


def get_registry_public_key() -> Optional[bytes]:
    """Get the registry's public key for the /v1/public-key endpoint.

    Key directory from REGISTRY_KEY_DIR env var (default: /etc/rye-registry/keys).

    Returns:
        Public key PEM bytes, or None
    """
    import os
    from lilux.primitives.signing import ensure_keypair

    registry_key_dir = Path(os.environ.get(
        "REGISTRY_KEY_DIR", "/etc/rye-registry/keys"
    ))

    try:
        _, public_pem = ensure_keypair(registry_key_dir)
        return public_pem
    except Exception:
        return None


def verify_registry_signature(
    content: str,
    item_type: str,
    expected_author: str,
) -> Tuple[bool, Optional[str], Optional[Dict[str, str]]]:
    """Verify a registry Ed25519 signature on pulled content.

    Args:
        content: Content with registry signature
        item_type: Type of item
        expected_author: Author username from database

    Returns:
        Tuple of (is_valid, error_message, signature_info)
    """
    strategy = MetadataManager.get_strategy(item_type)

    sig_info = strategy.extract_signature(content)
    if not sig_info:
        return False, "No signature found", None

    registry_username = sig_info.get("registry_username")
    if not registry_username:
        return False, "Not a registry signature (missing |registry@username)", sig_info

    if registry_username != expected_author:
        return (
            False,
            f"Username mismatch: signature says {registry_username}, expected {expected_author}",
            sig_info,
        )

    content_without_sig = strategy.remove_signature(content)
    computed_hash = compute_content_hash(content_without_sig)

    if computed_hash != sig_info["hash"]:
        return False, "Content integrity check failed: hash mismatch", sig_info

    ed25519_sig = sig_info.get("ed25519_sig")
    pubkey_fp = sig_info.get("pubkey_fp")
    if not ed25519_sig or not pubkey_fp:
        return False, "Missing Ed25519 signature fields", sig_info

    from lilux.primitives.signing import verify_signature
    from rye.utils.trust_store import TrustStore

    trust_store = TrustStore()
    public_key_pem = trust_store.get_registry_key()

    if public_key_pem is None:
        return False, "Registry key not pinned. Pull again to TOFU-pin.", sig_info

    from lilux.primitives.signing import compute_key_fingerprint
    if compute_key_fingerprint(public_key_pem) != pubkey_fp:
        return False, "Registry key fingerprint mismatch", sig_info

    if not verify_signature(sig_info["hash"], ed25519_sig, public_key_pem):
        return False, "Ed25519 signature verification failed", sig_info

    return True, None, sig_info
