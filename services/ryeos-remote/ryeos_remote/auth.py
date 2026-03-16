"""Authentication for ryeos-remote.

Dual auth: API key (bearer) and HMAC (webhook).
- Bearer: validates rye_sk_... keys against Supabase api_keys table.
- Webhook: HMAC-SHA256 signature verification via webhook_bindings table.
"""

import hashlib
import hmac as hmac_mod
import logging
import time
from dataclasses import dataclass, field
from typing import Optional

from fastapi import Depends, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer

from ryeos_remote.config import Settings, get_settings

logger = logging.getLogger(__name__)

security = HTTPBearer()

API_KEY_PREFIX = "rye_" + "sk_"


@dataclass
class User:
    id: str
    username: str
    email: Optional[str] = None
    scopes: list[str] | None = None


async def _resolve_api_key(token: str, settings: Settings) -> User:
    from supabase import create_client

    key_hash = hashlib.sha256(token.encode("utf-8")).hexdigest()
    supabase = create_client(settings.supabase_url, settings.supabase_service_key)

    result = (
        supabase.table("api_keys")
        .select("id, user_id, scopes, revoked_at, expires_at")
        .eq("key_hash", key_hash)
        .execute()
    )
    if not result.data:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid API key")

    record = result.data[0]
    if record.get("revoked_at"):
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "API key revoked")
    if record.get("expires_at"):
        from datetime import datetime, timezone

        exp = datetime.fromisoformat(record["expires_at"].replace("Z", "+00:00"))
        if datetime.now(timezone.utc) > exp:
            raise HTTPException(status.HTTP_401_UNAUTHORIZED, "API key expired")

    user_result = (
        supabase.table("users")
        .select("id, username, email")
        .eq("id", record["user_id"])
        .execute()
    )
    if not user_result.data:
        raise HTTPException(status.HTTP_401_UNAUTHORIZED, "User not found")

    u = user_result.data[0]
    return User(id=u["id"], username=u["username"], email=u.get("email"), scopes=record.get("scopes"))


def require_scope(user: User, required: str) -> None:
    """Raise 403 if user's key doesn't have the required scope.

    Scope format: 'service:action' (e.g., 'remote:execute', 'remote:*', 'registry:read').
    A scope of 'service:*' grants all actions for that service.
    Keys with no scopes get default backward-compatible access.
    """
    if user.scopes is None:
        return  # No scopes = unrestricted (shouldn't happen with DB default)

    service, _, action = required.partition(":")

    # Check for exact match or wildcard
    if required in user.scopes or f"{service}:*" in user.scopes:
        return

    raise HTTPException(
        status_code=status.HTTP_403_FORBIDDEN,
        detail=f"API key missing required scope: {required}",
    )


async def get_current_user(
    credentials: HTTPAuthorizationCredentials = Depends(security),
    settings: Settings = Depends(get_settings),
) -> User:
    token = credentials.credentials

    if token.startswith(API_KEY_PREFIX):
        return await _resolve_api_key(token, settings)

    raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid token — use an API key")


# ---------------------------------------------------------------------------
# HMAC webhook verification
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
# Replay protection (DB-backed for horizontal scaling)
# ---------------------------------------------------------------------------

REPLAY_TTL_SECONDS = 600  # 10 minutes


def check_replay(hook_id: str, delivery_id: str, settings=None) -> None:
    """Reject duplicate webhook deliveries using persistent DB store.

    Uses webhook_deliveries_replay table with (hook_id, delivery_id) unique key.
    Insert succeeds → first delivery. Insert conflicts → duplicate.
    """
    if not delivery_id:
        raise HTTPException(
            status.HTTP_400_BAD_REQUEST,
            "X-Webhook-Delivery-Id header required",
        )

    if settings is None:
        from ryeos_remote.config import get_settings
        settings = get_settings()

    from supabase import create_client
    sb = create_client(settings.supabase_url, settings.supabase_service_key)

    # Check if already processed
    existing = (
        sb.table("webhook_deliveries_replay")
        .select("delivery_id")
        .eq("hook_id", hook_id)
        .eq("delivery_id", delivery_id)
        .execute()
    )
    if existing.data:
        raise HTTPException(
            status.HTTP_200_OK,
            "Already processed",
        )

    # Insert — if concurrent insert races, the unique constraint catches it
    try:
        sb.table("webhook_deliveries_replay").insert({
            "hook_id": hook_id,
            "delivery_id": delivery_id,
        }).execute()
    except Exception:
        # Unique constraint violation = concurrent duplicate
        raise HTTPException(
            status.HTTP_200_OK,
            "Already processed",
        )


# ---------------------------------------------------------------------------
# ResolvedExecution — normalized result from dual-auth
# ---------------------------------------------------------------------------


@dataclass
class ResolvedExecution:
    """Normalized execution request after auth resolution.

    Both bearer and webhook paths produce this. The /execute handler
    doesn't know or care which auth path was used.
    """
    user: User
    item_type: str
    item_id: str
    project_path: str
    parameters: dict
    thread: str
