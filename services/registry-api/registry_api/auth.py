"""Authentication utilities for Registry API.

Primary auth: API keys (rye_sk_...) — non-interactive, used for all operations.
Bootstrap auth: Supabase JWT (OAuth/device flow) — used once to create initial API key.
"""

import hashlib
import logging
from dataclasses import dataclass
from functools import lru_cache
from typing import Optional

import httpx
from fastapi import Depends, HTTPException, Request, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from jose import JWTError, jwt
from jose.backends import ECKey

from registry_api.config import Settings, get_settings

logger = logging.getLogger(__name__)

security = HTTPBearer()
security_optional = HTTPBearer(auto_error=False)

# Cache JWKS for 1 hour
_jwks_cache: dict = {}

# API key prefix
API_KEY_PREFIX = "rye_sk_"


@dataclass
class User:
    """Authenticated user information."""

    id: str
    email: Optional[str]
    username: str


def _get_jwks(supabase_url: str) -> dict:
    """Fetch JWKS from Supabase (cached)."""
    if supabase_url in _jwks_cache:
        return _jwks_cache[supabase_url]
    
    jwks_url = f"{supabase_url}/auth/v1/.well-known/jwks.json"
    try:
        resp = httpx.get(jwks_url, timeout=10)
        resp.raise_for_status()
        jwks = resp.json()
        _jwks_cache[supabase_url] = jwks
        logger.info(f"Fetched JWKS from {jwks_url}")
        return jwks
    except Exception as e:
        logger.error(f"Failed to fetch JWKS: {e}")
        raise HTTPException(
            status_code=status.HTTP_500_INTERNAL_SERVER_ERROR,
            detail="Failed to fetch authentication keys",
        )


def _get_signing_key(jwks: dict, kid: str) -> dict:
    """Get the signing key matching the kid from JWKS."""
    for key in jwks.get("keys", []):
        if key.get("kid") == kid:
            return key
    raise HTTPException(
        status_code=status.HTTP_401_UNAUTHORIZED,
        detail="Token signing key not found",
        headers={"WWW-Authenticate": "Bearer"},
    )


# --- Bootstrap auth (JWT) ---
# Used only for initial API key creation via device auth flow.
# Once an API key exists, all subsequent requests use rye_sk_... tokens.
def decode_supabase_token(token: str, settings: Settings) -> dict:
    """Decode and validate a Supabase JWT token.

    Supports both:
    - ES256 (asymmetric) - newer Supabase tokens, verified via JWKS
    - HS256 (symmetric) - legacy tokens, verified via JWT secret

    Args:
        token: The JWT token from Authorization header
        settings: Application settings with JWT secret

    Returns:
        Decoded token payload

    Raises:
        HTTPException: If token is invalid or expired
    """
    try:
        # Peek at header to determine algorithm
        unverified_header = jwt.get_unverified_header(token)
        alg = unverified_header.get("alg", "HS256")
        
        if alg == "ES256":
            # Asymmetric - fetch JWKS and verify
            kid = unverified_header.get("kid")
            if not kid:
                raise HTTPException(
                    status_code=status.HTTP_401_UNAUTHORIZED,
                    detail="Token missing key ID",
                    headers={"WWW-Authenticate": "Bearer"},
                )
            
            jwks = _get_jwks(settings.supabase_url)
            signing_key = _get_signing_key(jwks, kid)
            
            payload = jwt.decode(
                token,
                signing_key,
                algorithms=["ES256"],
                audience="authenticated",
            )
        else:
            # HS256 - use JWT secret
            payload = jwt.decode(
                token,
                settings.supabase_jwt_secret,
                algorithms=["HS256"],
                audience="authenticated",
            )
        
        return payload
    except JWTError as e:
        logger.warning(f"JWT validation failed: {e}")
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid or expired token",
            headers={"WWW-Authenticate": "Bearer"},
        )


