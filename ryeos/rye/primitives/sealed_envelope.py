"""Sealed secret envelope: decryption, sealing, and validation.

Server-side decryption shells out to the lillux binary.
Client-side sealing uses Python crypto with ephemeral X25519 keys.
"""

import base64
import json
import logging
import shutil
import subprocess
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.x25519 import (
    X25519PrivateKey,
    X25519PublicKey,
)
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.hashes import SHA256
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

from rye.primitives.signing import compute_box_fingerprint

log = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Shared helpers
# ---------------------------------------------------------------------------

def _b64url_decode(data: bytes | str) -> bytes:
    """Decode base64url with or without padding."""
    if isinstance(data, str):
        data = data.encode()
    data += b"=" * (-len(data) % 4)
    return base64.urlsafe_b64decode(data)


# ---------------------------------------------------------------------------
# Secret name safety
# ---------------------------------------------------------------------------

RESERVED_ENV_NAMES = frozenset({
    "PATH", "HOME", "USER", "SHELL", "LANG", "TERM",
    "PYTHONPATH", "PYTHONHOME", "PYTHON_PATH",
    "TMPDIR", "TEMP", "TMP",
    "RYE_SIGNING_KEY_DIR", "RYE_KERNEL_PYTHON", "RYE_REMOTE_NAME",
    "RYE_NODE_CONFIG", "USER_SPACE",
    "VIRTUAL_ENV", "CONDA_PREFIX",
    "LD_LIBRARY_PATH", "LD_PRELOAD",
    "DYLD_LIBRARY_PATH", "DYLD_INSERT_LIBRARIES",
})

RESERVED_ENV_PREFIXES = (
    "SUPABASE_", "MODAL_", "LD_", "SSL_",
    "AWS_", "GOOGLE_", "AZURE_", "GITHUB_", "CI_",
    "DOCKER_", "RYE_INTERNAL_",
)

MAX_TOTAL_ENV_BYTES = 1024 * 1024  # 1 MB
MAX_VARIABLE_COUNT = 256
MAX_VALUE_LENGTH = 64 * 1024  # 64 KB per value


def is_safe_secret_name(name: str) -> bool:
    """Return True if *name* is safe to inject as an env variable."""
    if not name.isidentifier():
        return False
    if name in RESERVED_ENV_NAMES:
        return False
    if name.startswith(RESERVED_ENV_PREFIXES):
        return False
    return True


def validate_env_map(env_map: dict) -> list[str]:
    """Validate a decrypted env map. Returns a list of error strings (empty = valid)."""
    errors: list[str] = []

    if len(env_map) > MAX_VARIABLE_COUNT:
        errors.append(
            f"too many variables: {len(env_map)} exceeds limit of {MAX_VARIABLE_COUNT}"
        )

    total_bytes = 0
    for name, value in env_map.items():
        if not isinstance(name, str):
            errors.append(f"variable name is not a string: {name!r}")
            continue
        if not isinstance(value, str):
            errors.append(f"variable value for {name} is not a string")
            continue

        if "\x00" in value:
            errors.append(f"variable {name} contains NUL byte")

        val_len = len(value.encode())
        if val_len > MAX_VALUE_LENGTH:
            errors.append(
                f"variable {name} value too large: {val_len} bytes "
                f"exceeds limit of {MAX_VALUE_LENGTH}"
            )

        total_bytes += len(name.encode()) + val_len

    if total_bytes > MAX_TOTAL_ENV_BYTES:
        errors.append(
            f"total env size {total_bytes} bytes exceeds limit of {MAX_TOTAL_ENV_BYTES}"
        )

    return errors


# ---------------------------------------------------------------------------
# Server-side envelope decryption (shells out to lillux binary)
# ---------------------------------------------------------------------------

def decrypt_envelope(envelope: dict, box_key_path: Path) -> dict:
    """Decrypt a sealed secret envelope by shelling out to the lillux binary.

    Args:
        envelope: The sealed envelope dict (version, enc, ciphertext, aad_fields, …).
        box_key_path: Path to the ``box_key.pem`` file containing the raw
            base64url-encoded X25519 private key.

    Returns:
        Dict of safe env variable name → value pairs.

    Raises:
        ConfigurationError: If the lillux binary is not found.
        ValueError: On decryption failure or error from the binary.
    """
    from rye.errors import ConfigurationError

    lillux_bin = shutil.which("lillux")
    if not lillux_bin:
        raise ConfigurationError(
            "lillux binary not found on PATH. "
            "Ensure ryeos is installed correctly."
        )

    key_dir = str(box_key_path.parent)
    proc = subprocess.run(
        [lillux_bin, "envelope", "open", "--key-dir", key_dir],
        input=json.dumps(envelope),
        capture_output=True,
        text=True,
    )

    if proc.returncode != 0:
        stderr = proc.stderr.strip() if proc.stderr else "unknown error"
        raise ValueError(f"lillux envelope open failed: {stderr}")

    try:
        result = json.loads(proc.stdout)
    except json.JSONDecodeError as exc:
        raise ValueError(f"lillux returned invalid JSON: {exc}") from exc

    if "error" in result:
        raise ValueError(result["error"])

    return result["env"]


