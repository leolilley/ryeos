"""Authentication for ryeos-remote.

Dual auth: signed-request (Ed25519) and HMAC (webhook).
- Signed-request: verifies X-Rye-Signature headers against authorized key files.
- Webhook: HMAC-SHA256 signature verification via webhook_bindings table.
"""

import fnmatch
import hashlib
import hmac as hmac_mod
import logging
import time
from dataclasses import dataclass
from pathlib import Path
from typing import Optional

try:
    import tomllib
except ModuleNotFoundError:
    import tomli as tomllib  # type: ignore[no-redef]

from fastapi import Depends, HTTPException, Request, status

from ryeos_remote.config import Settings, get_settings

logger = logging.getLogger(__name__)


# ---------------------------------------------------------------------------
# Principal (replaces User)
# ---------------------------------------------------------------------------


@dataclass
class Principal:
    """Authenticated caller identity.

    fingerprint: Ed25519 key fingerprint (the identity)
    capabilities: fnmatch patterns from authorized key file
    owner: human-readable label from authorized key file
    """
    fingerprint: str
    capabilities: list[str]
    owner: str = ""


# ---------------------------------------------------------------------------
# Authorized key file loading + verification
# ---------------------------------------------------------------------------


def _load_authorized_key(fingerprint: str, settings: Settings) -> dict:
    """Load and verify an authorized key TOML file.

    The file must be signed by this node's key (signature header line).
    Returns the parsed TOML dict.
    Raises HTTPException(401) on any failure.
    """
    auth_dir = settings.authorized_keys_dir()
    key_file = auth_dir / f"{fingerprint}.toml"

    if not key_file.exists():
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unknown principal")

    raw = key_file.read_text()

    # Verify node signature (first line: # rye:signed:<timestamp>:<hash>:<sig>:<signer_fp>)
    lines = raw.split("\n", 1)
    sig_line = lines[0].strip()
    if not sig_line.startswith("# rye:signed:"):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (unsigned)")

    parts = sig_line[len("# rye:signed:"):].split(":", 3)
    if len(parts) != 4:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (malformed sig)")

    _sig_timestamp, content_hash, sig_b64, signer_fp = parts

    # Verify signature was made by this node's key
    from lillux.primitives.signing import load_keypair, compute_key_fingerprint, verify_signature

    try:
        _, node_pub = load_keypair(Path(settings.signing_key_dir))
        node_fp = compute_key_fingerprint(node_pub)
    except FileNotFoundError:
        raise HTTPException(
            status.HTTP_500_INTERNAL_SERVER_ERROR,
            "Node signing key not configured",
        )

    if signer_fp != node_fp:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (wrong signer)")

    # The signed content is everything after the signature line
    body = lines[1] if len(lines) > 1 else ""
    actual_hash = hashlib.sha256(body.encode()).hexdigest()
    if actual_hash != content_hash:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (tampered)")

    if not verify_signature(content_hash, sig_b64, node_pub):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (bad signature)")

    # Parse TOML body
    try:
        data = tomllib.loads(body)
    except Exception:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (invalid TOML)")

    # Verify fingerprint matches
    if data.get("fingerprint") != fingerprint:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Unauthorized key file (fingerprint mismatch)")

    # Check expiry
    expires_at = data.get("expires_at")
    if expires_at:
        from datetime import datetime, timezone
        try:
            exp = datetime.fromisoformat(expires_at.replace("Z", "+00:00"))
            if datetime.now(timezone.utc) > exp:
                raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Authorized key expired")
        except (ValueError, AttributeError):
            raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Authorized key file (bad expiry)")

    return data


