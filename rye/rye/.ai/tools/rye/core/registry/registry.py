"""
Registry tool - auth, push/pull, publish, key management.

Provides operations for interacting with the Rye Registry (Supabase backend).
Uses the http_client primitive for all network operations.
Uses the auth runtime for secure token storage.

Device Auth Flow (like Supabase CLI):
1. Generate session ID + ECDH keypair
2. Open browser to registry auth page
3. User authenticates via Supabase Auth
4. Server encrypts access token with shared ECDH secret
5. CLI polls for encrypted token, decrypts locally
6. Stores token in keyring

Actions:
  Auth:
    - signup: Create account with email/password
    - login: Start device auth flow (opens browser, works for OAuth signup too)
    - login_poll: Poll for auth completion
    - logout: Clear local auth session
    - whoami: Show current authenticated user

  Items:
    - search: Search for items in the registry
    - pull: Download item from registry to local (with signature verification)
    - push: Upload local item to registry (with server-side validation)
    - set_visibility: Change item visibility (public/private/unlisted)

  Keys:
    - keys_generate: Generate new signing keypair
    - keys_list: List signing keys
    - keys_trust: Add public key to trusted list
    - keys_revoke: Revoke a signing key
"""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "rye/core/registry"
__tool_description__ = "Registry tool for auth, push/pull, publish, and key management"

import asyncio
import base64
import hashlib
import json
import os
import secrets
import time
import webbrowser
from dataclasses import dataclass
from datetime import datetime, timezone
from pathlib import Path
from typing import Any, Dict, List, Literal, Optional, Tuple
from urllib.parse import urlencode

# Telemetry integration
try:
    from .telemetry.lib import TelemetryStore

    TELEMETRY_AVAILABLE = True
except ImportError:
    TELEMETRY_AVAILABLE = False

# Try to import cryptography for ECDH
try:
    from cryptography.hazmat.primitives import hashes, serialization
    from cryptography.hazmat.primitives.asymmetric import ec
    from cryptography.hazmat.primitives.ciphers.aead import AESGCM
    from cryptography.hazmat.primitives.kdf.hkdf import HKDF

    CRYPTO_AVAILABLE = True
except ImportError:
    CRYPTO_AVAILABLE = False

TOOL_METADATA = {
    "name": "registry",
    "description": "Registry operations: auth, push/pull, publish, key management",
    "version": "1.0.0",
    "protected": True,
}

ACTIONS = [
    # Auth
    "signup",
    "login",
    "login_poll",
    "logout",
    "whoami",
    # Items
    "search",
    "pull",
    "push",
    "set_visibility",
    # Keys
    "keys_generate",
    "keys_list",
    "keys_trust",
    "keys_revoke",
]

# Registry configuration from environment
REGISTRY_URL = os.environ.get(
    "RYE_REGISTRY_URL", "https://jvdgicalhvhaqtcalseq.supabase.co"
)
REGISTRY_ANON_KEY = os.environ.get("RYE_REGISTRY_ANON_KEY", "")

# Auth configuration
# Service key for keyring storage (kernel uses service_name="lilux" by default)
REGISTRY_SERVICE = "rye_registry"
# Env var override for CI/headless - checked before keyring
REGISTRY_TOKEN_ENV = "RYE_REGISTRY_TOKEN"


def _get_rye_state_dir() -> Path:
    """Get RYE state directory from kernel path service.

    Cross-platform:
        - Linux: $XDG_STATE_HOME/rye/ → ~/.local/state/rye/
        - macOS: ~/Library/Application Support/rye/
        - Windows: %LOCALAPPDATA%/rye/
    """
    from lilux.utils.path_service import get_rye_state_dir

    return get_rye_state_dir()


def _get_keys_dir() -> Path:
    """Get signing keys directory under RYE state."""
    return _get_rye_state_dir() / "signing-keys"


def _get_trusted_keys_dir() -> Path:
    """Get trusted keys directory under signing keys."""
    return _get_keys_dir() / "trusted"


def _get_session_dir() -> Path:
    """Get sessions directory under RYE state."""
    return _get_rye_state_dir() / "sessions"


def _get_token_from_env() -> Optional[str]:
    """Check for token in env var (CI/headless mode)."""
    return os.environ.get(REGISTRY_TOKEN_ENV)


@dataclass
class RegistryConfig:
    """Registry connection configuration."""

    base_url: str
    anon_key: str

    @classmethod
    def from_env(cls) -> "RegistryConfig":
        return cls(
            base_url=REGISTRY_URL,
            anon_key=REGISTRY_ANON_KEY,
        )


# =============================================================================
# HTTP CLIENT WRAPPER
# =============================================================================