def decrypt_and_inject(envelope: dict, signing_key_dir: str) -> dict:
    """Decrypt a sealed envelope using the box key in *signing_key_dir*.

    This is the high-level entry point called by ``subprocess.py`` to obtain
    an env map ready for injection into a child process.

    Args:
        envelope: The sealed envelope dict.
        signing_key_dir: Path to the directory containing ``box_key.pem``.

    Returns:
        Dict of safe env variable name → value pairs.
    """
    box_key_path = Path(signing_key_dir) / "box_key.pem"
    return decrypt_envelope(envelope, box_key_path)


# ---------------------------------------------------------------------------
# Client-side envelope sealing (Python crypto, ephemeral keys)
# ---------------------------------------------------------------------------

def _canonical_json(data: dict) -> bytes:
    """Return canonical JSON bytes: sorted keys, compact separators."""
    return json.dumps(data, sort_keys=True, separators=(",", ":")).encode()


def _derive_key(shared_secret: bytes) -> bytes:
    """Derive a 32-byte symmetric key from the X25519 shared secret."""
    hkdf = HKDF(
        algorithm=SHA256(),
        length=32,
        salt=None,
        info=b"execution-secrets/v1",
    )
    return hkdf.derive(shared_secret)


def seal_secrets(env_map: dict, recipient_box_pub: bytes) -> dict:
    """Seal an environment map to a recipient's X25519 public key.

    Args:
        env_map: Secret environment variables, e.g. ``{"API_KEY": "sk-..."}``.
        recipient_box_pub: Raw base64url-encoded X25519 public key bytes
            (as stored in ``box_pub.pem`` files).

    Returns:
        Sealed envelope dict ready for transmission.
    """
    # Decode the recipient's raw public key
    recipient_pub = X25519PublicKey.from_public_bytes(
        _b64url_decode(recipient_box_pub)
    )

    # Generate ephemeral keypair
    ephemeral_priv = X25519PrivateKey.generate()
    ephemeral_pub_bytes = ephemeral_priv.public_key().public_bytes_raw()

    # Key agreement + derivation
    shared_secret = ephemeral_priv.exchange(recipient_pub)
    symmetric_key = _derive_key(shared_secret)

    # Build AAD
    fingerprint = compute_box_fingerprint(recipient_box_pub)
    aad_fields = {
        "kind": "execution-secrets/v1",
        "recipient": f"fp:{fingerprint}",
    }
    aad = _canonical_json(aad_fields)

    # Encrypt
    plaintext = _canonical_json(env_map)
    aead = ChaCha20Poly1305(symmetric_key)
    nonce = b"\x00" * 12  # single-use key, fixed nonce is safe
    ciphertext = aead.encrypt(nonce, plaintext, aad)

    return {
        "version": 1,
        "recipient": f"fp:{fingerprint}",
        "enc": base64.urlsafe_b64encode(ephemeral_pub_bytes).decode(),
        "ciphertext": base64.urlsafe_b64encode(ciphertext).decode(),
        "aad_fields": aad_fields,
    }


def open_envelope(envelope: dict, box_key: bytes) -> dict:
    """Open a sealed envelope using the recipient's X25519 private key.

    Args:
        envelope: Sealed envelope dict as returned by :func:`seal_secrets`.
        box_key: Raw base64url-encoded X25519 private key bytes
            (as stored in ``box_key.pem`` files).

    Returns:
        Decrypted environment map dict.

    Raises:
        ValueError: On any decryption or validation failure.
    """
    try:
        # Decode keys
        recipient_priv = X25519PrivateKey.from_private_bytes(
            _b64url_decode(box_key)
        )
        ephemeral_pub = X25519PublicKey.from_public_bytes(
            _b64url_decode(envelope["enc"])
        )

        # Key agreement + derivation
        shared_secret = recipient_priv.exchange(ephemeral_pub)
        symmetric_key = _derive_key(shared_secret)

        # Reconstruct AAD
        aad = _canonical_json(envelope["aad_fields"])

        # Decrypt
        ciphertext = _b64url_decode(envelope["ciphertext"])
        aead = ChaCha20Poly1305(symmetric_key)
        nonce = b"\x00" * 12
        plaintext = aead.decrypt(nonce, ciphertext, aad)

        return json.loads(plaintext)
    except Exception as exc:
        raise ValueError(f"Failed to open sealed envelope: {exc}") from exc


def seal_secrets_for_identity(env_map: dict, identity_doc: dict) -> dict:
    """Seal secrets using a recipient's identity document.

    Extracts the X25519 box public key from the identity document
    (as returned by the ``/public-key`` endpoint) and delegates to
    :func:`seal_secrets`.

    Args:
        env_map: Secret environment variables.
        identity_doc: Identity document containing a ``box_key`` field
            in the format ``"x25519:<base64_of_raw_key_bytes>"``.

    Returns:
        Sealed envelope dict.

    Raises:
        ValueError: If the identity document has no ``box_key`` field.
    """
    box_key_str = identity_doc.get("box_key")
    if not box_key_str:
        raise ValueError("Identity document has no box_key field")

    if not box_key_str.startswith("x25519:"):
        raise ValueError(f"Unexpected box_key format: {box_key_str!r}")

    # The identity doc stores: "x25519:" + base64.b64encode(box_pub).decode()
    # where box_pub is the raw base64url-encoded bytes from box_pub.pem.
    # Decode the outer base64 to recover the original box_pub.pem content.
    recipient_box_pub = base64.b64decode(box_key_str.removeprefix("x25519:"))

    return seal_secrets(env_map, recipient_box_pub)
