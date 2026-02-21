"""
Centralized Metadata Management

Provides unified interface for signing and hashing operations
across directives, tools, and knowledge entries.

Signature format: rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP

Note: Parsing is delegated to parser_router which uses data-driven parsers.
"""

import hashlib
import logging
import re
from abc import ABC, abstractmethod
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, Optional

from rye.utils.signature_formats import get_signature_format
from rye.constants import ItemType, AI_DIR

logger = logging.getLogger(__name__)

# Regex components for the new rye:signed format
# rye:signed:TIMESTAMP:CONTENT_HASH:ED25519_SIG:PUBKEY_FP
# Optional registry provenance suffix: |provider@username
_SIGNED_FIELDS = (
    r"(\d{4}-\d{2}-\d{2}T\d{2}:\d{2}:\d{2}Z)"  # TIMESTAMP (ISO 8601)
    r":([a-f0-9]{64})"  # CONTENT_HASH
    r":([A-Za-z0-9_-]+={0,2})"  # ED25519_SIG (base64url)
    r":([a-f0-9]{16})"  # PUBKEY_FP
    r"(?:\|([a-zA-Z0-9_-]+)@([a-zA-Z0-9_-]+))?"  # optional |provider@username
)


def compute_content_hash(content: str) -> str:
    """Compute SHA256 hash of content (full 64 characters)."""
    return hashlib.sha256(content.encode()).hexdigest()


def generate_timestamp() -> str:
    """Generate ISO format timestamp in UTC."""
    return datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")


class MetadataStrategy(ABC):
    """Base strategy for item-type-specific metadata operations."""

    @abstractmethod
    def extract_content_for_hash(self, file_content: str) -> str:
        """Extract the content portion that should be hashed."""

    @abstractmethod
    def format_signature(
        self, timestamp: str, content_hash: str,
        ed25519_sig: str = "", pubkey_fp: str = "",
    ) -> str:
        """Format signature according to item type."""

    @abstractmethod
    def extract_signature(self, file_content: str) -> Optional[Dict[str, str]]:
        """Extract signature from file content. Returns None if no signature found."""

    @abstractmethod
    def insert_signature(self, content: str, signature: str) -> str:
        """Insert signature into content."""

    @abstractmethod
    def remove_signature(self, content: str) -> str:
        """Remove existing signature from content."""


class DirectiveMetadataStrategy(MetadataStrategy):
    """Strategy for directive metadata operations (XML in markdown)."""

    _SIGNED_RE = re.compile(
        r"^<!-- rye:signed:" + _SIGNED_FIELDS + r" -->"
    )
    _REMOVE_RE = re.compile(
        r"^<!-- (?:rye|kiwi-mcp):(?:validated|signed):[^>]+-->\n"
    )

    def extract_content_for_hash(self, file_content: str) -> str:
        xml_content = self._extract_xml_from_content(file_content)
        if not xml_content:
            raise ValueError("No XML directive found in content")
        return xml_content

    def format_signature(
        self, timestamp: str, content_hash: str,
        ed25519_sig: str = "", pubkey_fp: str = "",
    ) -> str:
        return (
            f"<!-- rye:signed:{timestamp}:{content_hash}"
            f":{ed25519_sig}:{pubkey_fp} -->\n"
        )

    def extract_signature(self, file_content: str) -> Optional[Dict[str, str]]:
        sig_match = self._SIGNED_RE.match(file_content)
        if not sig_match:
            return None
        result: Dict[str, str] = {
            "timestamp": sig_match.group(1),
            "hash": sig_match.group(2),
            "ed25519_sig": sig_match.group(3),
            "pubkey_fp": sig_match.group(4),
        }
        if sig_match.group(5) and sig_match.group(6):
            result["registry_provider"] = sig_match.group(5)
            result["registry_username"] = sig_match.group(6)
        return result

    def insert_signature(self, content: str, signature: str) -> str:
        content_clean = self.remove_signature(content)
        return signature + content_clean

    def remove_signature(self, content: str) -> str:
        return self._REMOVE_RE.sub("", content)

    def _extract_xml_from_content(self, content: str) -> Optional[str]:
        start_match = re.search(r"<directive[^>]*>", content)
        if not start_match:
            return None
        start_idx = start_match.start()
        end_tag = "</directive>"
        end_idx = content.rfind(end_tag)
        if end_idx == -1 or end_idx < start_idx:
            return None
        return content[start_idx : end_idx + len(end_tag)].strip()