class RegistryHttpClient:
    """Wrapper around http_client primitive for registry API calls."""

    def __init__(self, config: RegistryConfig):
        self.config = config
        self._http = None

    async def _get_http(self):
        """Lazy load http_client primitive."""
        if self._http is None:
            from ..primitives.http_client import HttpClientPrimitive

            self._http = HttpClientPrimitive()
        return self._http

    async def get(
        self,
        path: str,
        headers: Optional[Dict] = None,
        auth_token: Optional[str] = None,
    ) -> Dict:
        """Make GET request to registry API."""
        http = await self._get_http()

        req_headers = {
            "apikey": self.config.anon_key,
            "Content-Type": "application/json",
        }
        if auth_token:
            req_headers["Authorization"] = f"Bearer {auth_token}"
        if headers:
            req_headers.update(headers)

        config = {
            "method": "GET",
            "url": f"{self.config.base_url}{path}",
            "headers": req_headers,
            "timeout": 30,
        }

        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }

    async def post(
        self,
        path: str,
        body: Dict,
        headers: Optional[Dict] = None,
        auth_token: Optional[str] = None,
    ) -> Dict:
        """Make POST request to registry API."""
        http = await self._get_http()

        req_headers = {
            "apikey": self.config.anon_key,
            "Content-Type": "application/json",
        }
        if auth_token:
            req_headers["Authorization"] = f"Bearer {auth_token}"
        if headers:
            req_headers.update(headers)

        config = {
            "method": "POST",
            "url": f"{self.config.base_url}{path}",
            "headers": req_headers,
            "body": body,
            "timeout": 30,
        }

        result = await http.execute(config, {})
        return {
            "success": result.success,
            "status_code": result.status_code,
            "body": result.body,
            "error": result.error,
        }

    async def close(self):
        """Close HTTP client."""
        if self._http:
            await self._http.close()


# =============================================================================
# ECDH KEY EXCHANGE FOR DEVICE AUTH
# =============================================================================


def generate_ecdh_keypair() -> Tuple[bytes, bytes]:
    """Generate ECDH P-256 keypair for device auth.

    Returns:
        Tuple of (private_key_bytes, public_key_bytes)
    """
    if not CRYPTO_AVAILABLE:
        raise RuntimeError("cryptography library required for device auth")

    private_key = ec.generate_private_key(ec.SECP256R1())
    public_key = private_key.public_key()

    # Serialize keys
    private_bytes = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    public_bytes = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )

    return private_bytes, public_bytes


def derive_shared_secret(private_key_pem: bytes, peer_public_key_pem: bytes) -> bytes:
    """Derive shared secret using ECDH.

    Args:
        private_key_pem: Our private key in PEM format
        peer_public_key_pem: Server's public key in PEM format

    Returns:
        32-byte shared secret for AES-GCM
    """
    if not CRYPTO_AVAILABLE:
        raise RuntimeError("cryptography library required")

    private_key = serialization.load_pem_private_key(private_key_pem, password=None)
    peer_public_key = serialization.load_pem_public_key(peer_public_key_pem)

    # Perform ECDH
    shared_key = private_key.exchange(ec.ECDH(), peer_public_key)

    # Derive AES key using HKDF
    derived_key = HKDF(
        algorithm=hashes.SHA256(),
        length=32,
        salt=None,
        info=b"rye-registry-auth",
    ).derive(shared_key)

    return derived_key


def decrypt_token(encrypted_b64: str, nonce_b64: str, shared_secret: bytes) -> str:
    """Decrypt access token using AES-GCM.

    Args:
        encrypted_b64: Base64-encoded encrypted token
        nonce_b64: Base64-encoded nonce
        shared_secret: 32-byte shared secret from ECDH

    Returns:
        Decrypted access token string
    """
    if not CRYPTO_AVAILABLE:
        raise RuntimeError("cryptography library required")

    encrypted = base64.b64decode(encrypted_b64)
    nonce = base64.b64decode(nonce_b64)

    aesgcm = AESGCM(shared_secret)
    decrypted = aesgcm.decrypt(nonce, encrypted, None)

    return decrypted.decode("utf-8")


