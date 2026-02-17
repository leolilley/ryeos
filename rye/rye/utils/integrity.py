"""Unified content integrity verification.

Single entry point for all integrity checks across MCP tools.
Replaces the dead-code IntegrityVerifier with MetadataManager-based verification.
"""

import logging
from pathlib import Path
from typing import Optional

from rye.constants import ItemType
from rye.utils.metadata_manager import MetadataManager

logger = logging.getLogger(__name__)


class IntegrityError(Exception):
    """Content integrity check failed."""
    pass


def verify_item(
    file_path: Path,
    item_type: str,
    *,
    project_path: Optional[Path] = None,
) -> str:
    """Verify signature matches content. Returns verified hash.

    Checks:
    1. Signature exists (rye:signed: format with Ed25519)
    2. Content hash matches embedded hash
    3. Ed25519 signature is valid
    4. Signing key is in trust store

    Raises IntegrityError if unsigned, tampered, or untrusted.

    Args:
        file_path: Path to the item file
        item_type: One of ItemType.DIRECTIVE, ItemType.TOOL, ItemType.KNOWLEDGE
        project_path: Optional project path for tool signature format resolution

    Returns:
        Verified content hash (SHA256 hex digest)
    """
    content = file_path.read_text(encoding="utf-8")

    sig_info = MetadataManager.get_signature_info(
        item_type, content, file_path=file_path, project_path=project_path
    )
    if not sig_info:
        raise IntegrityError(f"Unsigned item: {file_path}")

    expected = sig_info["hash"]
    actual = MetadataManager.compute_hash(
        item_type, content, file_path=file_path, project_path=project_path
    )
    if actual != expected:
        raise IntegrityError(
            f"Integrity failed: {file_path} "
            f"(expected {expected[:16]}…, got {actual[:16]}…)"
        )

    ed25519_sig = sig_info["ed25519_sig"]
    pubkey_fp = sig_info["pubkey_fp"]

    from lilux.primitives.signing import verify_signature
    from rye.utils.trust_store import TrustStore

    trust_store = TrustStore()
    public_key_pem = trust_store.get_key(pubkey_fp)

    if public_key_pem is None:
        raise IntegrityError(
            f"Untrusted key {pubkey_fp} for {file_path}. "
            f"Add the key to the trust store via the sign tool."
        )

    if not verify_signature(expected, ed25519_sig, public_key_pem):
        raise IntegrityError(
            f"Ed25519 signature verification failed: {file_path}"
        )

    return actual
