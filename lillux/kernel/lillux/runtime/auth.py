"""Authentication store service (Phase 4.2).

Secure credential management with file-based encrypted storage fallback.
Uses OS keychain when available, otherwise stores encrypted tokens in {USER_SPACE}/.ai/auth/.
"""

import base64
import hashlib
import json
import os
import stat
import time
from pathlib import Path
from typing import Any, Dict, List, Optional

import httpx

try:
    import keyring
    from keyring.backends.fail import Keyring as FailKeyring
    # Check if keyring has a working backend
    _keyring_backend = keyring.get_keyring()
    KEYRING_AVAILABLE = not isinstance(_keyring_backend, FailKeyring)
except ImportError:
    keyring = None
    KEYRING_AVAILABLE = False

try:
    from cryptography.fernet import Fernet
    from cryptography.hazmat.primitives import hashes
    from cryptography.hazmat.primitives.kdf.pbkdf2 import PBKDF2HMAC
    CRYPTO_AVAILABLE = True
except ImportError:
    CRYPTO_AVAILABLE = False

from lillux.primitives.errors import AuthenticationRequired, RefreshError


def _get_auth_dir() -> Path:
    """Get auth storage directory ({USER_SPACE}/.ai/auth/)."""
    user_space = os.environ.get("USER_SPACE", str(Path.home()))
    auth_dir = Path(user_space) / ".ai" / "auth"
    auth_dir.mkdir(parents=True, exist_ok=True)
    # Secure permissions (owner only)
    auth_dir.chmod(stat.S_IRWXU)
    return auth_dir


def _derive_key(salt: bytes) -> bytes:
    """Derive encryption key from machine-specific data.
    
    Uses a combination of username, hostname, and a fixed salt to create
    a deterministic key. This isn't high security, but prevents casual
    reading of tokens and is better than plaintext.
    """
    if not CRYPTO_AVAILABLE:
        raise RuntimeError("cryptography library required for file-based auth storage")
    
    # Machine-specific seed (not secret, just unique per machine)
    seed = f"{os.getlogin()}@{os.uname().nodename}:lillux-auth".encode()
    
    kdf = PBKDF2HMAC(
        algorithm=hashes.SHA256(),
        length=32,
        salt=salt,
        iterations=100_000,
    )
    return base64.urlsafe_b64encode(kdf.derive(seed))