async def execute(
    action: str, project_path: str, params: Optional[Dict[str, Any]] = None
) -> Dict[str, Any]:
    """
    Execute a registry action.

    Args:
        action: One of the ACTIONS
        project_path: Path to project root
        params: Action-specific parameters

    Returns:
        Action result dict
    """
    params = params or {}

    if action not in ACTIONS:
        return {
            "error": f"Unknown action: {action}",
            "valid_actions": ACTIONS,
        }

    # Track execution with telemetry
    start_time = time.time()
    result: Dict[str, Any] = {}
    error_msg: Optional[str] = None
    http_calls = 0

    try:
        # Auth actions
        if action == "signup":
            result = await _signup(params)
        elif action == "login":
            result = await _login(params)
        elif action == "login_poll":
            result = await _login_poll(params)
        elif action == "logout":
            result = await _logout()
        elif action == "whoami":
            result = await _whoami()

        # Item actions
        elif action == "search":
            result = await _search(
                query=params.get("query"),
                item_type=params.get("item_type"),
                category=params.get("category"),
                author=params.get("author"),
                limit=params.get("limit", 20),
            )
            http_calls = 1
        elif action == "pull":
            result = await _pull(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
                version=params.get("version"),
                dest_path=params.get("dest_path") or project_path,
                verify=params.get("verify", True),
            )
            http_calls = 1  # pull makes HTTP requests
        elif action == "push":
            result = await _push(
                item_type=params.get("item_type"),
                item_path=params.get("item_path"),
                name=params.get("name"),
                version=params.get("version"),
                visibility=params.get("visibility", "private"),
            )
            http_calls = 2  # push typically makes 2 HTTP requests (check + create)
        elif action == "set_visibility":
            result = await _set_visibility(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
                visibility=params.get("visibility"),
            )
            http_calls = 1

        # Key actions
        elif action == "keys_generate":
            result = await _keys_generate(
                label=params.get("label"),
                make_primary=params.get("make_primary", False),
            )
        elif action == "keys_list":
            result = await _keys_list()
        elif action == "keys_trust":
            result = await _keys_trust(
                key_id=params.get("key_id"),
                public_key=params.get("public_key"),
            )
        elif action == "keys_revoke":
            result = await _keys_revoke(
                key_id=params.get("key_id"),
            )
        else:
            result = {"error": f"Action '{action}' not implemented"}

        # Check if result indicates an error
        if "error" in result:
            error_msg = result["error"]

    except Exception as e:
        error_msg = str(e)
        result = {"error": error_msg}

    # Record telemetry
    duration_ms = (time.time() - start_time) * 1000

    if TELEMETRY_AVAILABLE:
        try:
            store = TelemetryStore()
            store.record_execution(
                item_id=f"registry.{action}",
                item_type="tool",
                outcome="success" if error_msg is None else "failure",
                duration_ms=duration_ms,
                http_calls=http_calls,
                subprocess_calls=0,
                error=error_msg,
                path=project_path,
            )
        except Exception:
            pass  # Don't fail the action if telemetry fails

    return result


# =============================================================================
# AUTH ACTIONS
# =============================================================================


def _save_session(
    session_id: str, private_key: bytes, public_key: bytes, expires_at: str
) -> Path:
    """Save session data for later polling."""
    session_dir = _get_session_dir()
    session_dir.mkdir(parents=True, exist_ok=True)
    session_path = session_dir / f"{session_id}.json"

    session_data = {
        "session_id": session_id,
        "private_key": base64.b64encode(private_key).decode(),
        "public_key": base64.b64encode(public_key).decode(),
        "expires_at": expires_at,
        "created_at": datetime.now(timezone.utc).isoformat(),
    }

    session_path.write_text(json.dumps(session_data))
    os.chmod(session_path, 0o600)  # Private - contains private key

    return session_path


def _load_session(session_id: str) -> Optional[Dict]:
    """Load session data for polling."""
    session_path = _get_session_dir() / f"{session_id}.json"
    if not session_path.exists():
        return None

    try:
        return json.loads(session_path.read_text())
    except (json.JSONDecodeError, OSError):
        return None


def _delete_session(session_id: str) -> None:
    """Delete session after successful auth or expiry."""
    session_path = _get_session_dir() / f"{session_id}.json"
    if session_path.exists():
        session_path.unlink()


async def _signup(params: Dict[str, Any]) -> Dict[str, Any]:
    """
    Sign up for a new registry account via email/password.

    For OAuth signup (GitHub, etc.), use 'login' instead - it handles
    both login and signup automatically.

    Args:
        email: User's email address
        password: Password (min 8 chars)
        username: Optional preferred username
    """
    email = params.get("email")
    password = params.get("password")
    username = params.get("username")

    if not email or not password:
        return {
            "error": "Required: email and password",
            "usage": "signup(email='you@example.com', password='securepass')",
            "alternative": "Use 'login' action for GitHub OAuth signup",
        }

    if len(password) < 8:
        return {"error": "Password must be at least 8 characters"}

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        # Call Supabase Auth signup endpoint
        result = await http.post(
            "/auth/v1/signup",
            body={
                "email": email,
                "password": password,
                "data": (
                    {
                        "preferred_username": username,
                    }
                    if username
                    else {}
                ),
            },
        )

        await http.close()

        if not result["success"]:
            error_msg = (
                result.get("body", {}).get("error_description")
                or result.get("body", {}).get("msg")
                or result["error"]
            )
            return {
                "error": f"Signup failed: {error_msg}",
                "status_code": result["status_code"],
            }

        body = result["body"]

        # Check if email confirmation is required
        if body.get("confirmation_sent_at"):
            return {
                "status": "confirmation_required",
                "email": email,
                "message": f"Check your email ({email}) to confirm your account, then run 'registry login'",
            }

        # If no confirmation required, we have a session
        if body.get("access_token"):
            try:
                from ..runtimes.auth import AuthStore

                auth_store = AuthStore()  # Uses kernel default service_name="lilux"
                auth_store.set_token(
                    service=REGISTRY_SERVICE,
                    access_token=body["access_token"],
                    refresh_token=body.get("refresh_token"),
                    expires_in=body.get("expires_in", 3600),
                    scopes=["registry:read", "registry:write"],
                )

                return {
                    "status": "authenticated",
                    "message": "Account created and logged in",
                    "user": body.get("user", {}),
                }
            except ImportError:
                return {
                    "status": "created",
                    "message": "Account created. Run 'registry login' to authenticate.",
                }

        return {
            "status": "created",
            "message": "Account created. Check your email for confirmation, then run 'registry login'.",
        }

    except Exception as e:
        await http.close()
        return {"error": f"Signup failed: {e}"}