class ToolMetadataStrategy(MetadataStrategy):
    """Strategy for tool metadata operations (language-aware)."""

    def __init__(
        self, file_path: Optional[Path] = None, project_path: Optional[Path] = None
    ):
        self.file_path = file_path
        self.project_path = project_path
        self._sig_format = None

    def _get_signature_format(self) -> Dict[str, Any]:
        if self._sig_format is None:
            if self.file_path:
                self._sig_format = get_signature_format(
                    self.file_path, self.project_path
                )
            else:
                self._sig_format = {"prefix": "#", "after_shebang": True}
        return self._sig_format

    def extract_content_for_hash(self, file_content: str) -> str:
        content_without_sig = self.remove_signature(file_content)
        content_without_sig = re.sub(r"^#!/[^\n]*\n", "", content_without_sig)
        return content_without_sig

    def format_signature(
        self, timestamp: str, content_hash: str,
        ed25519_sig: str = "", pubkey_fp: str = "",
    ) -> str:
        sig_format = self._get_signature_format()
        prefix = sig_format["prefix"]
        return (
            f"{prefix} rye:signed:{timestamp}:{content_hash}"
            f":{ed25519_sig}:{pubkey_fp}\n"
        )

    def extract_signature(self, file_content: str) -> Optional[Dict[str, str]]:
        sig_format = self._get_signature_format()
        prefix = re.escape(sig_format["prefix"])

        if sig_format.get("after_shebang", True):
            sig_pattern = (
                rf"^(?:#!/[^\n]*\n)?{prefix} rye:signed:" + _SIGNED_FIELDS
            )
        else:
            sig_pattern = rf"^{prefix} rye:signed:" + _SIGNED_FIELDS

        sig_match = re.match(sig_pattern, file_content)
        if not sig_match:
            return None

        result: Dict[str, str] = {
            "timestamp": sig_match.group(1),
            "hash": sig_match.group(2),
            "ed25519_sig": sig_match.group(3),
            "pubkey_fp": sig_match.group(4),
        }
        if sig_match.group(5) and sig_match.group(6):
            result["registry_provider"] = sig_match.group(5)
            result["registry_username"] = sig_match.group(6)
        return result

    def insert_signature(self, content: str, signature: str) -> str:
        sig_format = self._get_signature_format()
        content_clean = self.remove_signature(content)

        if sig_format.get("after_shebang", True) and content_clean.startswith("#!/"):
            lines = content_clean.split("\n", 1)
            return lines[0] + "\n" + signature + (lines[1] if len(lines) > 1 else "")
        else:
            return signature + content_clean

    def remove_signature(self, content: str) -> str:
        sig_format = self._get_signature_format()
        prefix = re.escape(sig_format["prefix"])

        content_without_shebang = re.sub(r"^#!/[^\n]*\n", "", content)
        sig_pattern = rf"^{prefix} (?:rye|kiwi-mcp):(?:validated|signed):[^\n]+\n"
        content_without_sig = re.sub(sig_pattern, "", content_without_shebang)

        shebang_match = re.match(r"^(#!/[^\n]*\n)", content)
        if shebang_match:
            return shebang_match.group(1) + content_without_sig
        return content_without_sig


class KnowledgeMetadataStrategy(MetadataStrategy):
    """Strategy for knowledge metadata operations (signature at top like directives)."""

    _SIGNED_RE = re.compile(
        r"^<!-- rye:signed:" + _SIGNED_FIELDS + r" -->"
    )
    _REMOVE_RE = re.compile(
        r"^<!-- (?:rye|kiwi-mcp):(?:validated|signed):[^>]+-->\n"
    )

    def extract_content_for_hash(self, file_content: str) -> str:
        content_without_sig = self.remove_signature(file_content)

        if not content_without_sig.startswith("---"):
            return content_without_sig

        end_idx = content_without_sig.find("---", 3)
        if end_idx == -1:
            return content_without_sig

        entry_content = content_without_sig[end_idx + 3 :].strip()
        return entry_content

    def format_signature(
        self, timestamp: str, content_hash: str,
        ed25519_sig: str = "", pubkey_fp: str = "",
    ) -> str:
        return (
            f"<!-- rye:signed:{timestamp}:{content_hash}"
            f":{ed25519_sig}:{pubkey_fp} -->\n"
        )

    def extract_signature(self, file_content: str) -> Optional[Dict[str, str]]:
        sig_match = self._SIGNED_RE.match(file_content)
        if not sig_match:
            return None
        result: Dict[str, str] = {
            "timestamp": sig_match.group(1),
            "hash": sig_match.group(2),
            "ed25519_sig": sig_match.group(3),
            "pubkey_fp": sig_match.group(4),
        }
        if sig_match.group(5) and sig_match.group(6):
            result["registry_provider"] = sig_match.group(5)
            result["registry_username"] = sig_match.group(6)
        return result

    def insert_signature(self, content: str, signature: str) -> str:
        content_clean = self.remove_signature(content)
        return signature + content_clean

    def remove_signature(self, content: str) -> str:
        return self._REMOVE_RE.sub("", content)


