"""Ed25519 signing primitives for content integrity.

Pure cryptographic operations — no policy, no I/O beyond key material.
"""

import base64
import hashlib
import os
from pathlib import Path
from typing import Tuple

from cryptography.hazmat.primitives import serialization
from cryptography.hazmat.primitives.asymmetric.ed25519 import (
    Ed25519PrivateKey,
    Ed25519PublicKey,
)


def generate_keypair() -> Tuple[bytes, bytes]:
    """Generate a new Ed25519 keypair.

    Returns:
        Tuple of (private_key_pem, public_key_pem)
    """
    private_key = Ed25519PrivateKey.generate()
    public_key = private_key.public_key()

    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )

    public_pem = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )

    return private_pem, public_pem


def sign_hash(content_hash: str, private_key_pem: bytes) -> str:
    """Sign a content hash with Ed25519.

    Args:
        content_hash: SHA256 hex digest to sign
        private_key_pem: Ed25519 private key in PEM format

    Returns:
        Base64url-encoded signature string
    """
    private_key = serialization.load_pem_private_key(private_key_pem, password=None)
    signature = private_key.sign(content_hash.encode("utf-8"))
    return base64.urlsafe_b64encode(signature).decode("ascii")


def verify_signature(content_hash: str, signature_b64: str, public_key_pem: bytes) -> bool:
    """Verify an Ed25519 signature against a content hash.

    Args:
        content_hash: SHA256 hex digest that was signed
        signature_b64: Base64url-encoded Ed25519 signature
        public_key_pem: Ed25519 public key in PEM format

    Returns:
        True if signature is valid, False otherwise
    """
    try:
        public_key = serialization.load_pem_public_key(public_key_pem)
        signature = base64.urlsafe_b64decode(signature_b64)
        public_key.verify(signature, content_hash.encode("utf-8"))
        return True
    except Exception:
        return False


def compute_key_fingerprint(public_key_pem: bytes) -> str:
    """Compute fingerprint of an Ed25519 public key.

    Returns first 16 hex characters of SHA256(public_key_pem).

    Args:
        public_key_pem: Ed25519 public key in PEM format

    Returns:
        16-character hex fingerprint
    """
    return hashlib.sha256(public_key_pem).hexdigest()[:16]


def save_keypair(
    private_key_pem: bytes,
    public_key_pem: bytes,
    key_dir: Path,
) -> None:
    """Save keypair to directory with proper permissions.

    Args:
        private_key_pem: Ed25519 private key PEM bytes
        public_key_pem: Ed25519 public key PEM bytes
        key_dir: Directory to save keys into
    """
    key_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(key_dir, 0o700)

    private_path = key_dir / "private_key.pem"
    public_path = key_dir / "public_key.pem"

    private_path.write_bytes(private_key_pem)
    os.chmod(private_path, 0o600)

    public_path.write_bytes(public_key_pem)
    os.chmod(public_path, 0o644)


def load_keypair(key_dir: Path) -> Tuple[bytes, bytes]:
    """Load keypair from directory.

    Args:
        key_dir: Directory containing private_key.pem and public_key.pem

    Returns:
        Tuple of (private_key_pem, public_key_pem)

    Raises:
        FileNotFoundError: If keys don't exist
    """
    private_path = key_dir / "private_key.pem"
    public_path = key_dir / "public_key.pem"

    if not private_path.exists():
        raise FileNotFoundError(f"Private key not found at {private_path}")
    if not public_path.exists():
        raise FileNotFoundError(f"Public key not found at {public_path}")

    return private_path.read_bytes(), public_path.read_bytes()


def ensure_keypair(key_dir: Path) -> Tuple[bytes, bytes]:
    """Ensure a keypair exists at key_dir, generating one if needed.

    Args:
        key_dir: Directory for keys

    Returns:
        Tuple of (private_key_pem, public_key_pem)
    """
    try:
        return load_keypair(key_dir)
    except FileNotFoundError:
        private_pem, public_pem = generate_keypair()
        save_keypair(private_pem, public_pem, key_dir)
        return private_pem, public_pem