async def _login(params: Dict[str, Any]) -> Dict[str, Any]:
    """
    Start device authorization flow.

    1. Generate session ID + ECDH keypair
    2. Open browser to registry auth page with public key
    3. Return session_id for polling

    User then runs login_poll to complete the flow.
    """
    if not CRYPTO_AVAILABLE:
        return {
            "error": "cryptography library required for device auth",
            "solution": "pip install cryptography",
        }

    try:
        from ..runtimes.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available - auth runtime not installed"}

    # Check env var override first (CI/headless mode)
    env_token = _get_token_from_env()
    if env_token:
        return {
            "status": "env_token",
            "message": f"Using token from {REGISTRY_TOKEN_ENV} environment variable",
        }

    # Check if already authenticated via keyring
    auth_store = AuthStore()  # Uses kernel default service_name="lilux"
    if auth_store.is_authenticated(REGISTRY_SERVICE):
        return {
            "status": "already_authenticated",
            "message": "Already logged in. Use 'registry logout' first if you want to re-authenticate.",
        }

    # Generate session ID and ECDH keypair
    session_id = secrets.token_urlsafe(32)
    private_key, public_key = generate_ecdh_keypair()

    # Get hostname and username for token name
    import getpass
    import platform

    try:
        username = getpass.getuser()
        hostname = platform.node()
    except Exception:
        username = "user"
        hostname = "device"

    token_name = f"{username}@{hostname}-{datetime.now().strftime('%Y%m%d%H%M%S')}"

    # Build auth URL
    config = RegistryConfig.from_env()

    # Encode public key for URL
    public_key_b64 = base64.urlsafe_b64encode(public_key).decode().rstrip("=")

    auth_params = urlencode(
        {
            "session_id": session_id,
            "public_key": public_key_b64,
            "token_name": token_name,
        }
    )

    # Use Supabase's auth UI - redirect to login then back to device callback
    auth_url = f"{config.base_url}/auth/v1/authorize?provider=github&redirect_to={config.base_url}/functions/v1/device-auth-callback?{auth_params}"

    # For simpler approach, we'll use a custom edge function endpoint
    # that handles the device auth flow
    auth_url = f"{config.base_url}/functions/v1/device-auth?{auth_params}"

    # Save session for later polling
    expires_at = (datetime.now(timezone.utc).replace(microsecond=0)).isoformat()
    _save_session(session_id, private_key, public_key, expires_at)

    # Open browser
    open_browser = params.get("open_browser", True)
    if open_browser:
        try:
            webbrowser.open(auth_url)
            browser_opened = True
        except Exception:
            browser_opened = False
    else:
        browser_opened = False

    return {
        "status": "awaiting_auth",
        "session_id": session_id,
        "auth_url": auth_url,
        "browser_opened": browser_opened,
        "expires_in": 300,  # 5 minutes
        "instructions": [
            "1. Open the URL in your browser"
            + (" (already opened)" if browser_opened else ""),
            "2. Sign in with GitHub or email",
            "3. The auth will complete automatically, or run:",
            f"   registry login_poll --session_id={session_id}",
        ],
        "next_action": {
            "action": "login_poll",
            "params": {"session_id": session_id},
        },
    }