class MetadataManager:
    """Unified metadata management interface."""

    @classmethod
    def get_strategy(
        cls,
        item_type: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> MetadataStrategy:
        if item_type == ItemType.DIRECTIVE:
            return DirectiveMetadataStrategy()
        elif item_type == ItemType.TOOL:
            return ToolMetadataStrategy(file_path=file_path, project_path=project_path)
        elif item_type == ItemType.KNOWLEDGE:
            return KnowledgeMetadataStrategy()
        else:
            raise ValueError(
                f"Unknown item_type: {item_type}. Supported: {ItemType.ALL}"
            )

    @classmethod
    def compute_hash(
        cls,
        item_type: str,
        file_content: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> str:
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        content_for_hash = strategy.extract_content_for_hash(file_content)
        return compute_content_hash(content_for_hash)

    @classmethod
    def create_signature(
        cls,
        item_type: str,
        file_content: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> str:
        """Create Ed25519 signature for content.

        Auto-generates keypair on first use. Auto-trusts own public key.
        """
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        content_for_hash = strategy.extract_content_for_hash(file_content)
        content_hash = compute_content_hash(content_for_hash)
        timestamp = generate_timestamp()

        from rye.utils.path_utils import get_user_space
        from lilux.primitives.signing import (
            ensure_keypair,
            sign_hash,
            compute_key_fingerprint,
        )
        from rye.utils.trust_store import TrustStore

        key_dir = get_user_space() / AI_DIR / "keys"
        private_pem, public_pem = ensure_keypair(key_dir)

        ed25519_sig = sign_hash(content_hash, private_pem)
        pubkey_fp = compute_key_fingerprint(public_pem)

        trust_store = TrustStore()
        if not trust_store.is_trusted(pubkey_fp):
            trust_store.add_key(public_pem, owner="local")

        return strategy.format_signature(timestamp, content_hash, ed25519_sig, pubkey_fp)

    @classmethod
    def create_signature_from_hash(
        cls,
        item_type: str,
        content_hash: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> str:
        """Create Ed25519 signature using a precomputed integrity hash."""
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        timestamp = generate_timestamp()

        from rye.utils.path_utils import get_user_space
        from lilux.primitives.signing import (
            ensure_keypair,
            sign_hash,
            compute_key_fingerprint,
        )
        from rye.utils.trust_store import TrustStore

        key_dir = get_user_space() / AI_DIR / "keys"
        private_pem, public_pem = ensure_keypair(key_dir)

        ed25519_sig = sign_hash(content_hash, private_pem)
        pubkey_fp = compute_key_fingerprint(public_pem)

        trust_store = TrustStore()
        if not trust_store.is_trusted(pubkey_fp):
            trust_store.add_key(public_pem, owner="local")

        return strategy.format_signature(timestamp, content_hash, ed25519_sig, pubkey_fp)

    @classmethod
    def sign_content(
        cls,
        item_type: str,
        file_content: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> str:
        """Add Ed25519 signature to content."""
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        signature = cls.create_signature(
            item_type, file_content, file_path=file_path, project_path=project_path
        )
        return strategy.insert_signature(file_content, signature)

    @classmethod
    def sign_content_with_hash(
        cls,
        item_type: str,
        file_content: str,
        content_hash: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> str:
        """Add Ed25519 signature to content using a precomputed integrity hash."""
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        signature = cls.create_signature_from_hash(
            item_type, content_hash, file_path=file_path, project_path=project_path
        )
        return strategy.insert_signature(file_content, signature)

    @classmethod
    def get_signature_info(
        cls,
        item_type: str,
        file_content: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> Optional[Dict[str, str]]:
        """Get signature information without verification."""
        strategy = cls.get_strategy(
            item_type, file_path=file_path, project_path=project_path
        )
        return strategy.extract_signature(file_content)

    @classmethod
    def get_signature_hash(
        cls,
        item_type: str,
        file_content: str,
        file_path: Optional[Path] = None,
        project_path: Optional[Path] = None,
    ) -> Optional[str]:
        """Extract integrity hash from signature without verification."""
        signature_data = cls.get_signature_info(
            item_type, file_content, file_path, project_path
        )
        return signature_data["hash"] if signature_data else None
