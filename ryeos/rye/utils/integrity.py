"""Unified content integrity verification.

Single entry point for all integrity checks across MCP tools.
Replaces the dead-code IntegrityVerifier with MetadataManager-based verification.
"""

import logging
import os
from pathlib import Path
from typing import Optional

from rye.constants import ItemType
from rye.utils.metadata_manager import MetadataManager

logger = logging.getLogger(__name__)


class IntegrityError(Exception):
    """Content integrity check failed."""
    pass


def _infer_item_id(file_path: Path, item_type: str, project_path: Optional[Path]) -> str:
    """Best-effort extraction of item_id from file path."""
    type_dir = ItemType.TYPE_DIRS.get(item_type, item_type)
    parts = file_path.parts
    for i, part in enumerate(parts):
        if part == ".ai" and i + 1 < len(parts) and parts[i + 1] == type_dir:
            rel = file_path.relative_to(Path(*parts[:i + 2]))
            return str(rel.with_suffix(""))
    return file_path.stem


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
    4. Signing key is in trust store (project > user > system)

    Raises IntegrityError if unsigned, tampered, or untrusted.

    Args:
        file_path: Path to the item file
        item_type: One of ItemType.DIRECTIVE, ItemType.TOOL, ItemType.KNOWLEDGE
        project_path: Optional project path for tool signature format resolution

    Returns:
        Verified content hash (SHA256 hex digest), or "unverified" in dev mode.
    """
    try:
        content = file_path.read_text(encoding="utf-8")

        sig_info = MetadataManager.get_signature_info(
            item_type, content, file_path=file_path, project_path=project_path
        )
        if not sig_info:
            item_id = _infer_item_id(file_path, item_type, project_path)
            raise IntegrityError(
                f"Unsigned item: {file_path}\n"
                f"  Item type: {item_type}\n"
                f"  Expected: rye:signed: header\n"
                f"  Fix: rye sign {item_type} {item_id}"
            )

        expected = sig_info["hash"]
        actual = MetadataManager.compute_hash(
            item_type, content, file_path=file_path, project_path=project_path
        )
        if actual != expected:
            item_id = _infer_item_id(file_path, item_type, project_path)
            raise IntegrityError(
                f"Content modified since signing: {file_path}\n"
                f"  Expected hash: {expected[:16]}…\n"
                f"  Actual hash: {actual[:16]}…\n"
                f"  Fix: Re-sign after editing:\n"
                f"    rye sign {item_type} {item_id}"
            )

        ed25519_sig = sig_info["ed25519_sig"]
        pubkey_fp = sig_info["pubkey_fp"]

        from lillux.primitives.signing import verify_signature
        from rye.utils.trust_store import TrustStore

        trust_store = TrustStore(project_path=project_path)
        key_info = trust_store.get_key(pubkey_fp)

        if key_info is None:
            trusted_keys = trust_store.list_keys()
            trusted_fps = [k.fingerprint for k in trusted_keys]
            item_id = _infer_item_id(file_path, item_type, project_path)
            raise IntegrityError(
                f"Untrusted key {pubkey_fp} for {file_path}\n"
                f"  Item signed by: {pubkey_fp}\n"
                f"  Trusted keys: {', '.join(trusted_fps) or 'none'}\n"
                f"  Fix: Add this key to your trust store, or re-sign:\n"
                f"    rye sign {item_type} {item_id}"
            )

        if not verify_signature(expected, ed25519_sig, key_info.public_key_pem):
            raise IntegrityError(
                f"Ed25519 signature verification failed: {file_path}\n"
                f"  Signed by: {pubkey_fp} ({key_info.owner or 'unknown'})\n"
                f"  This may indicate the file was tampered with after signing"
            )

        return actual
    except IntegrityError as e:
        if os.environ.get("RYE_DEV_MODE") == "1":
            logger.warning("[DEV MODE] %s — executing anyway", e)
            return "unverified"
        raise
