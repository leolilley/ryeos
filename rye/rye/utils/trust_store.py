"""Trust store for Ed25519 public keys.

Manages trusted keys under get_user_space() / AI_DIR / "trusted_keys/".
- Own pubkey auto-trusted on keygen
- Registry pubkey pinned on first pull (TOFU)
- Peer pubkeys manually trusted via sign tool
"""

import logging
from pathlib import Path
from typing import Optional

from lilux.primitives.signing import compute_key_fingerprint
from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space

logger = logging.getLogger(__name__)


class TrustStore:
    """Manages trusted Ed25519 public keys."""

    REGISTRY_KEY_NAME = "registry.pem"

    def __init__(self, trust_dir: Optional[Path] = None):
        self.trust_dir = trust_dir or (get_user_space() / AI_DIR / "trusted_keys")

    def _ensure_dir(self) -> None:
        self.trust_dir.mkdir(parents=True, exist_ok=True)

    def is_trusted(self, fingerprint: str) -> bool:
        """Check if a key fingerprint is in the trust store."""
        key_path = self.trust_dir / f"{fingerprint}.pem"
        if key_path.exists():
            return True
        registry_path = self.trust_dir / self.REGISTRY_KEY_NAME
        if registry_path.exists():
            registry_pem = registry_path.read_bytes()
            if compute_key_fingerprint(registry_pem) == fingerprint:
                return True
        return False

    def get_key(self, fingerprint: str) -> Optional[bytes]:
        """Get public key PEM by fingerprint.

        Returns:
            Public key PEM bytes, or None if not trusted
        """
        key_path = self.trust_dir / f"{fingerprint}.pem"
        if key_path.exists():
            return key_path.read_bytes()
        registry_path = self.trust_dir / self.REGISTRY_KEY_NAME
        if registry_path.exists():
            registry_pem = registry_path.read_bytes()
            if compute_key_fingerprint(registry_pem) == fingerprint:
                return registry_pem
        return None

    def add_key(self, public_key_pem: bytes, label: Optional[str] = None) -> str:
        """Add a public key to the trust store.

        Args:
            public_key_pem: Ed25519 public key in PEM format
            label: Optional human-readable label (unused in filename)

        Returns:
            Fingerprint of the added key
        """
        self._ensure_dir()
        fingerprint = compute_key_fingerprint(public_key_pem)
        key_path = self.trust_dir / f"{fingerprint}.pem"
        key_path.write_bytes(public_key_pem)
        logger.info(f"Trusted key {fingerprint}" + (f" ({label})" if label else ""))
        return fingerprint

    def remove_key(self, fingerprint: str) -> bool:
        """Remove a key from the trust store.

        Returns:
            True if removed, False if not found
        """
        key_path = self.trust_dir / f"{fingerprint}.pem"
        if key_path.exists():
            key_path.unlink()
            logger.info(f"Removed trusted key {fingerprint}")
            return True
        return False

    def pin_registry_key(self, public_key_pem: bytes) -> str:
        """Pin the registry public key (TOFU).

        If a registry key already exists, this is a no-op (returns existing fingerprint).

        Args:
            public_key_pem: Registry's Ed25519 public key PEM

        Returns:
            Fingerprint of the pinned key
        """
        self._ensure_dir()
        registry_path = self.trust_dir / self.REGISTRY_KEY_NAME
        if registry_path.exists():
            existing_pem = registry_path.read_bytes()
            return compute_key_fingerprint(existing_pem)
        registry_path.write_bytes(public_key_pem)
        fingerprint = compute_key_fingerprint(public_key_pem)
        logger.info(f"Pinned registry key {fingerprint}")
        return fingerprint

    def get_registry_key(self) -> Optional[bytes]:
        """Get the pinned registry public key.

        Returns:
            Registry public key PEM, or None if not pinned
        """
        registry_path = self.trust_dir / self.REGISTRY_KEY_NAME
        if registry_path.exists():
            return registry_path.read_bytes()
        return None

    def list_keys(self):
        """List all trusted keys.

        Returns:
            List of dicts with fingerprint, path, and is_registry fields
        """
        if not self.trust_dir.exists():
            return []
        keys = []
        for pem_file in self.trust_dir.glob("*.pem"):
            pem_bytes = pem_file.read_bytes()
            fp = compute_key_fingerprint(pem_bytes)
            keys.append({
                "fingerprint": fp,
                "path": str(pem_file),
                "is_registry": pem_file.name == self.REGISTRY_KEY_NAME,
                "label": pem_file.stem,
            })
        return keys
