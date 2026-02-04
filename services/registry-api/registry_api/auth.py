"""Authentication utilities for Registry API."""

import logging
from dataclasses import dataclass
from typing import Optional

from fastapi import Depends, HTTPException, status
from fastapi.security import HTTPAuthorizationCredentials, HTTPBearer
from jose import JWTError, jwt

from registry_api.config import Settings, get_settings

logger = logging.getLogger(__name__)

security = HTTPBearer()


@dataclass
class User:
    """Authenticated user information."""

    id: str
    email: Optional[str]
    username: str


def decode_supabase_token(token: str, settings: Settings) -> dict:
    """Decode and validate a Supabase JWT token.

    Args:
        token: The JWT token from Authorization header
        settings: Application settings with JWT secret

    Returns:
        Decoded token payload

    Raises:
        HTTPException: If token is invalid or expired
    """
    try:
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
