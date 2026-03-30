"""Signed domain object convention for CAS objects.

All domain CAS objects (identity/v1, execution-result/v1, webhook-binding/v1,
registry-index/v1, namespace-claim/v1) use this shared signature convention:

- Payload: canonical JSON (sorted keys, no extra whitespace)
- Signature: Ed25519 over the canonical JSON bytes
- The ``_signature`` field is excluded when computing the canonical JSON
- Stored via cas.store_object() as a regular dict

Convention:
    {
      "kind": "identity/v1",
      ...domain fields...,
      "_signature": {
        "signer": "fp:<fingerprint>",
        "sig": "<ed25519_signature_b64>",
        "signed_at": "2026-03-26T00:00:00Z"
      }
    }
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/crypto"
__tool_description__ = "Signed domain object convention utilities"

import hashlib
import json
from datetime import datetime, timezone
from pathlib import Path

from rye.primitives.signing import (
    compute_key_fingerprint,
    load_keypair,
    sign_hash,
    verify_signature,
)


def canonical_json(data: dict) -> str:
    """Return canonical JSON for signing: sorted keys, compact, ``_signature`` excluded.

    Args:
        data: Domain object dict (may or may not contain ``_signature``).

    Returns:
        Deterministic JSON string suitable for hashing.
    """
    cleaned = {k: v for k, v in data.items() if k != "_signature"}
    return json.dumps(cleaned, sort_keys=True, separators=(",", ":"))


def sign_object(
    data: dict,
    private_key_pem: bytes,
    public_key_pem: bytes,
) -> dict:
    """Sign a domain object dict with Ed25519.

    Args:
        data: Domain object dict (``_signature`` is stripped if present).
        private_key_pem: Ed25519 private key in PEM format.
        public_key_pem: Ed25519 public key in PEM format.

    Returns:
        New dict with all original fields plus ``_signature``.
    """
    payload = canonical_json(data)
    content_hash = hashlib.sha256(payload.encode()).hexdigest()

    sig_b64 = sign_hash(content_hash, private_key_pem)
    fingerprint = compute_key_fingerprint(public_key_pem)
    signed_at = datetime.now(timezone.utc).strftime("%Y-%m-%dT%H:%M:%SZ")

    return {
        **{k: v for k, v in data.items() if k != "_signature"},
        "_signature": {
            "signer": f"fp:{fingerprint}",
            "sig": sig_b64,
            "signed_at": signed_at,
        },
    }


def verify_object(signed_dict: dict, public_key_pem: bytes) -> bool:
    """Verify the Ed25519 signature on a signed domain object.

    Args:
        signed_dict: Dict containing a ``_signature`` field.
        public_key_pem: Ed25519 public key in PEM format.

    Returns:
        True if the signature is valid, False otherwise.
    """
    try:
        sig_block = signed_dict.get("_signature")
        if not sig_block:
            return False

        payload = canonical_json(signed_dict)
        content_hash = hashlib.sha256(payload.encode()).hexdigest()
        return verify_signature(content_hash, sig_block["sig"], public_key_pem)
    except Exception:
        return False


def sign_object_with_key_dir(data: dict, key_dir: Path) -> dict:
    """Sign a domain object using a keypair loaded from a directory.

    Convenience wrapper that loads ``private_key.pem`` and ``public_key.pem``
    from *key_dir*, then delegates to :func:`sign_object`.

    Args:
        data: Domain object dict.
        key_dir: Directory containing the Ed25519 keypair files.

    Returns:
        New dict with ``_signature`` attached.
    """
    private_key_pem, public_key_pem = load_keypair(key_dir)
    return sign_object(data, private_key_pem, public_key_pem)
