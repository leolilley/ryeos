"""Shared authorized-key document helpers.

Build, sign, parse, and validate authorized-key TOML documents.
Used by both the node_keys tool (user-space) and ryeos-node (server-space).
"""

import base64
import hashlib
import re
import time
from pathlib import Path
from typing import Any, Dict, List, Optional, Tuple


# Validation patterns
_FINGERPRINT_RE = re.compile(r"^[a-f0-9]{16}$")
_UNSAFE_TOML_RE = re.compile(r'["\\\n\r]')


def validate_fingerprint(fingerprint: str) -> None:
    """Validate fingerprint format. Raises ValueError on bad format."""
    if not _FINGERPRINT_RE.match(fingerprint):
        raise ValueError(
            f"Invalid fingerprint format: {fingerprint!r}. "
            "Must be exactly 16 lowercase hex characters."
        )


def validate_label(label: str) -> None:
    """Validate label for TOML safety. Raises ValueError on injection risk."""
    if _UNSAFE_TOML_RE.search(label):
        raise ValueError(
            f"Invalid label: contains quotes, backslashes, or newlines."
        )
    if len(label) > 128:
        raise ValueError("Label too long (max 128 characters).")


def validate_scopes(scopes: List[str]) -> None:
    """Validate scope strings for TOML safety. Raises ValueError on injection."""
    for scope in scopes:
        if _UNSAFE_TOML_RE.search(scope):
            raise ValueError(
                f"Invalid scope {scope!r}: contains quotes, backslashes, or newlines."
            )
        if len(scope) > 256:
            raise ValueError(f"Scope too long (max 256 characters): {scope!r}")


def build_authorized_key_body(
    fingerprint: str,
    public_key_encoded: str,
    label: str = "unnamed",
    scopes: Optional[List[str]] = None,
    extra_fields: Optional[Dict[str, str]] = None,
) -> Tuple[str, str]:
    """Build authorized-key TOML body with safe serialization.

    Args:
        fingerprint: Key fingerprint (16 hex chars)
        public_key_encoded: The public key in "ed25519:<base64>" format
        label: Human-readable label
        scopes: Access scopes (default: ["*"])
        extra_fields: Additional TOML key-value pairs to include

    Returns:
        Tuple of (body_text, timestamp) where body_text is the TOML content
        without the signature header.

    Raises:
        ValueError: If any field fails validation.
    """
    if scopes is None:
        scopes = ["*"]

    validate_fingerprint(fingerprint)
    validate_label(label)
    validate_scopes(scopes)

    timestamp = time.strftime("%Y-%m-%dT%H:%M:%SZ", time.gmtime())
    scopes_str = ", ".join(f'"{s}"' for s in scopes)

    body = (
        f'fingerprint = "{fingerprint}"\n'
        f'public_key = "{public_key_encoded}"\n'
        f'label = "{label}"\n'
        f'scopes = [{scopes_str}]\n'
    )

    if extra_fields:
        for k, v in extra_fields.items():
            if _UNSAFE_TOML_RE.search(k) or _UNSAFE_TOML_RE.search(v):
                raise ValueError(
                    f"Invalid extra_fields: key or value contains quotes, "
                    "backslashes, or newlines."
                )
            body += f'{k} = "{v}"\n'

    body += f'created_at = "{timestamp}"\n'

    return body, timestamp


def sign_authorized_key(
    body: str,
    timestamp: str,
    private_key: bytes,
    public_key: bytes,
) -> str:
    """Sign an authorized-key document body.

    Args:
        body: TOML body text (from build_authorized_key_body)
        timestamp: ISO timestamp for the signature header
        private_key: Signer's private key PEM bytes
        public_key: Signer's public key PEM bytes

    Returns:
        Full signed document (signature header + body)
    """
    from rye.primitives.signing import compute_key_fingerprint, sign_hash

    content_hash = hashlib.sha256(body.encode()).hexdigest()
    sig_b64 = sign_hash(content_hash, private_key)
    signer_fp = compute_key_fingerprint(public_key)

    return f"# rye:signed:{timestamp}:{content_hash}:{sig_b64}:{signer_fp}\n{body}"


def build_and_sign_authorized_key(
    public_key_pem: bytes,
    signer_private: bytes,
    signer_public: bytes,
    label: str = "unnamed",
    scopes: Optional[List[str]] = None,
    extra_fields: Optional[Dict[str, str]] = None,
) -> Tuple[str, str]:
    """Build and sign a complete authorized-key document.

    Convenience function combining build + sign.

    Args:
        public_key_pem: The public key PEM bytes of the key being authorized
        signer_private: Private key PEM of the signing authority (node key)
        signer_public: Public key PEM of the signing authority
        label: Human-readable label
        scopes: Access scopes
        extra_fields: Additional TOML fields

    Returns:
        Tuple of (signed_document, fingerprint)
    """
    from rye.primitives.signing import compute_key_fingerprint

    fingerprint = compute_key_fingerprint(public_key_pem)
    pub_b64 = base64.b64encode(public_key_pem).decode("ascii")
    public_key_encoded = f"ed25519:{pub_b64}"

    body, timestamp = build_authorized_key_body(
        fingerprint=fingerprint,
        public_key_encoded=public_key_encoded,
        label=label,
        scopes=scopes,
        extra_fields=extra_fields,
    )

    signed = sign_authorized_key(body, timestamp, signer_private, signer_public)
    return signed, fingerprint


def parse_authorized_key(raw: str) -> Dict[str, Any]:
    """Parse an authorized-key TOML file (with or without signature header).

    Returns the parsed dict. Does NOT verify the signature — use
    verify_authorized_key() for that.
    """
    try:
        import tomllib
    except ModuleNotFoundError:
        import tomli as tomllib  # type: ignore[no-redef]

    lines = raw.split("\n", 1)
    body = lines[1] if len(lines) > 1 and lines[0].startswith("# rye:signed:") else raw
    return tomllib.loads(body)


def verify_authorized_key_signature(
    raw: str,
    signer_public: bytes,
) -> Dict[str, Any]:
    """Parse and verify an authorized-key document's signature.

    Args:
        raw: Full document text (signature header + body)
        signer_public: Expected signer's public key PEM bytes

    Returns:
        Parsed TOML dict if valid.

    Raises:
        ValueError: If signature is missing, malformed, or invalid.
    """
    from rye.primitives.signing import compute_key_fingerprint, verify_signature

    lines = raw.split("\n", 1)
    sig_line = lines[0].strip()
    if not sig_line.startswith("# rye:signed:"):
        raise ValueError("Missing signature header")

    remainder = sig_line[len("# rye:signed:"):]
    rparts = remainder.rsplit(":", 3)
    if len(rparts) != 4:
        raise ValueError("Malformed signature header")

    _sig_timestamp, content_hash, sig_b64, signer_fp = rparts

    expected_fp = compute_key_fingerprint(signer_public)
    if signer_fp != expected_fp:
        raise ValueError(f"Wrong signer: expected {expected_fp}, got {signer_fp}")

    body = lines[1] if len(lines) > 1 else ""
    actual_hash = hashlib.sha256(body.encode()).hexdigest()
    if actual_hash != content_hash:
        raise ValueError("Content hash mismatch (tampered)")

    if not verify_signature(content_hash, sig_b64, signer_public):
        raise ValueError("Invalid signature")

    try:
        import tomllib
    except ModuleNotFoundError:
        import tomli as tomllib  # type: ignore[no-redef]

    return tomllib.loads(body)
