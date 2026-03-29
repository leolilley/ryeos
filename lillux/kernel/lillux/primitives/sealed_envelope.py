"""Sealed envelope decryption for subprocess secret injection.

Decrypts secret envelopes sealed to a node's X25519 box key,
validates the resulting env map, and returns safe entries for
subprocess environment injection.
"""

import base64
import json
import logging
from pathlib import Path

from cryptography.hazmat.primitives.asymmetric.x25519 import (
    X25519PrivateKey,
    X25519PublicKey,
)
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.hashes import SHA256
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

log = logging.getLogger(__name__)


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
# Envelope decryption
# ---------------------------------------------------------------------------

_HKDF_INFO = b"execution-secrets/v1"


def decrypt_envelope(envelope: dict, box_key_path: Path) -> dict:
    """Decrypt a sealed secret envelope and return a validated env map.

    Args:
        envelope: The sealed envelope dict (version, enc, ciphertext, aad_fields, …).
        box_key_path: Path to the ``box_key.pem`` file containing the raw
            base64url-encoded X25519 private key.

    Returns:
        Dict of safe env variable name → value pairs.

    Raises:
        ValueError: On version mismatch, missing fields, decryption failure,
            or env-map validation errors.
    """
    # -- version gate -------------------------------------------------------
    version = envelope.get("version")
    if version != 1:
        raise ValueError(f"unsupported envelope version: {version}")

    for field in ("enc", "ciphertext", "aad_fields"):
        if field not in envelope:
            raise ValueError(f"envelope missing required field: {field}")

    aad_fields = envelope.get("aad_fields", {})
    if aad_fields.get("kind") != "execution-secrets/v1":
        raise ValueError(f"unexpected envelope kind: {aad_fields.get('kind')!r}")

    # -- load recipient private key -----------------------------------------
    raw_key_b64 = box_key_path.read_bytes().strip()
    private_bytes = _b64url_decode(raw_key_b64)
    private_key = X25519PrivateKey.from_private_bytes(private_bytes)

    # -- reconstruct shared secret ------------------------------------------
    ephemeral_pub_bytes = _b64url_decode(envelope["enc"])
    ephemeral_pub = X25519PublicKey.from_public_bytes(ephemeral_pub_bytes)
    shared_secret = private_key.exchange(ephemeral_pub)

    # -- derive symmetric key via HKDF-SHA256 -------------------------------
    symmetric_key = HKDF(
        algorithm=SHA256(),
        length=32,
        salt=None,
        info=_HKDF_INFO,
    ).derive(shared_secret)

    # -- reconstruct AAD (canonical JSON: sorted keys, compact) -------------
    aad = json.dumps(
        envelope["aad_fields"], sort_keys=True, separators=(",", ":")
    ).encode()

    # -- decrypt ciphertext with ChaCha20Poly1305 ---------------------------
    # Nonce is fixed to zero — safe because the symmetric key is
    # single-use (derived from an ephemeral X25519 keypair).
    ciphertext = _b64url_decode(envelope["ciphertext"])
    nonce = b"\x00" * 12
    try:
        plaintext = ChaCha20Poly1305(symmetric_key).decrypt(nonce, ciphertext, aad)
    except Exception as exc:
        raise ValueError(f"envelope decryption failed: {exc}") from exc

    try:
        env_map = json.loads(plaintext)
    except json.JSONDecodeError as exc:
        raise ValueError(f"decrypted payload is not valid JSON: {exc}") from exc

    if not isinstance(env_map, dict):
        raise ValueError("decrypted payload is not a JSON object")

    # -- validate -----------------------------------------------------------
    errors = validate_env_map(env_map)
    if errors:
        raise ValueError(
            f"env map validation failed: {'; '.join(errors)}"
        )

    # -- filter unsafe names ------------------------------------------------
    safe: dict[str, str] = {}
    for name, value in env_map.items():
        if is_safe_secret_name(name):
            safe[name] = value
        else:
            log.warning("skipping unsafe secret name: %s", name)

    return safe


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