async def _login_poll(params: Dict[str, Any]) -> Dict[str, Any]:
    """
    Poll for auth completion and exchange encrypted token.

    Args:
        session_id: Session ID from login
        max_attempts: Max poll attempts (default 60)
        interval: Seconds between polls (default 5)
    """
    session_id = params.get("session_id")
    if not session_id:
        return {"error": "session_id required"}

    # Load session
    session = _load_session(session_id)
    if not session:
        return {
            "error": f"Session not found: {session_id}",
            "solution": "Run 'registry login' first",
        }

    try:
        from ..runtimes.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)
    auth_store = AuthStore()  # Uses kernel default service_name="lilux"

    max_attempts = params.get("max_attempts", 60)
    interval = params.get("interval", 5)

    private_key = base64.b64decode(session["private_key"])

    for attempt in range(max_attempts):
        # Poll the device-auth-poll endpoint
        result = await http.get(
            f"/functions/v1/device-auth-poll?session_id={session_id}"
        )

        if not result["success"]:
            if result["status_code"] == 404:
                # Session not found or expired
                _delete_session(session_id)
                return {
                    "error": "Session expired or not found",
                    "solution": "Run 'registry login' again",
                }
            elif result["status_code"] == 202:
                # Still pending - wait and retry
                if attempt < max_attempts - 1:
                    await asyncio.sleep(interval)
                    continue

        if result["success"] and result["body"]:
            body = result["body"]

            if body.get("status") == "pending":
                if attempt < max_attempts - 1:
                    await asyncio.sleep(interval)
                    continue

            if body.get("status") == "completed":
                # Decrypt token
                try:
                    server_public_key = base64.b64decode(body["server_public_key"])
                    shared_secret = derive_shared_secret(private_key, server_public_key)

                    access_token = decrypt_token(
                        body["encrypted_token"],
                        body["nonce"],
                        shared_secret,
                    )

                    # Store in keyring
                    auth_store.set_token(
                        service=REGISTRY_SERVICE,
                        access_token=access_token,
                        refresh_token=body.get("refresh_token"),
                        expires_in=body.get("expires_in", 3600),
                        scopes=["registry:read", "registry:write"],
                    )

                    # Clean up session
                    _delete_session(session_id)

                    await http.close()

                    return {
                        "status": "authenticated",
                        "message": "Successfully logged in to Rye Registry",
                        "user": body.get("user", {}),
                    }

                except Exception as e:
                    await http.close()
                    return {
                        "error": f"Failed to decrypt token: {e}",
                        "solution": "Try 'registry login' again",
                    }

    await http.close()

    return {
        "status": "timeout",
        "error": "Authentication timed out",
        "solution": "Run 'registry login' again",
    }


async def _logout() -> Dict[str, Any]:
    """Clear local auth session."""
    # Check if using env var token
    if _get_token_from_env():
        return {
            "status": "env_token",
            "message": f"Using {REGISTRY_TOKEN_ENV} env var. Unset it to logout.",
        }

    try:
        from ..runtimes.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    auth_store = AuthStore()  # Uses kernel default service_name="lilux"
    auth_store.clear_token(REGISTRY_SERVICE)

    return {
        "status": "logged_out",
        "message": "Successfully logged out from Rye Registry",
    }


async def _whoami() -> Dict[str, Any]:
    """Show current authenticated user."""
    # Check env var override first
    env_token = _get_token_from_env()
    if env_token:
        return {
            "authenticated": True,
            "source": "env",
            "env_var": REGISTRY_TOKEN_ENV,
            "message": f"Using token from {REGISTRY_TOKEN_ENV} environment variable",
        }

    try:
        from ..runtimes.auth import AuthenticationRequired, AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    auth_store = AuthStore()  # Uses kernel default service_name="lilux"

    if not auth_store.is_authenticated(REGISTRY_SERVICE):
        return {
            "authenticated": False,
            "message": "Not logged in. Run 'registry login' to authenticate.",
        }

    # Get cached metadata (never includes actual token)
    metadata = auth_store.get_cached_metadata(REGISTRY_SERVICE)

    return {
        "authenticated": True,
        "source": "keyring",
        "scopes": metadata.get("scopes", []) if metadata else [],
        "expires_at": metadata.get("expires_at") if metadata else None,
        "has_refresh_token": (
            metadata.get("has_refresh_token", False) if metadata else False
        ),
    }


# =============================================================================
# ITEM ACTIONS
# =============================================================================


