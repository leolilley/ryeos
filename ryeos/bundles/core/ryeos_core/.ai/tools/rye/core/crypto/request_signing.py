"""Per-request Ed25519 signing for outbound HTTP calls.

Shared utility importable by core bundle tools that make authenticated
outbound requests (remote, registry). Not a standalone tool.

Request signature format:
    string_to_sign = "ryeos-request-v1\\n" +
                     METHOD + "\\n" +
                     CANONICAL_PATH + "\\n" +
                     SHA256(BODY) + "\\n" +
                     TIMESTAMP + "\\n" +
                     NONCE + "\\n" +
                     AUDIENCE_NODE_ID

Headers:
    X-Rye-Key-Id:    fp:<fingerprint>
    X-Rye-Timestamp: <unix_timestamp>
    X-Rye-Nonce:     <random_hex>
    X-Rye-Signature: <ed25519_signature_b64>
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/core/crypto"
__tool_description__ = "Per-request Ed25519 signing for HTTP calls"

import hashlib
import os
import time
from typing import Dict, Optional
from urllib.parse import urlparse, parse_qsl, urlencode

from rye.primitives.signing import (
    compute_key_fingerprint,
    sign_hash,
)


def canonical_path(url_or_path: str) -> str:
    """Build canonical path from URL or path string.

    Canonical path = path + sorted query string (if any).

    Args:
        url_or_path: Full URL or just the path portion.

    Returns:
        Canonical path string.
    """
    parsed = urlparse(url_or_path)
    path = parsed.path or "/"

    if parsed.query:
        # Sort query parameters for canonical form
        params = parse_qsl(parsed.query, keep_blank_values=True)
        params.sort()
        return f"{path}?{urlencode(params)}"

    return path


def sign_request(
    method: str,
    url_or_path: str,
    body: Optional[bytes],
    audience: str,
    private_key_pem: bytes,
    public_key_pem: bytes,
) -> Dict[str, str]:
    """Sign an outbound HTTP request with Ed25519.

    Args:
        method: HTTP method (GET, POST, etc.)
        url_or_path: Full URL or path (query string is canonicalized)
        body: Request body bytes (None or b"" for empty)
        audience: Target node ID (fp:<fingerprint> of the recipient node)
        private_key_pem: Caller's Ed25519 private key PEM
        public_key_pem: Caller's Ed25519 public key PEM

    Returns:
        Dict of HTTP headers to add to the request.
    """
    fingerprint = compute_key_fingerprint(public_key_pem)
    timestamp = str(int(time.time()))
    nonce = os.urandom(16).hex()

    body_hash = hashlib.sha256(body or b"").hexdigest()
    canon_path = canonical_path(url_or_path)

    string_to_sign = "\n".join([
        "ryeos-request-v1",
        method.upper(),
        canon_path,
        body_hash,
        timestamp,
        nonce,
        audience,
    ])

    content_hash = hashlib.sha256(string_to_sign.encode()).hexdigest()
    signature = sign_hash(content_hash, private_key_pem)

    return {
        "X-Rye-Key-Id": f"fp:{fingerprint}",
        "X-Rye-Timestamp": timestamp,
        "X-Rye-Nonce": nonce,
        "X-Rye-Signature": signature,
    }


def verify_request_signature(
    method: str,
    url_or_path: str,
    body: Optional[bytes],
    audience: str,
    headers: Dict[str, str],
    public_key_pem: bytes,
    *,
    max_age_seconds: int = 300,
) -> bool:
    """Verify an inbound signed request.

    Args:
        method: HTTP method
        url_or_path: Request path (with query string)
        body: Request body bytes
        audience: This node's ID (fp:<fingerprint>)
        headers: Request headers (must contain X-Rye-* headers)
        public_key_pem: Caller's Ed25519 public key PEM
        max_age_seconds: Maximum age of request in seconds (default: 300 = 5 min)

    Returns:
        True if signature is valid and request is fresh, False otherwise.
    """
    try:
        from rye.primitives.signing import verify_signature

        key_id = headers.get("X-Rye-Key-Id", headers.get("x-rye-key-id", ""))
        timestamp = headers.get("X-Rye-Timestamp", headers.get("x-rye-timestamp", ""))
        nonce = headers.get("X-Rye-Nonce", headers.get("x-rye-nonce", ""))
        signature = headers.get("X-Rye-Signature", headers.get("x-rye-signature", ""))

        if not all([key_id, timestamp, nonce, signature]):
            return False

        # Check freshness
        req_time = int(timestamp)
        now = int(time.time())
        if abs(now - req_time) > max_age_seconds:
            return False

        body_hash = hashlib.sha256(body or b"").hexdigest()
        canon_path = canonical_path(url_or_path)

        string_to_sign = "\n".join([
            "ryeos-request-v1",
            method.upper(),
            canon_path,
            body_hash,
            timestamp,
            nonce,
            audience,
        ])

        content_hash = hashlib.sha256(string_to_sign.encode()).hexdigest()
        return verify_signature(content_hash, signature, public_key_pem)
    except Exception:
        return False