def _hash_api_key(key: str) -> str:
    """Compute SHA256 hash of an API key for lookup."""
    return hashlib.sha256(key.encode("utf-8")).hexdigest()


async def _resolve_api_key(token: str, settings: Settings) -> User:
    """Resolve an API key (rye_sk_...) to a User.

    Looks up the key hash in the api_keys table, validates it's active,
    updates last_used_at, and returns the associated User.
    """
    from supabase import create_client

    key_hash = _hash_api_key(token)

    supabase = create_client(settings.supabase_url, settings.supabase_service_key)

    result = (
        supabase.table("api_keys")
        .select("id, user_id, scopes, expires_at, revoked_at")
        .eq("key_hash", key_hash)
        .execute()
    )

    if not result.data:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid API key",
            headers={"WWW-Authenticate": "Bearer"},
        )

    key_record = result.data[0]

    # Check revoked
    if key_record.get("revoked_at"):
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="API key has been revoked",
            headers={"WWW-Authenticate": "Bearer"},
        )

    # Check expired
    if key_record.get("expires_at"):
        from datetime import datetime, timezone

        expires = datetime.fromisoformat(key_record["expires_at"].replace("Z", "+00:00"))
        if datetime.now(timezone.utc) > expires:
            raise HTTPException(
                status_code=status.HTTP_401_UNAUTHORIZED,
                detail="API key has expired",
                headers={"WWW-Authenticate": "Bearer"},
            )

    # Update last_used_at
    try:
        supabase.table("api_keys").update(
            {"last_used_at": "now()"}
        ).eq("id", key_record["id"]).execute()
    except Exception:
        pass  # Non-critical

    # Resolve user
    user_id = key_record["user_id"]
    user_result = (
        supabase.table("users")
        .select("id, username, email")
        .eq("id", user_id)
        .execute()
    )

    if not user_result.data:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="API key user not found",
            headers={"WWW-Authenticate": "Bearer"},
        )

    user_data = user_result.data[0]
    return User(
        id=user_data["id"],
        email=user_data.get("email"),
        username=user_data["username"],
    )


async def get_current_user(
    credentials: HTTPAuthorizationCredentials = Depends(security),
    settings: Settings = Depends(get_settings),
) -> User:
    """Extract and validate user from Bearer token.

    Auth detection order:
    1. API keys (rye_sk_...) — primary auth for all operations
    2. Supabase JWTs — bootstrap only, for initial API key creation

    Args:
        credentials: Bearer token from Authorization header
        settings: Application settings

    Returns:
        User object with id, email, and username

    Raises:
        HTTPException: If authentication fails
    """
    token = credentials.credentials

    # API key path
    if token.startswith(API_KEY_PREFIX):
        return await _resolve_api_key(token, settings)

    # JWT path
    payload = decode_supabase_token(token, settings)

    user_id = payload.get("sub")
    if not user_id:
        raise HTTPException(
            status_code=status.HTTP_401_UNAUTHORIZED,
            detail="Invalid token: missing user ID",
        )

    # Extract user metadata
    user_metadata = payload.get("user_metadata", {})
    email = payload.get("email")
    username = user_metadata.get("preferred_username")

    if not username:
        # Fallback to email prefix if no username set
        if email:
            username = email.split("@")[0]
        else:
            raise HTTPException(
                status_code=status.HTTP_400_BAD_REQUEST,
                detail="User must have a username set. Update your profile.",
            )

    return User(id=user_id, email=email, username=username)


async def get_current_user_optional(
    credentials: Optional[HTTPAuthorizationCredentials] = Depends(security_optional),
    settings: Settings = Depends(get_settings),
) -> Optional[User]:
    """Extract user from JWT token if provided, otherwise return None.
    
    Used for endpoints that work for both authenticated and unauthenticated users.
    """
    if not credentials:
        return None
    
    try:
        return await get_current_user(credentials, settings)
    except HTTPException:
        return None
