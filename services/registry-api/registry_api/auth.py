"""Authentication utilities for Registry API."""

import logging
from dataclasses import dataclass
from functools import lru_cache
from typing import Optional

import httpx
from fastapi import Depends, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from jose import JWTError, jwt
from jose.backends import ECKey

from registry_api.config import Settings, get_settings

logger = logging.getLogger(__name__)

security = HTTPBearer()

# Cache JWKS for 1 hour
_jwks_cache: dict = {}


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


async def get_current_user(
    credentials: HTTPAuthorizationCredentials = Depends(security),
    settings: Settings = Depends(get_settings),
) -> User:
    """Extract and validate user from JWT token.

    Args:
        credentials: Bearer token from Authorization header
        settings: Application settings

    Returns:
        User object with id, email, and username

    Raises:
        HTTPException: If authentication fails
    """
    token = credentials.credentials
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