def _verify_signed_request(request: Request, raw_body: bytes, settings: Settings) -> Principal:
    """Verify Ed25519 signed request headers.

    Extracts the caller's fingerprint from X-Rye-Key-Id, loads their
    authorized key file, verifies the request signature, checks replay,
    and returns a Principal.
    """
    from ryeos_remote.replay import get_replay_guard

    key_id = request.headers.get("x-rye-key-id", "")
    timestamp = request.headers.get("x-rye-timestamp", "")
    nonce = request.headers.get("x-rye-nonce", "")
    signature = request.headers.get("x-rye-signature", "")

    if not all([key_id, timestamp, nonce, signature]):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Missing auth headers")

    # Extract fingerprint from key_id (format: fp:<fingerprint>)
    if not key_id.startswith("fp:"):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid key ID format")
    fingerprint = key_id[3:]

    # Check timestamp freshness
    try:
        req_time = int(timestamp)
    except ValueError:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid timestamp")
    now = int(time.time())
    if abs(now - req_time) > 300:  # 5 minute window
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Request expired")

    # Load authorized key file (verifies node signature, expiry)
    auth_data = _load_authorized_key(fingerprint, settings)

    # Get caller's public key from authorized key file
    public_key_b64 = auth_data.get("public_key", "")
    if not public_key_b64.startswith("ed25519:"):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid public key format")
    # The public key PEM is stored as base64 after "ed25519:" prefix
    # But we stored it as the actual PEM content after the prefix
    import base64
    public_key_pem = base64.b64decode(public_key_b64[8:])

    # Compute this node's audience (fp:<node_fingerprint>)
    from lillux.primitives.signing import load_keypair, compute_key_fingerprint, verify_signature

    _, node_pub = load_keypair(Path(settings.signing_key_dir))
    node_fp = compute_key_fingerprint(node_pub)
    audience = f"fp:{node_fp}"

    # Reconstruct string_to_sign and verify
    body_hash = hashlib.sha256(raw_body or b"").hexdigest()

    # Build canonical path from request
    path = request.url.path
    query = str(request.url.query) if request.url.query else ""
    if query:
        from urllib.parse import parse_qsl, urlencode
        params = parse_qsl(query, keep_blank_values=True)
        params.sort()
        canon_path = f"{path}?{urlencode(params)}"
    else:
        canon_path = path

    string_to_sign = "\n".join([
        "ryeos-request-v1",
        request.method.upper(),
        canon_path,
        body_hash,
        timestamp,
        nonce,
        audience,
    ])

    content_hash = hashlib.sha256(string_to_sign.encode()).hexdigest()
    if not verify_signature(content_hash, signature, public_key_pem):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid signature")

    # Replay check
    guard = get_replay_guard(settings.cas_base_path)
    if not guard.check_and_record(fingerprint, nonce):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Replayed request")

    return Principal(
        fingerprint=fingerprint,
        capabilities=auth_data.get("capabilities", []),
        owner=auth_data.get("owner", ""),
    )


def require_capability(principal: Principal, action: str) -> None:
    """Raise 403 if principal doesn't have a matching capability.

    Capabilities use fnmatch patterns (e.g., 'rye.execute.tool.*').
    """
    for cap in principal.capabilities:
        if fnmatch.fnmatch(action, cap):
            return
    raise HTTPException(
        status_code=status.HTTP_403_FORBIDDEN,
        detail=f"Missing required capability: {action}",
    )


async def get_current_principal(
    request: Request,
    settings: Settings = Depends(get_settings),
) -> Principal:
    """FastAPI dependency: authenticate via signed request headers."""
    raw_body = await request.body()
    return _verify_signed_request(request, raw_body, settings)


# ---------------------------------------------------------------------------
# HMAC webhook verification (unchanged — external services don't have Ed25519)
# ---------------------------------------------------------------------------

WEBHOOK_TIMESTAMP_MAX_FUTURE_SECONDS = 30
WEBHOOK_TIMESTAMP_MAX_AGE_SECONDS = 300


def verify_timestamp(timestamp: str) -> None:
    """Reject stale or future webhook timestamps."""
    if not timestamp:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    try:
        ts = int(timestamp)
    except ValueError:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    now = int(time.time())
    if ts > now + WEBHOOK_TIMESTAMP_MAX_FUTURE_SECONDS:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    if now - ts > WEBHOOK_TIMESTAMP_MAX_AGE_SECONDS:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")


def verify_hmac(timestamp: str, raw_body: bytes, secret: str, signature: str) -> None:
    """Verify HMAC-SHA256 signature over timestamp.raw_body."""
    if not signature or not signature.startswith("sha256="):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    received = signature[7:]
    if len(received) != 64:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")
    signed = timestamp.encode() + b"." + raw_body
    expected = hmac_mod.new(
        secret.encode(),
        signed,
        hashlib.sha256,
    ).hexdigest()
    if not hmac_mod.compare_digest(expected, received):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid webhook auth")


# ---------------------------------------------------------------------------
# ResolvedExecution — normalized result from dual-auth
# ---------------------------------------------------------------------------


@dataclass
class ResolvedExecution:
    """Normalized execution request after auth resolution.

    Both signed-request and webhook paths produce this. The /execute handler
    doesn't know or care which auth path was used.
    """
    principal: Principal
    item_type: str
    item_id: str
    project_path: str
    parameters: dict
    thread: str