class AuthStore:
    """Secure credential management using OS keychain or encrypted files."""

    def __init__(self, service_name: str = "lillux"):
        """Initialize auth store with service name.
        
        Args:
            service_name: Service name for keychain/file storage.
        """
        self.service_name = service_name
        self._metadata_cache: Dict[str, Dict[str, Any]] = {}
        self._use_keyring = KEYRING_AVAILABLE
        
        # Initialize file-based storage if keyring unavailable
        if not self._use_keyring and CRYPTO_AVAILABLE:
            self._auth_dir = _get_auth_dir()
            self._salt_file = self._auth_dir / ".salt"
            self._salt = self._get_or_create_salt()
        else:
            self._auth_dir = None
            self._salt = None

    def _get_or_create_salt(self) -> bytes:
        """Get or create salt for key derivation."""
        if self._salt_file.exists():
            return self._salt_file.read_bytes()
        salt = os.urandom(16)
        self._salt_file.write_bytes(salt)
        self._salt_file.chmod(stat.S_IRUSR | stat.S_IWUSR)
        return salt

    def _get_token_path(self, service: str) -> Path:
        """Get path for token file."""
        # Hash service name to avoid special chars in filename
        name_hash = hashlib.sha256(f"{self.service_name}_{service}".encode()).hexdigest()[:16]
        return self._auth_dir / f"{name_hash}.token"

    def _encrypt(self, data: str) -> bytes:
        """Encrypt data using Fernet."""
        key = _derive_key(self._salt)
        f = Fernet(key)
        return f.encrypt(data.encode())

    def _decrypt(self, data: bytes) -> str:
        """Decrypt data using Fernet."""
        key = _derive_key(self._salt)
        f = Fernet(key)
        return f.decrypt(data).decode()

    def _write_file_token(self, service: str, token_data: Dict[str, Any]) -> bool:
        """Write token to encrypted file."""
        if not self._auth_dir or not self._salt:
            return False
        try:
            token_path = self._get_token_path(service)
            encrypted = self._encrypt(json.dumps(token_data))
            token_path.write_bytes(encrypted)
            token_path.chmod(stat.S_IRUSR | stat.S_IWUSR)
            return True
        except Exception:
            return False

    def _read_file_token(self, service: str) -> Optional[Dict[str, Any]]:
        """Read token from encrypted file."""
        if not self._auth_dir or not self._salt:
            return None
        try:
            token_path = self._get_token_path(service)
            if not token_path.exists():
                return None
            encrypted = token_path.read_bytes()
            decrypted = self._decrypt(encrypted)
            return json.loads(decrypted)
        except Exception:
            return None

    def _delete_file_token(self, service: str) -> None:
        """Delete token file."""
        if not self._auth_dir:
            return
        try:
            token_path = self._get_token_path(service)
            if token_path.exists():
                token_path.unlink()
        except Exception:
            pass

    def set_token(
        self,
        service: str,
        access_token: str,
        refresh_token: Optional[str] = None,
        expires_in: int = 3600,
        scopes: Optional[List[str]] = None,
        refresh_config: Optional[Dict[str, str]] = None,
    ) -> None:
        """Store token securely.
        
        Uses OS keychain if available, otherwise encrypted file storage.
        
        Args:
            service: Service identifier.
            access_token: Access token to store.
            refresh_token: Optional refresh token for OAuth2.
            expires_in: Token expiry in seconds from now.
            scopes: Optional list of scopes for this token.
            refresh_config: Optional OAuth2 refresh config.
        """
        expires_at = time.time() + expires_in

        token_data = {
            "access_token": access_token,
            "expires_at": expires_at,
            "scopes": scopes or [],
        }
        if refresh_token:
            token_data["refresh_token"] = refresh_token
        if refresh_config:
            token_data["refresh_config"] = refresh_config

        stored = False
        
        if self._use_keyring and keyring:
            access_key = f"{self.service_name}_{service}_access_token"
            try:
                keyring.set_password(self.service_name, access_key, json.dumps(token_data))
                stored = True
            except Exception:
                pass

        if not stored:
            stored = self._write_file_token(service, token_data)

        if stored:
            self._metadata_cache[service] = {
                "expires_at": expires_at,
                "scopes": scopes or [],
                "has_refresh_token": refresh_token is not None,
            }

    def get_cached_metadata(self, service: str) -> Optional[Dict[str, Any]]:
        """Get cached metadata for a service (no secrets)."""
        return self._metadata_cache.get(service)

    def is_authenticated(self, service: str) -> bool:
        """Check if service has valid (non-expired) authentication."""
        token_data = None

        if self._use_keyring and keyring:
            access_key = f"{self.service_name}_{service}_access_token"
            try:
                token_json = keyring.get_password(self.service_name, access_key)
                if token_json:
                    token_data = json.loads(token_json)
            except Exception:
                pass

        # Fall back to file storage
        if not token_data:
            token_data = self._read_file_token(service)

        if not token_data:
            return False

        # Check expiry — expired with no refresh token means not authenticated
        expires_at = token_data.get("expires_at")
        if expires_at and isinstance(expires_at, (int, float)) and time.time() > expires_at:
            if not token_data.get("refresh_token"):
                return False

        return True

    def clear_token(self, service: str) -> None:
        """Logout from service (remove token)."""
        if self._use_keyring and keyring:
            access_key = f"{self.service_name}_{service}_access_token"
            try:
                keyring.delete_password(self.service_name, access_key)
            except Exception:
                pass

        self._delete_file_token(service)
        self._metadata_cache.pop(service, None)

    async def get_token(
        self,
        service: str,
        scope: Optional[str] = None,
    ) -> str:
        """Retrieve token with automatic refresh on expiry.
        
        Args:
            service: Service identifier.
            scope: Optional scope to validate.
        
        Returns:
            Valid access token.
        
        Raises:
            AuthenticationRequired: If token missing or refresh fails.
        """
        token_data = None
        
        # Try keyring first
        if self._use_keyring and keyring:
            access_key = f"{self.service_name}_{service}_access_token"
            try:
                token_json = keyring.get_password(self.service_name, access_key)
                if token_json:
                    token_data = json.loads(token_json)
            except Exception:
                pass

        # Fall back to file storage
        if not token_data:
            token_data = self._read_file_token(service)

        if not token_data:
            raise AuthenticationRequired(f"No token for {service}", service=service)

        # Check expiry
        expires_at = token_data.get("expires_at")
        if expires_at and isinstance(expires_at, (int, float)) and time.time() > expires_at:
            refresh_token = token_data.get("refresh_token")
            if not refresh_token:
                raise AuthenticationRequired(
                    f"Token expired for {service} and no refresh token",
                    service=service,
                )

            try:
                refresh_config = token_data.get("refresh_config") or {}
                refresh_url = refresh_config.get("refresh_url") if isinstance(refresh_config, dict) else None
                client_id = refresh_config.get("client_id") if isinstance(refresh_config, dict) else None
                client_secret = refresh_config.get("client_secret") if isinstance(refresh_config, dict) else None
                
                if not refresh_url or not client_id or not client_secret:
                    raise RefreshError(
                        f"Missing refresh configuration for {service}",
                        service=service,
                    )
                
                new_tokens = await self._refresh_token(
                    refresh_token=str(refresh_token),
                    refresh_url=str(refresh_url),
                    client_id=str(client_id),
                    client_secret=str(client_secret),
                )

                current_scopes = token_data.get("scopes")
                self.set_token(
                    service,
                    access_token=new_tokens["access_token"],
                    refresh_token=new_tokens.get("refresh_token"),
                    expires_in=new_tokens.get("expires_in", 3600),
                    scopes=current_scopes if isinstance(current_scopes, list) else None,
                )

                token_data = new_tokens

            except RefreshError:
                raise
            except Exception as e:
                raise RefreshError(
                    f"Failed to refresh token for {service}: {str(e)}",
                    service=service,
                )

        # Check scope if requested
        scopes = token_data.get("scopes", [])
        if scope and scope not in scopes:
            raise AuthenticationRequired(
                f"Token for {service} lacks scope {scope}",
                service=service,
            )

        return token_data["access_token"]

    async def _refresh_token(
        self,
        refresh_token: str,
        refresh_url: str,
        client_id: str,
        client_secret: str,
    ) -> Dict[str, Any]:
        """Refresh OAuth2 token."""
        try:
            async with httpx.AsyncClient() as client:
                response = await client.post(
                    refresh_url,
                    data={
                        "grant_type": "refresh_token",
                        "refresh_token": refresh_token,
                        "client_id": client_id,
                        "client_secret": client_secret,
                    },
                    timeout=30,
                )

                if response.status_code != 200:
                    raise RefreshError(
                        f"Refresh failed: {response.status_code} {response.text}"
                    )

                result = response.json()

                return {
                    "access_token": result.get("access_token"),
                    "refresh_token": result.get("refresh_token", refresh_token),
                    "expires_in": result.get("expires_in", 3600),
                }

        except httpx.HTTPError as e:
            raise RefreshError(f"Refresh request failed: {str(e)}")
        except Exception as e:
            raise RefreshError(f"Refresh error: {str(e)}")
