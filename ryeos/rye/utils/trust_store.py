"""Identity-aware trust store for Ed25519 public keys.

Trusted keys are TOML identity documents that bind a key to a registry account.
They follow the standard 3-tier resolution: project > user > system.

Each trusted key file lives at .ai/trusted_keys/{fingerprint}.toml:

    # rye:signed:TIMESTAMP:HASH:SIG:FP
    fingerprint = "16e73c5829f69d6f"
    owner = "leo"
    attestation = ""

    [public_key]
    pem = \"\"\"
    -----BEGIN PUBLIC KEY-----
    MCowBQYDK2VwAyEA...
    -----END PUBLIC KEY-----
    \"\"\"
"""

import logging
import tomllib
from dataclasses import dataclass
from pathlib import Path
from typing import List, Optional

from lillux.primitives.signing import compute_key_fingerprint
from rye.constants import AI_DIR
from rye.utils.path_utils import get_user_space

logger = logging.getLogger(__name__)

TRUSTED_KEYS_DIR = "trusted_keys"


@dataclass
class TrustedKeyInfo:
    """A trusted key with identity binding."""

    fingerprint: str
    owner: str
    public_key_pem: bytes
    attestation: Optional[str] = None
    source: str = ""  # "project", "user", or "system:{bundle_id}"

    def to_toml(self) -> str:
        """Serialize to TOML string."""
        pem_str = self.public_key_pem.decode("utf-8").strip()
        attestation = self.attestation or ""
        lines = [
            f'fingerprint = "{self.fingerprint}"',
            f'owner = "{self.owner}"',
            f'attestation = "{attestation}"',
            "",
            "[public_key]",
            f'pem = """',
            pem_str,
            '"""',
            "",
        ]
        return "\n".join(lines)

    @classmethod
    def from_toml(cls, path: Path, source: str = "") -> "TrustedKeyInfo":
        """Load from a TOML file."""
        content = path.read_text(encoding="utf-8")
        # Strip signature comment before parsing TOML
        lines = content.split("\n")
        toml_lines = [
            line for line in lines
            if not line.startswith("# rye:signed:")
        ]
        raw = tomllib.loads("\n".join(toml_lines))
        pem_str = raw.get("public_key", {}).get("pem", "").strip()
        attestation = raw.get("attestation") or None
        if attestation == "":
            attestation = None
        return cls(
            fingerprint=raw["fingerprint"],
            owner=raw.get("owner", "unknown"),
            public_key_pem=(pem_str + "\n").encode("utf-8"),
            attestation=attestation,
            source=source,
        )


