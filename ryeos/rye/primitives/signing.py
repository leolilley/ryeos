"""Ed25519 and X25519 signing/box primitives for content integrity.

Delegates all crypto operations to the ``lillux`` Rust binary via subprocess.
"""

import json
import os
import shutil
import subprocess
import sys
import tempfile
from pathlib import Path
from typing import Optional, Tuple

_cached_binary: Optional[str] = None


def _lillux() -> str:
    """Locate the ``lillux`` binary, caching the result."""
    global _cached_binary
    if _cached_binary is not None:
        return _cached_binary

    # Check next to the running Python interpreter first
    candidate = Path(sys.executable).parent / "lillux"
    if candidate.is_file() and os.access(candidate, os.X_OK):
        _cached_binary = str(candidate)
        return _cached_binary

    found = shutil.which("lillux")
    if found:
        _cached_binary = found
        return _cached_binary

    raise FileNotFoundError(
        "lillux binary not found — install it or place it next to the Python interpreter"
    )


def _run(args: list[str]) -> subprocess.CompletedProcess[str]:
    """Run the lillux binary and return the completed process."""
    result = subprocess.run(
        [_lillux(), *args],
        capture_output=True,
        text=True,
    )
    if result.returncode != 0:
        raise RuntimeError(
            f"lillux {' '.join(args)} failed (rc={result.returncode}): {result.stderr.strip()}"
        )
    return result


def generate_keypair() -> Tuple[bytes, bytes]:
    """Generate a new Ed25519 keypair.

    Returns:
        Tuple of (private_key_pem, public_key_pem)
    """
    tmpdir = tempfile.mkdtemp()
    try:
        _run(["identity", "keypair", "generate", "--key-dir", tmpdir])
        private_pem = Path(tmpdir, "private_key.pem").read_bytes()
        public_pem = Path(tmpdir, "public_key.pem").read_bytes()
        return private_pem, public_pem
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


def generate_full_keypair() -> Tuple[bytes, bytes, bytes, bytes]:
    """Generate a new Ed25519 keypair and X25519 box keypair.

    Returns:
        Tuple of (private_key_pem, public_key_pem, box_key, box_pub)
    """
    tmpdir = tempfile.mkdtemp()
    try:
        _run(["identity", "keypair", "generate", "--key-dir", tmpdir])
        private_pem = Path(tmpdir, "private_key.pem").read_bytes()
        public_pem = Path(tmpdir, "public_key.pem").read_bytes()
        box_key = Path(tmpdir, "box_key.pem").read_bytes()
        box_pub = Path(tmpdir, "box_pub.pem").read_bytes()
        return private_pem, public_pem, box_key, box_pub
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


def sign_hash(content_hash: str, private_key_pem: bytes) -> str:
    """Sign a content hash with Ed25519.

    Args:
        content_hash: SHA256 hex digest to sign
        private_key_pem: Ed25519 private key in PEM format

    Returns:
        Base64url-encoded signature string
    """
    tmpdir = tempfile.mkdtemp()
    try:
        key_path = Path(tmpdir, "private_key.pem")
        key_path.write_bytes(private_key_pem)
        os.chmod(key_path, 0o600)

        result = _run(["identity", "sign", "--key-dir", tmpdir, "--hash", content_hash])
        data = json.loads(result.stdout)
        return data["signature"]
    finally:
        shutil.rmtree(tmpdir, ignore_errors=True)


def verify_signature(content_hash: str, signature_b64: str, public_key_pem: bytes) -> bool:
    """Verify an Ed25519 signature against a content hash.

    Args:
        content_hash: SHA256 hex digest that was signed
        signature_b64: Base64url-encoded Ed25519 signature
        public_key_pem: Ed25519 public key in PEM format

    Returns:
        True if signature is valid, False otherwise
    """
    tmpfile = None
    try:
        fd, tmpfile = tempfile.mkstemp(suffix=".pem")
        os.write(fd, public_key_pem)
        os.close(fd)

        result = _run([
            "identity", "verify",
            "--hash", content_hash,
            "--signature", signature_b64,
            "--public-key", tmpfile,
        ])
        data = json.loads(result.stdout)
        return data["valid"]
    except Exception:
        return False
    finally:
        if tmpfile and os.path.exists(tmpfile):
            os.unlink(tmpfile)


