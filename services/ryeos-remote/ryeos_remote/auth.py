"""Authentication for ryeos-remote.

API key auth only — validates rye_sk_... keys against Supabase api_keys table.
"""

import hashlib
import logging
from dataclasses import dataclass
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


async def _resolve_api_key(token: str, settings: Settings) -> User:
    from supabase import create_client

    key_hash = hashlib.sha256(token.encode("utf-8")).hexdigest()
    supabase = create_client(settings.supabase_url, settings.supabase_service_key)

    result = (
        supabase.table("api_keys")
        .select("id, user_id, revoked_at, expires_at")
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
    return User(id=u["id"], username=u["username"], email=u.get("email"))


async def get_current_user(
    credentials: HTTPAuthorizationCredentials = Depends(security),
    settings: Settings = Depends(get_settings),
) -> User:
    token = credentials.credentials

    if token.startswith(API_KEY_PREFIX):
        return await _resolve_api_key(token, settings)

    raise HTTPException(status.HTTP_401_UNAUTHORIZED, "Invalid token — use an API key")