class TrustStore:
    """Manages trusted Ed25519 public keys with 3-tier resolution.

    Resolution order: project > user > system .ai/trusted_keys/{fp}.toml
    """

    def __init__(
        self,
        *,
        project_path: Optional[Path] = None,
    ):
        self.project_path = project_path
        self._user_trust_dir = get_user_space() / AI_DIR / TRUSTED_KEYS_DIR

    def _search_dirs(self) -> List[tuple[str, Path]]:
        """Build ordered list of (source_label, directory) to search."""
        dirs: List[tuple[str, Path]] = []
        if self.project_path:
            dirs.append(("project", self.project_path / AI_DIR / TRUSTED_KEYS_DIR))
        dirs.append(("user", self._user_trust_dir))
        from rye.utils.path_utils import get_system_spaces
        for bundle in get_system_spaces():
            dirs.append(
                (f"system:{bundle.bundle_id}", bundle.root_path / AI_DIR / TRUSTED_KEYS_DIR)
            )
        return dirs

    def is_trusted(self, fingerprint: str) -> bool:
        """Check if a key fingerprint is trusted in any space."""
        return self.get_key(fingerprint) is not None

    def get_key(self, fingerprint: str) -> Optional[TrustedKeyInfo]:
        """Get trusted key by fingerprint.

        Searches project > user > system .ai/trusted_keys/{fingerprint}.toml
        """
        for source, trust_dir in self._search_dirs():
            if not trust_dir.is_dir():
                continue
            key_file = trust_dir / f"{fingerprint}.toml"
            if key_file.is_file():
                try:
                    info = TrustedKeyInfo.from_toml(key_file, source=source)
                    # Validate fingerprint matches actual key
                    actual_fp = compute_key_fingerprint(info.public_key_pem)
                    if actual_fp != fingerprint:
                        logger.warning(
                            "Fingerprint mismatch in %s: expected %s, got %s",
                            key_file, fingerprint, actual_fp,
                        )
                        continue
                    return info
                except Exception:
                    logger.warning("Failed to load trusted key %s", key_file, exc_info=True)
                    continue
        return None

    def get_public_key(self, fingerprint: str) -> Optional[bytes]:
        """Get public key PEM bytes by fingerprint. Convenience wrapper."""
        info = self.get_key(fingerprint)
        return info.public_key_pem if info else None

    def add_key(
        self,
        public_key_pem: bytes,
        owner: str = "local",
        *,
        attestation: Optional[str] = None,
        space: str = "user",
    ) -> str:
        """Add a public key to the trust store.

        Args:
            public_key_pem: Ed25519 public key in PEM format
            owner: Registry username or "local" for self-generated keys
            attestation: Registry attestation signature (optional)
            space: Where to write: "user" (default) or "project"

        Returns:
            Fingerprint of the added key
        """
        fingerprint = compute_key_fingerprint(public_key_pem)

        if space == "project" and self.project_path:
            trust_dir = self.project_path / AI_DIR / TRUSTED_KEYS_DIR
        else:
            trust_dir = self._user_trust_dir

        trust_dir.mkdir(parents=True, exist_ok=True)

        info = TrustedKeyInfo(
            fingerprint=fingerprint,
            owner=owner,
            public_key_pem=public_key_pem,
            attestation=attestation,
        )
        key_file = trust_dir / f"{fingerprint}.toml"
        key_file.write_text(info.to_toml(), encoding="utf-8")
        logger.info("Trusted key %s (owner=%s)", fingerprint, owner)
        return fingerprint

    def remove_key(self, fingerprint: str) -> bool:
        """Remove a key from the user trust store.

        Returns:
            True if removed, False if not found
        """
        key_file = self._user_trust_dir / f"{fingerprint}.toml"
        if key_file.exists():
            key_file.unlink()
            logger.info("Removed trusted key %s", fingerprint)
            return True
        return False

    def pin_registry_key(
        self,
        public_key_pem: bytes,
        registry_name: str = "rye-registry",
    ) -> str:
        """Pin the registry public key (TOFU).

        The registry key is stored as a normal trusted key with owner set
        to the registry name. If already pinned (same fingerprint exists),
        this is a no-op.

        Args:
            public_key_pem: Registry's Ed25519 public key PEM
            registry_name: Registry identifier (default: "rye-registry")

        Returns:
            Fingerprint of the pinned key
        """
        fingerprint = compute_key_fingerprint(public_key_pem)
        existing = self.get_key(fingerprint)
        if existing:
            return fingerprint
        return self.add_key(public_key_pem, owner=registry_name)

    def get_registry_key(self, registry_name: str = "rye-registry") -> Optional[bytes]:
        """Get the pinned registry public key by scanning for owner match.

        Returns:
            Registry public key PEM, or None if not pinned
        """
        for info in self.list_keys():
            if info.owner == registry_name:
                return info.public_key_pem
        return None

    def list_keys(self) -> List[TrustedKeyInfo]:
        """List all trusted keys across all spaces."""
        keys: List[TrustedKeyInfo] = []
        seen_fps: set[str] = set()
        for source, trust_dir in self._search_dirs():
            if not trust_dir.is_dir():
                continue
            for toml_file in trust_dir.glob("*.toml"):
                try:
                    info = TrustedKeyInfo.from_toml(toml_file, source=source)
                    if info.fingerprint not in seen_fps:
                        seen_fps.add(info.fingerprint)
                        keys.append(info)
                except Exception:
                    logger.warning("Failed to load trusted key %s", toml_file, exc_info=True)
        return keys
