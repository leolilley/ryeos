"""Sealed secret envelope construction (HPKE-style).

Shared utility importable by core bundle tools (like the remote tool)
for encrypting secret environment variables to a specific recipient node.
Not a standalone tool.

Envelope format:
    {
      "version": 1,
      "recipient": "fp:<node_fingerprint>",
      "enc": "<ephemeral_public_key_b64url>",
      "ciphertext": "<encrypted_env_map_b64url>",
      "aad_fields": {
        "kind": "execution-secrets/v1",
        "recipient": "fp:<node_fingerprint>"
      }
    }

Construction:
    - X25519 ephemeral key exchange
    - HKDF-SHA256 key derivation (info: b"execution-secrets/v1")
    - ChaCha20Poly1305 AEAD encryption
    - AAD = canonical JSON of aad_fields
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/crypto"
__tool_description__ = "Sealed secret envelope construction for execution secrets"

import base64
import json


def _b64url_decode(data: bytes | str) -> bytes:
    """Decode base64url with or without padding."""
    if isinstance(data, str):
        data = data.encode()
    # Add padding if missing
    data += b"=" * (-len(data) % 4)
    return base64.urlsafe_b64decode(data)

from cryptography.hazmat.primitives.asymmetric.x25519 import (
    X25519PrivateKey,
    X25519PublicKey,
)
from cryptography.hazmat.primitives.ciphers.aead import ChaCha20Poly1305
from cryptography.hazmat.primitives.hashes import SHA256
from cryptography.hazmat.primitives.kdf.hkdf import HKDF

from rye.primitives.signing import compute_box_fingerprint


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