async def _search(
    query: Optional[str],
    item_type: Optional[str] = None,
    category: Optional[str] = None,
    author: Optional[str] = None,
    limit: int = 20,
) -> Dict[str, Any]:
    """
    Search for items in the registry via Registry API.

    Args:
        query: Search query (searches name and description)
        item_type: Filter by type ("directive", "tool", or "knowledge")
        category: Filter by category
        author: Filter by author username
        limit: Maximum results to return (default 20)
    """
    if not query:
        return {
            "error": "Required: query",
            "usage": "search(query='bootstrap', item_type='directive')",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        # Build query params for Registry API
        url = f"/v1/search?query={query}&limit={limit}"
        if item_type:
            url += f"&item_type={item_type}"
        if category:
            url += f"&category={category}"
        if author:
            url += f"&author={author}"

        result = await http.get(url)
        await http.close()

        if not result["success"]:
            return {
                "error": f"Search failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        body = result.get("body", {})
        return {
            "status": "success",
            "query": query,
            "results": body.get("results", []),
            "total": body.get("total", 0),
            "filters": {
                "item_type": item_type,
                "category": category,
                "author": author,
            },
        }

    except Exception as e:
        await http.close()
        return {"error": f"Search failed: {e}"}


async def _pull(
    item_type: Optional[str],
    item_id: Optional[str],
    version: Optional[str],
    dest_path: str,
    verify: bool = True,
) -> Dict[str, Any]:
    """
    Download item from registry via Registry API with signature verification.

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_id: Item identifier (namespace/name format)
        version: Specific version (or "latest")
        dest_path: Destination directory
        verify: Verify registry signature (default True)
    """
    if not item_type or not item_id:
        return {
            "error": "Required: item_type and item_id",
            "usage": "pull(item_type='directive', item_id='core/bootstrap')",
        }

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        # Call Registry API pull endpoint
        url = f"/v1/pull/{item_type}/{item_id}"
        if version:
            url += f"?version={version}"

        result = await http.get(url)
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            if isinstance(error_body, dict) and "error" in error_body:
                return {
                    "error": error_body["error"],
                    "suggestion": f"Search for available items with: search(query='{item_id}')",
                }
            return {
                "error": f"Failed to fetch {item_type}: {result.get('error', 'Unknown')}",
                "status_code": result.get("status_code"),
            }

        body = result.get("body", {})
        content = body.get("content", "")
        author_username = body.get("author", "")
        item_version = body.get("version", "")
        signature_data = body.get("signature", {})

        # Verify registry signature locally if enabled
        signature_info = None
        if verify and content:
            try:
                from rye.utils.metadata_manager import MetadataManager

                strategy = MetadataManager.get_strategy(item_type)
                sig_info = strategy.extract_signature(content)

                if not sig_info:
                    return {
                        "error": "No signature found on registry content",
                        "hint": "Content may be corrupted or from an older registry version",
                    }

                # Verify it's a registry signature
                registry_username = sig_info.get("registry_username")
                if registry_username:
                    # Verify username matches author from API
                    if author_username and registry_username != author_username:
                        return {
                            "error": "Signature username mismatch",
                            "signature_says": registry_username,
                            "registry_says": author_username,
                            "hint": "Content may have been tampered with",
                        }

                    # Verify content hash
                    content_without_sig = strategy.remove_signature(content)
                    computed_hash = hashlib.sha256(content_without_sig.encode()).hexdigest()

                    if computed_hash != sig_info["hash"]:
                        return {
                            "error": "Content integrity check failed",
                            "expected_hash": sig_info["hash"],
                            "computed_hash": computed_hash,
                            "hint": "Content was modified after signing",
                        }

                signature_info = {
                    "verified": True,
                    "registry_username": registry_username,
                    "timestamp": sig_info.get("timestamp"),
                    "hash": sig_info.get("hash"),
                }

            except ImportError:
                # MetadataManager not available, skip verification
                signature_info = {"verified": False, "reason": "MetadataManager not available"}

        # Determine destination path
        dest = Path(dest_path)
        if dest.is_dir():
            # Build path like .ai/directives/category/name.md
            # Use item_id last segment as filename
            filename = item_id.split("/")[-1]
            ext = ".md" if item_type in ["directive", "knowledge"] else ".py"
            dest = dest / ".ai" / f"{item_type}s" / f"{filename}{ext}"

        # Create directory and write content
        dest.parent.mkdir(parents=True, exist_ok=True)
        dest.write_text(content)

        return {
            "status": "pulled",
            "item_type": item_type,
            "item_id": item_id,
            "version": item_version,
            "path": str(dest),
            "content_hash": signature_data.get("hash", ""),
            "author": author_username,
            "signature": signature_info,
        }

    except Exception as e:
        await http.close()
        return {"error": f"Pull failed: {e}"}


async def _push(
    item_type: Optional[str],
    item_path: Optional[str],
    name: Optional[str],
    version: Optional[str],
    visibility: str = "private",
) -> Dict[str, Any]:
    """
    Upload local item to registry with server-side validation.

    Flow:
    1. Validate content locally using rye validators
    2. Sign content locally (standard signature)
    3. Push to Registry API (server re-validates and adds |registry@username)
    4. Update local file with registry-signed content

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_path: Path to local item file
        name: Registry name (namespace/name format)
        version: Version string (semver)
        visibility: "public", "private", or "unlisted"
    """
    if not item_type or not item_path or not name or not version:
        return {
            "error": "Required: item_type, item_path, name, version",
            "usage": "push(item_type='directive', item_path='.ai/directives/my.md', name='me/my', version='1.0.0')",
        }

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    path = Path(item_path)
    if not path.exists():
        return {"error": f"File not found: {item_path}"}

    # Check auth - env var first, then keyring
    env_token = _get_token_from_env()
    if env_token:
        token = env_token
    else:
        try:
            from ..runtimes.auth import AuthenticationRequired, AuthStore

            auth_store = AuthStore()  # Uses kernel default service_name="lilux"
            token = await auth_store.get_token(REGISTRY_SERVICE, scope="registry:write")
        except AuthenticationRequired:
            return {
                "error": "Authentication required",
                "solution": "Run 'registry login' first",
            }
        except ImportError:
            return {"error": "AuthStore not available"}

    # Read content
    content = path.read_text()

    # Step 1: Validate locally using rye validators (same as sign tool)
    try:
        from rye.utils.parser_router import ParserRouter
        from rye.utils.validators import apply_field_mapping, validate_parsed_data
        from rye.utils.metadata_manager import MetadataManager

        parser_router = ParserRouter()
        parser_types = {
            "directive": "markdown_xml",
            "tool": "python_ast",
            "knowledge": "markdown_yaml",
        }
        parser_type = parser_types.get(item_type)

        # Strip existing signature for validation
        strategy = MetadataManager.get_strategy(item_type)
        content_clean = strategy.remove_signature(content)

        # Parse content
        parsed = parser_router.parse(parser_type, content_clean)
        if "error" in parsed:
            return {
                "error": "Failed to parse content",
                "details": parsed.get("error"),
                "path": str(path),
            }

        # Add name for tools (matches client sign tool behavior)
        if item_type == "tool":
            parsed["name"] = path.stem

        # Apply field mapping
        parsed = apply_field_mapping(item_type, parsed)

        # Validate
        validation = validate_parsed_data(
            item_type=item_type,
            parsed_data=parsed,
            file_path=path,
            location="project",
        )

        if not validation["valid"]:
            return {
                "error": "Validation failed",
                "issues": validation["issues"],
                "path": str(path),
                "hint": "Fix validation issues before pushing",
            }

        # Step 2: Sign locally (standard signature, no registry suffix)
        signed_content = MetadataManager.sign_content(
            item_type, content_clean, file_path=path
        )

    except ImportError as e:
        return {"error": f"Missing rye validation modules: {e}"}

    # Step 3: Push to Registry API (server re-validates and adds |registry@username)
    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        # Push to Registry API endpoint
        # The API validates, signs with registry provenance, and stores
        result = await http.post(
            "/v1/push",
            body={
                "item_type": item_type,
                "item_id": name,
                "content": signed_content,
                "version": version,
                "metadata": {
                    "visibility": visibility,
                    "category": parsed.get("category", ""),
                    "description": parsed.get("description", ""),
                },
            },
            auth_token=token,
        )

        await http.close()

        if not result["success"]:
            # Check if this is a validation error from the server
            error_body = result.get("body", {})
            if isinstance(error_body, dict) and "issues" in error_body:
                return {
                    "error": "Server-side validation failed",
                    "issues": error_body["issues"],
                    "hint": "Server rejected content - check validation rules",
                }
            return {
                "error": f"Push failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        # Step 4: Update local file with registry-signed content
        response_body = result.get("body", {})
        if isinstance(response_body, dict) and "signed_content" in response_body:
            registry_signed = response_body["signed_content"]
            path.write_text(registry_signed)

        return {
            "status": "pushed",
            "item_type": item_type,
            "name": name,
            "version": version,
            "visibility": visibility,
            "content_hash": response_body.get("signature", {}).get("hash", ""),
            "registry_username": response_body.get("signature", {}).get("registry_username"),
            "size_bytes": len(signed_content.encode()),
            "local_updated": "signed_content" in response_body,
        }

    except Exception as e:
        await http.close()
        return {"error": f"Push failed: {e}"}


async def _set_visibility(
    item_type: Optional[str],
    item_id: Optional[str],
    visibility: Optional[str],
) -> Dict[str, Any]:
    """Change item visibility."""
    if not item_type or not item_id or not visibility:
        return {
            "error": "Required: item_type, item_id, visibility",
            "usage": "set_visibility(item_type='directive', item_id='me/my', visibility='public')",
        }

    valid_visibilities = ["public", "private", "unlisted"]
    if visibility not in valid_visibilities:
        return {
            "error": f"Invalid visibility: {visibility}",
            "valid": valid_visibilities,
        }

    # Check auth - env var first, then keyring
    env_token = _get_token_from_env()
    if env_token:
        token = env_token
    else:
        try:
            from ..runtimes.auth import AuthenticationRequired, AuthStore

            auth_store = AuthStore()  # Uses kernel default service_name="lilux"
            token = await auth_store.get_token(REGISTRY_SERVICE, scope="registry:write")
        except AuthenticationRequired:
            return {
                "error": "Authentication required",
                "solution": "Run 'registry login' first",
            }
        except ImportError:
            return {"error": "AuthStore not available"}

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        table = f"{item_type}s"

        # Update visibility via PATCH
        result = await http.post(
            f"/rest/v1/{table}?name=eq.{item_id}",
            body={"visibility": visibility},
            auth_token=token,
            headers={"X-HTTP-Method-Override": "PATCH"},
        )

        await http.close()

        if not result["success"]:
            return {
                "error": f"Failed to update visibility: {result['error']}",
                "status_code": result["status_code"],
            }

        return {
            "status": "updated",
            "item_type": item_type,
            "item_id": item_id,
            "visibility": visibility,
        }

    except Exception as e:
        await http.close()
        return {"error": f"Set visibility failed: {e}"}


# =============================================================================
# KEY ACTIONS
# =============================================================================


async def _keys_generate(
    label: Optional[str],
    make_primary: bool = False,
) -> Dict[str, Any]:
    """
    Generate new Ed25519 signing keypair.

    Private key stored locally in $USER_SPACE/registry/keys/ (default ~/.ai/registry/keys/)
    Public key uploaded to registry (requires auth)
    """
    try:
        from cryptography.hazmat.primitives import serialization
        from cryptography.hazmat.primitives.asymmetric.ed25519 import Ed25519PrivateKey
    except ImportError:
        return {
            "error": "cryptography library required for key generation. Run: pip install cryptography"
        }

    # Generate keypair
    private_key = Ed25519PrivateKey.generate()
    public_key = private_key.public_key()

    # Serialize keys
    private_pem = private_key.private_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PrivateFormat.PKCS8,
        encryption_algorithm=serialization.NoEncryption(),
    )
    public_pem = public_key.public_bytes(
        encoding=serialization.Encoding.PEM,
        format=serialization.PublicFormat.SubjectPublicKeyInfo,
    )

    # Generate key ID from public key hash
    key_id = hashlib.sha256(public_pem).hexdigest()[:16]

    # Ensure keys directory exists with proper permissions
    keys_dir = _get_keys_dir()
    keys_dir.mkdir(parents=True, exist_ok=True)
    os.chmod(keys_dir, 0o700)

    # Save private key
    private_path = keys_dir / f"{key_id}.private.pem"
    private_path.write_bytes(private_pem)
    os.chmod(private_path, 0o600)

    # Save public key
    public_path = keys_dir / f"{key_id}.public.pem"
    public_path.write_bytes(public_pem)
    os.chmod(public_path, 0o644)

    # If make_primary, update symlink
    if make_primary:
        primary_link = keys_dir / "primary.pem"
        if primary_link.exists() or primary_link.is_symlink():
            primary_link.unlink()
        primary_link.symlink_to(f"{key_id}.private.pem")

    return {
        "status": "generated",
        "key_id": key_id,
        "label": label,
        "is_primary": make_primary,
        "private_key_path": str(private_path),
        "public_key_path": str(public_path),
        "public_key_pem": public_pem.decode(),
        "message": f"Generated keypair '{key_id}'. Upload public key to registry with: registry push_key",
        "next_step": "Run 'registry login' then 'registry push_key' to register with registry",
    }


async def _keys_list() -> Dict[str, Any]:
    """List local signing keys."""
    local_keys = []
    keys_dir = _get_keys_dir()

    if keys_dir.exists():
        # Find all private keys
        for key_file in keys_dir.glob("*.private.pem"):
            key_id = key_file.stem.replace(".private", "")
            public_exists = (keys_dir / f"{key_id}.public.pem").exists()

            # Check if primary
            primary_link = keys_dir / "primary.pem"
            is_primary = (
                primary_link.exists()
                and primary_link.is_symlink()
                and primary_link.resolve() == key_file
            )

            local_keys.append(
                {
                    "key_id": key_id,
                    "is_primary": is_primary,
                    "has_public": public_exists,
                    "path": str(key_file),
                }
            )

    # List trusted keys
    trusted_keys = []
    trusted_keys_dir = _get_trusted_keys_dir()
    if trusted_keys_dir.exists():
        for key_file in trusted_keys_dir.glob("*.pub"):
            key_id = key_file.stem
            trusted_keys.append(
                {
                    "key_id": key_id,
                    "path": str(key_file),
                }
            )

    return {
        "local_keys": local_keys,
        "trusted_keys": trusted_keys,
        "keys_dir": str(keys_dir),
        "trusted_dir": str(trusted_keys_dir),
    }


async def _keys_trust(
    key_id: Optional[str],
    public_key: Optional[str],
) -> Dict[str, Any]:
    """Add a public key to trusted list."""
    if not key_id:
        return {"error": "Required: key_id"}

    # Ensure trusted keys directory exists
    trusted_keys_dir = _get_trusted_keys_dir()
    trusted_keys_dir.mkdir(parents=True, exist_ok=True)

    # If public_key provided, save it
    if public_key:
        key_path = trusted_keys_dir / f"{key_id}.pub"
        key_path.write_text(public_key)

        return {
            "status": "trusted",
            "key_id": key_id,
            "path": str(key_path),
            "message": f"Added key '{key_id}' to trusted keys",
        }

    # Otherwise, fetch from registry
    return {
        "status": "pending",
        "key_id": key_id,
        "message": f"Would fetch public key '{key_id}' from registry and add to trusted",
        "note": "Full implementation requires http_client primitive integration",
    }


async def _keys_revoke(key_id: Optional[str]) -> Dict[str, Any]:
    """Revoke a signing key."""
    if not key_id:
        return {"error": "Required: key_id"}

    # Remove from local trusted keys if present
    trusted_path = _get_trusted_keys_dir() / f"{key_id}.pub"
    if trusted_path.exists():
        trusted_path.unlink()

    return {
        "status": "revoke_pending",
        "key_id": key_id,
        "removed_from_trusted": trusted_path.exists(),
        "message": f"Would revoke key '{key_id}' in registry",
        "note": "Full implementation requires auth + http_client primitive integration",
    }