def compute_key_fingerprint(public_key_pem: bytes) -> str:
    """Compute fingerprint of an Ed25519 public key.

    Args:
        public_key_pem: Ed25519 public key in PEM format

    Returns:
        Fingerprint string
    """
    tmpfile = None
    try:
        fd, tmpfile = tempfile.mkstemp(suffix=".pem")
        os.write(fd, public_key_pem)
        os.close(fd)

        result = _run(["identity", "keypair", "fingerprint", "--public-key", tmpfile])
        data = json.loads(result.stdout)
        return data["fingerprint"]
    finally:
        if tmpfile and os.path.exists(tmpfile):
            os.unlink(tmpfile)


def compute_box_fingerprint(box_pub: bytes) -> str:
    """Compute fingerprint of an X25519 box public key.

    Args:
        box_pub: Raw base64url-encoded X25519 public key bytes

    Returns:
        Fingerprint string
    """
    tmpfile = None
    try:
        fd, tmpfile = tempfile.mkstemp(suffix=".pem")
        os.write(fd, box_pub)
        os.close(fd)

        result = _run(["identity", "keypair", "box-fingerprint", "--public-key", tmpfile])
        data = json.loads(result.stdout)
        return data["fingerprint"]
    finally:
        if tmpfile and os.path.exists(tmpfile):
            os.unlink(tmpfile)


def load_box_keypair(key_dir: Path) -> Tuple[bytes, bytes]:
    """Load X25519 box keypair from directory.

    Args:
        key_dir: Directory containing box_key.pem and box_pub.pem

    Returns:
        Tuple of (box_key, box_pub) as raw bytes

    Raises:
        FileNotFoundError: If keys don't exist
    """
    box_key_path = key_dir / "box_key.pem"
    box_pub_path = key_dir / "box_pub.pem"

    if not box_key_path.exists():
        raise FileNotFoundError(f"Box key not found at {box_key_path}")
    if not box_pub_path.exists():
        raise FileNotFoundError(f"Box public key not found at {box_pub_path}")

    return box_key_path.read_bytes(), box_pub_path.read_bytes()


def save_box_keypair(box_key: bytes, box_pub: bytes, key_dir: Path) -> None:
    """Save X25519 box keypair to directory with proper permissions.

    Args:
        box_key: Raw base64url-encoded X25519 private key bytes
        box_pub: Raw base64url-encoded X25519 public key bytes
        key_dir: Directory to save keys into
    """
    key_dir.mkdir(parents=True, exist_ok=True)

    box_key_path = key_dir / "box_key.pem"
    box_pub_path = key_dir / "box_pub.pem"

    box_key_path.write_bytes(box_key)
    os.chmod(box_key_path, 0o600)

    box_pub_path.write_bytes(box_pub)
    os.chmod(box_pub_path, 0o644)


def save_keypair(
    private_key_pem: bytes,
    public_key_pem: bytes,
    key_dir: Path,
    *,
    box_key: Optional[bytes] = None,
    box_pub: Optional[bytes] = None,
) -> None:
    """Save keypair to directory with proper permissions.

    Args:
        private_key_pem: Ed25519 private key PEM bytes
        public_key_pem: Ed25519 public key PEM bytes
        key_dir: Directory to save keys into
        box_key: Optional X25519 box private key bytes
        box_pub: Optional X25519 box public key bytes
    """
    key_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(key_dir, 0o700)

    private_path = key_dir / "private_key.pem"
    public_path = key_dir / "public_key.pem"

    private_path.write_bytes(private_key_pem)
    os.chmod(private_path, 0o600)

    public_path.write_bytes(public_key_pem)
    os.chmod(public_path, 0o644)

    if box_key is not None and box_pub is not None:
        save_box_keypair(box_key, box_pub, key_dir)


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
        _run(["identity", "keypair", "generate", "--key-dir", str(key_dir)])
        return load_keypair(key_dir)


def ensure_full_keypair(key_dir: Path) -> Tuple[bytes, bytes, bytes, bytes]:
    """Ensure Ed25519 and X25519 keypairs exist at key_dir, generating if needed.

    Args:
        key_dir: Directory for keys

    Returns:
        Tuple of (private_key_pem, public_key_pem, box_key, box_pub)
    """
    try:
        ed_keys = load_keypair(key_dir)
        box_keys = load_box_keypair(key_dir)
        return ed_keys[0], ed_keys[1], box_keys[0], box_keys[1]
    except FileNotFoundError:
        _run(["identity", "keypair", "generate", "--key-dir", str(key_dir)])
        ed_keys = load_keypair(key_dir)
        box_keys = load_box_keypair(key_dir)
        return ed_keys[0], ed_keys[1], box_keys[0], box_keys[1]
