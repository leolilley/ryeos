# rye:signed:2026-02-28T00:25:41Z:edcabfd8d654d45b7270014d20eb47412f11063fa79b867b87a0b8eee401f08a:uom3e8K0lFbooHb8H_m_uHpnf_jqDpK3grmDVzLaKu4zI8ZUXxqaVP6asRoMRp9PqDBbmhzBqoePiOGEs2KUDQ==:4b987fd4e40303ac
"""
Registry tool - auth and item management for Rye Registry.

Identity model:
  item_id = "{namespace}/{category}/{name}" (canonical)
  - namespace: owner (no slashes), e.g., "leolilley"
  - category: folder path (may contain slashes), e.g., "core" or "rye/core/registry"
  - name: basename (no slashes), e.g., "bootstrap"
  
  Parsing: first segment = namespace, last segment = name, middle = category
  Example: "leolilley/rye/core/registry/registry" 
           -> namespace="leolilley", category="rye/core/registry", name="registry"

Provides operations for interacting with the Rye Registry:
- Auth via OAuth PKCE flow (GitHub, etc.)
- Push/pull items to/from registry
- Publish/unpublish to control visibility

Uses Railway API for item operations, Supabase for auth.

Actions:
  Auth:
    - signup: Create account with email/password
    - login: Device auth flow (opens browser, polls for completion, creates API key)
    - logout: Clear local auth session
    - whoami: Show current authenticated user

  Items:
    - search: Search for items in the registry
    - pull: Download item from registry to local (item_id=namespace/category/name)
    - push: Upload local item to registry (item_id=namespace/category/name)
    - delete: Remove item from registry
    - publish: Make item public (visibility='public')
    - unpublish: Make item private (visibility='private')
"""

__version__ = "1.1.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/script"
__category__ = "rye/core/registry"
__tool_description__ = "Registry tool for auth and item management"

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

# Import filesystem helpers
try:
    from rye.utils.path_utils import ensure_directory
except ImportError:
    # Fallback for when in .ai/tools context
    def ensure_directory(path: Path) -> Path:
        path = Path(path)
        path.mkdir(parents=True, exist_ok=True)
        return path

from rye.constants import AI_DIR

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
    "login_email",
    "logout",
    "whoami",
    # API keys
    "create_api_key",
    "list_api_keys",
    "revoke_api_key",
    # Items
    "search",
    "pull",
    "push",
    "delete",
    "publish",
    "unpublish",
    # Bundles
    "push_bundle",
    "pull_bundle",
]

# Registry configuration from environment
# API service on Railway (handles push/pull/search)
REGISTRY_API_URL = os.environ.get(
    "RYE_REGISTRY_API_URL", "https://rye-os-production.up.railway.app"
)
# Supabase for auth (device-auth flow)
REGISTRY_AUTH_URL = os.environ.get(
    "RYE_REGISTRY_AUTH_URL", "https://jvdgicalhvhaqtcalseq.supabase.co"
)
REGISTRY_ANON_KEY = os.environ.get(
    "RYE_REGISTRY_ANON_KEY",
    "sb_publishable_ZLeTVLX5wvbhyT5blq4gpg_67eWmaim"  # Default publishable key
)

# Auth configuration
# Service key for keyring storage (kernel uses service_name="lillux" by default)
REGISTRY_SERVICE = "rye_registry"
# Env var override for CI/headless - checked before keyring
REGISTRY_API_KEY_ENV = "RYE_REGISTRY_API_KEY"  # Primary: rye_sk_... API key


# =============================================================================
# ITEM ID HELPERS
# =============================================================================


def parse_item_id(item_id: str) -> Tuple[str, str, str]:
    """Parse item_id into (namespace, category, name).
    
    Format: namespace/category/name where category may contain slashes.
    Minimum 3 segments required.
    
    Returns:
        Tuple of (namespace, category, name)
    
    Raises:
        ValueError if item_id has fewer than 3 segments
    """
    segments = item_id.split("/")
    if len(segments) < 3:
        raise ValueError(
            f"item_id must have at least 3 segments (namespace/category/name), got: {item_id}"
        )
    namespace = segments[0]
    name = segments[-1]
    category = "/".join(segments[1:-1])
    return namespace, category, name


def build_item_id(namespace: str, category: str, name: str) -> str:
    """Build item_id from components."""
    return f"{namespace}/{category}/{name}"


def build_item_id_from_path(
    file_path: Path,
    namespace: str,
    item_type: str,
    project_path: Optional[Path] = None,
) -> str:
    """Build item_id from a local file path.
    
    Extracts category from path and combines with namespace and filename.
    
    Args:
        file_path: Path to the item file
        namespace: Owner namespace (usually authenticated username)
        item_type: "directive", "tool", or "knowledge"
        project_path: Optional project root for relative path calculation
    
    Returns:
        item_id in format namespace/category/name
    """
    from rye.utils.path_utils import extract_category_path
    
    name = file_path.stem
    category = extract_category_path(
        file_path, item_type, location="project", project_path=project_path
    )
    
    # Ensure category is not empty
    if not category:
        category = "uncategorized"
    
    return build_item_id(namespace, category, name)


def _get_rye_state_dir() -> Path:
    """Get RYE state directory.

    Uses rye's get_user_space() (defaults to ~, respects $USER_SPACE).
    """
    from rye.utils.path_utils import get_user_space

    return get_user_space()


def _get_session_dir() -> Path:
    """Get sessions directory under RYE state."""
    return _get_rye_state_dir() / "sessions"


def _get_api_key_from_env() -> Optional[str]:
    """Check for API key in env var (primary non-interactive auth)."""
    return os.environ.get(REGISTRY_API_KEY_ENV)


async def _resolve_auth_token(scope: str = "registry:read") -> Optional[str]:
    """Resolve auth token from all sources in priority order.

    1. RYE_REGISTRY_API_KEY env var (rye_sk_... API key)
    2. Keyring via AuthStore

    Returns token string or None if no auth available.
    """
    # 1. API key env var (primary)
    api_key = _get_api_key_from_env()
    if api_key:
        return api_key

    # 2. Keyring
    try:
        from lillux.runtime.auth import AuthStore
        auth_store = AuthStore()
        if auth_store.is_authenticated(REGISTRY_SERVICE):
            return await auth_store.get_token(REGISTRY_SERVICE, scope=scope)
    except Exception:
        pass

    return None


@dataclass
class RegistryConfig:
    """Registry connection configuration."""

    api_url: str  # Railway API for push/pull/search
    auth_url: str  # Supabase for device-auth
    anon_key: str

    @classmethod
    def from_env(cls) -> "RegistryConfig":
        return cls(
            api_url=REGISTRY_API_URL,
            auth_url=REGISTRY_AUTH_URL,
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
            from lillux.primitives.http_client import HttpClientPrimitive

            self._http = HttpClientPrimitive()
        return self._http

    def _get_base_url(self, path: str) -> str:
        """Get appropriate base URL based on path.
        
        Auth endpoints (/auth/*, /functions/*) go to Supabase.
        API endpoints (/v1/*) go to Railway.
        """
        if path.startswith("/auth/") or path.startswith("/functions/"):
            return self.config.auth_url
        return self.config.api_url

    async def get(
        self,
        path: str,
        headers: Optional[Dict] = None,
        auth_token: Optional[str] = None,
    ) -> Dict:
        """Make GET request to registry API."""
        http = await self._get_http()
        base_url = self._get_base_url(path)

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
            "url": f"{base_url}{path}",
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
        base_url = self._get_base_url(path)

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
            "url": f"{base_url}{path}",
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

    async def delete(
        self,
        path: str,
        headers: Optional[Dict] = None,
        auth_token: Optional[str] = None,
    ) -> Dict:
        """Make DELETE request to registry API."""
        http = await self._get_http()
        base_url = self._get_base_url(path)

        req_headers = {
            "apikey": self.config.anon_key,
            "Content-Type": "application/json",
        }
        if auth_token:
            req_headers["Authorization"] = f"Bearer {auth_token}"
        if headers:
            req_headers.update(headers)

        config = {
            "method": "DELETE",
            "url": f"{base_url}{path}",
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
        elif action == "login_email":
            result = await _login_email(params)
        elif action == "logout":
            result = await _logout()
        elif action == "whoami":
            result = await _whoami()

        # API key actions
        elif action == "create_api_key":
            result = await _create_api_key(params)
            http_calls = 1
        elif action == "list_api_keys":
            result = await _list_api_keys()
            http_calls = 1
        elif action == "revoke_api_key":
            result = await _revoke_api_key(params)
            http_calls = 1

        # Item actions
        elif action == "search":
            result = await _search(
                query=params.get("query"),
                item_type=params.get("item_type"),
                category=params.get("category"),
                namespace=params.get("namespace"),
                include_mine=params.get("include_mine", False),
                limit=params.get("limit", 20),
            )
            http_calls = 1
        elif action == "pull":
            result = await _pull(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
                version=params.get("version"),
                location=params.get("location", "project"),
                dest_path=params.get("dest_path"),
                project_path=project_path,
                verify=params.get("verify", True),
            )
            http_calls = 1  # pull makes HTTP requests
        elif action == "push":
            result = await _push(
                item_type=params.get("item_type"),
                item_path=params.get("item_path"),
                item_id=params.get("item_id"),
                version=params.get("version"),
                visibility=params.get("visibility", "private"),
                project_path=project_path,
            )
            http_calls = 2  # push typically makes 2 HTTP requests (check + create)
        elif action == "delete":
            result = await _delete(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
                version=params.get("version"),
            )
            http_calls = 1
        elif action == "publish":
            result = await _publish(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
            )
            http_calls = 1
        elif action == "unpublish":
            result = await _unpublish(
                item_type=params.get("item_type"),
                item_id=params.get("item_id"),
            )
            http_calls = 1

        # Bundle actions
        elif action == "push_bundle":
            result = await _push_bundle(
                bundle_id=params.get("bundle_id"),
                version=params.get("version"),
                project_path=params.get("project_path", project_path),
            )
            http_calls = 1
        elif action == "pull_bundle":
            result = await _pull_bundle(
                bundle_id=params.get("bundle_id"),
                version=params.get("version"),
                project_path=project_path,
            )
            http_calls = 1
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
    ensure_directory(session_dir)
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
                from lillux.runtime.auth import AuthStore

                auth_store = AuthStore()  # Uses kernel default service_name="lillux"
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


async def _login_email(params: Dict[str, Any]) -> Dict[str, Any]:
    """
    Login with email/password directly (no OAuth).

    Args:
        email: User's email address (or set RYE_REGISTRY_EMAIL env var)
        password: User's password (or set RYE_REGISTRY_PASSWORD env var)
    """
    email = params.get("email") or os.environ.get("RYE_REGISTRY_EMAIL")
    password = params.get("password") or os.environ.get("RYE_REGISTRY_PASSWORD")

    if not email or not password:
        return {
            "error": "Required: email and password",
            "usage": "login_email(email='you@example.com', password='yourpass')",
            "hint": "Or set RYE_REGISTRY_EMAIL and RYE_REGISTRY_PASSWORD env vars",
        }

    try:
        from lillux.runtime.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        # Call Supabase Auth token endpoint
        result = await http.post(
            "/auth/v1/token?grant_type=password",
            body={
                "email": email,
                "password": password,
            },
        )

        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            error_msg = (
                error_body.get("error_description")
                or error_body.get("msg")
                or error_body.get("error")
                or result.get("error")
                or "Unknown error"
            )
            return {
                "error": f"Login failed: {error_msg}",
                "status_code": result.get("status_code"),
            }

        body = result["body"]
        access_token = body.get("access_token")
        refresh_token = body.get("refresh_token")
        expires_in = body.get("expires_in", 3600)

        if not access_token:
            return {"error": "No access token in response"}

        # Store in keyring
        auth_store = AuthStore()
        auth_store.set_token(
            service=REGISTRY_SERVICE,
            access_token=access_token,
            refresh_token=refresh_token,
            expires_in=expires_in,
            scopes=["registry:read", "registry:write"],
        )

        return {
            "status": "authenticated",
            "message": "Successfully logged in to Rye Registry",
            "user": body.get("user", {}),
        }

    except Exception as e:
        await http.close()
        return {"error": f"Login failed: {e}"}


async def _login(params: Dict[str, Any]) -> Dict[str, Any]:
    """
    Device authorization flow.

    1. Generate session ID + ECDH keypair
    2. Open browser to registry auth page with public key
    3. Poll until auth completes or times out
    """
    if not CRYPTO_AVAILABLE:
        return {
            "error": "cryptography library required for device auth",
            "solution": "pip install cryptography",
        }

    try:
        from lillux.runtime.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available - auth runtime not installed"}

    # Check if already authenticated via keyring
    auth_store = AuthStore()  # Uses kernel default service_name="lillux"
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

    # Use device-auth edge function which creates session in DB then redirects to OAuth
    auth_url = f"{config.auth_url}/functions/v1/device-auth?{auth_params}"

    # Save session for later polling
    expires_at = (datetime.now(timezone.utc).replace(microsecond=0)).isoformat()
    _save_session(session_id, private_key, public_key, expires_at)

    # Open browser
    open_browser = params.get("open_browser", True)
    if open_browser:
        try:
            import subprocess
            import shutil
            
            # Try xdg-open first (Linux), then open (macOS), then webbrowser
            if shutil.which("xdg-open"):
                subprocess.Popen(["xdg-open", auth_url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            elif shutil.which("open"):
                subprocess.Popen(["open", auth_url], stdout=subprocess.DEVNULL, stderr=subprocess.DEVNULL)
            else:
                webbrowser.open(auth_url)
            browser_opened = True
        except Exception:
            browser_opened = False
    else:
        browser_opened = False

    # Poll for auth completion
    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    max_attempts = params.get("max_attempts", 60)
    interval = params.get("interval", 5)
    initial_delay = params.get("initial_delay", 3)

    # Wait before first poll to give the edge function time to create the session
    await asyncio.sleep(initial_delay)

    for attempt in range(max_attempts):
        result = await http.get(
            f"/functions/v1/device-auth-poll?session_id={session_id}"
        )

        if not result["success"]:
            if result["status_code"] == 404:
                # Grace period: session may not be created yet for the first few polls
                if attempt < 6:
                    if attempt < max_attempts - 1:
                        await asyncio.sleep(interval)
                        continue
                # After grace period, treat 404 as genuinely expired
                _delete_session(session_id)
                await http.close()
                return {
                    "error": "Session expired or not found",
                    "solution": "Run 'registry login' again",
                }
            # Any other error (network, 5xx, etc.) — retry
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
                try:
                    # Try ECDH decryption if server provided its public key
                    server_pub = body.get("server_public_key", "")
                    nonce_val = body.get("nonce", "")

                    if server_pub and nonce_val:
                        server_public_key = base64.b64decode(server_pub)
                        shared_secret = derive_shared_secret(private_key, server_public_key)
                        access_token = decrypt_token(
                            body["encrypted_token"],
                            nonce_val,
                            shared_secret,
                        )
                    else:
                        # Plaintext token (simplified flow)
                        access_token = body["encrypted_token"]

                    # Use temporary JWT to create a persistent API key
                    api_key_result = await http.post(
                        "/v1/api-keys",
                        body={"name": token_name},
                        auth_token=access_token,
                    )

                    _delete_session(session_id)

                    if api_key_result["success"] and api_key_result["body"]:
                        api_key = api_key_result["body"]["key"]

                        # Store API key in keyring (not the JWT)
                        auth_store.set_token(
                            service=REGISTRY_SERVICE,
                            access_token=api_key,
                            refresh_token=None,
                            expires_in=365 * 24 * 3600,  # API keys don't expire; use 1 year
                            scopes=["registry:read", "registry:write"],
                        )

                        await http.close()
                        return {
                            "status": "authenticated",
                            "message": "Successfully logged in to Rye Registry",
                            "api_key_name": token_name,
                            "api_key_prefix": api_key_result["body"].get("key_prefix", ""),
                            "user": body.get("user", {}),
                            "hint": "API key stored in keyring. For CI/serverless, set RYE_REGISTRY_API_KEY env var.",
                        }

                    # Fallback: store JWT if API key creation fails
                    auth_store.set_token(
                        service=REGISTRY_SERVICE,
                        access_token=access_token,
                        refresh_token=body.get("refresh_token"),
                        expires_in=body.get("expires_in", 3600),
                        scopes=["registry:read", "registry:write"],
                    )

                    await http.close()
                    return {
                        "status": "authenticated",
                        "message": "Logged in (API key creation failed, using session token)",
                        "user": body.get("user", {}),
                        "warning": "Run 'registry create_api_key' to create a persistent API key.",
                    }

                except Exception as e:
                    await http.close()
                    return {
                        "error": f"Failed to process token: {e}",
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
    # Check if using API key env var
    if _get_api_key_from_env():
        return {
            "status": "env_var",
            "message": f"Using {REGISTRY_API_KEY_ENV} env var. Unset it to logout.",
        }

    try:
        from lillux.runtime.auth import AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    auth_store = AuthStore()  # Uses kernel default service_name="lillux"
    auth_store.clear_token(REGISTRY_SERVICE)

    return {
        "status": "logged_out",
        "message": "Successfully logged out from Rye Registry",
    }


async def _whoami() -> Dict[str, Any]:
    """Show current authenticated user."""
    # Check API key env var first (primary)
    api_key = _get_api_key_from_env()
    if api_key:
        return {
            "authenticated": True,
            "source": "api_key",
            "env_var": REGISTRY_API_KEY_ENV,
            "key_prefix": api_key[7:15] if len(api_key) > 15 else "***",
            "message": f"Using API key from {REGISTRY_API_KEY_ENV} environment variable",
        }

    try:
        from lillux.runtime.auth import AuthenticationRequired, AuthStore
    except ImportError:
        return {"error": "AuthStore not available"}

    auth_store = AuthStore()  # Uses kernel default service_name="lillux"

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
# API KEY ACTIONS
# =============================================================================


async def _create_api_key(params: Dict[str, Any]) -> Dict[str, Any]:
    """Create a new API key for non-interactive auth.

    Requires an existing auth session (OAuth or API key).
    The raw key is stored in the OS keyring — never returned to the LLM.
    """
    name = params.get("name")
    if not name:
        # Auto-generate name from hostname
        import getpass
        import platform
        try:
            username = getpass.getuser()
            hostname = platform.node()
        except Exception:
            username = "user"
            hostname = "device"
        name = f"{username}@{hostname}-{datetime.now().strftime('%Y%m%d%H%M%S')}"

    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first to create an auth session, then create an API key.",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        body: Dict[str, Any] = {"name": name}
        scopes = params.get("scopes")
        if scopes:
            body["scopes"] = scopes
        expires_in_days = params.get("expires_in_days")
        if expires_in_days:
            body["expires_in_days"] = expires_in_days

        result = await http.post("/v1/api-keys", body=body, auth_token=token)
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            detail = error_body.get("detail", result.get("error", "Unknown error"))
            return {"error": f"Failed to create API key: {detail}"}

        key_data = result["body"]
        raw_key = key_data["key"]

        # Store in keyring immediately — raw key never leaves this function
        try:
            from lillux.runtime.auth import AuthStore
            auth_store = AuthStore()
            auth_store.set_token(
                service=REGISTRY_SERVICE,
                access_token=raw_key,
                refresh_token=None,
                expires_in=365 * 24 * 3600,
                scopes=key_data.get("scopes", ["registry:read", "registry:write"]),
            )
            stored = True
        except Exception:
            stored = False

        return {
            "status": "created",
            "name": key_data["name"],
            "key_prefix": key_data["key_prefix"],
            "scopes": key_data.get("scopes", []),
            "expires_at": key_data.get("expires_at"),
            "stored_in_keyring": stored,
            "message": (
                f"API key created: {key_data['name']} (prefix: {key_data['key_prefix']})\n"
                f"Stored securely in OS keyring."
            ),
        }

    except Exception as e:
        await http.close()
        return {"error": f"Failed to create API key: {e}"}


async def _list_api_keys() -> Dict[str, Any]:
    """List all API keys for the current user."""
    token = await _resolve_auth_token()
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first.",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        result = await http.get("/v1/api-keys", auth_token=token)
        await http.close()

        if not result["success"]:
            return {"error": f"Failed to list API keys: {result.get('error', 'Unknown')}"}

        body = result["body"]
        return {
            "status": "success",
            "keys": body.get("keys", []),
            "count": body.get("count", 0),
        }

    except Exception as e:
        await http.close()
        return {"error": f"Failed to list API keys: {e}"}


async def _revoke_api_key(params: Dict[str, Any]) -> Dict[str, Any]:
    """Revoke an API key by name."""
    name = params.get("name")
    if not name:
        return {
            "error": "Required: name",
            "usage": "revoke_api_key(name='my-key-name')",
        }

    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first.",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        result = await http.delete(f"/v1/api-keys/{name}", auth_token=token)
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            detail = error_body.get("detail", result.get("error", "Unknown error"))
            return {"error": f"Failed to revoke API key: {detail}"}

        return {
            "status": "revoked",
            "name": name,
            "message": f"API key '{name}' has been revoked.",
        }

    except Exception as e:
        await http.close()
        return {"error": f"Failed to revoke API key: {e}"}


# =============================================================================
# ITEM ACTIONS
# =============================================================================


async def _search(
    query: Optional[str],
    item_type: Optional[str] = None,
    category: Optional[str] = None,
    namespace: Optional[str] = None,
    include_mine: bool = False,
    limit: int = 20,
) -> Dict[str, Any]:
    """
    Search for items in the registry via Registry API.

    Args:
        query: Search query (searches name and description)
        item_type: Filter by type ("directive", "tool", or "knowledge")
        category: Filter by category prefix
        namespace: Filter by namespace (owner)
        include_mine: Include your own private items (requires auth)
        limit: Maximum results to return (default 20)
    """
    if not query:
        return {
            "error": "Required: query",
            "usage": "search(query='bootstrap', item_type='directive')",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)
    
    # Get auth token if include_mine is requested
    token = None
    if include_mine:
        token = await _resolve_auth_token(scope="registry:read")

    try:
        # Build query params for Registry API
        url = f"/v1/search?query={query}&limit={limit}"
        if item_type:
            url += f"&item_type={item_type}"
        if category:
            url += f"&category={category}"
        if namespace:
            url += f"&namespace={namespace}"
        if include_mine and token:
            url += "&include_mine=true"

        result = await http.get(url, auth_token=token)
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
                "namespace": namespace,
                "include_mine": include_mine and token is not None,
            },
        }

    except Exception as e:
        await http.close()
        return {"error": f"Search failed: {e}"}


async def _pull(
    item_type: Optional[str],
    item_id: Optional[str],
    version: Optional[str],
    location: str = "project",
    dest_path: Optional[str] = None,
    project_path: Optional[str] = None,
    verify: bool = True,
) -> Dict[str, Any]:
    """
    Download item from registry via Registry API with signature verification.

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_id: Item identifier (namespace/category/name format)
                 Example: "leolilley/core/bootstrap"
        version: Specific version (or "latest")
        location: Where to install - "project" (.ai/) or "user" (~/.ai/)
        dest_path: Override destination path (optional)
        project_path: Project root path (used when location="project")
        verify: Verify registry signature (default True)
    """
    if not item_type or not item_id:
        return {
            "error": "Required: item_type and item_id",
            "usage": "pull(item_type='directive', item_id='leolilley/core/bootstrap')",
        }
    
    # Validate item_id format
    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        return {
            "error": str(e),
            "hint": "item_id must be namespace/category/name format",
            "example": "leolilley/core/bootstrap",
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

        # Verify registry Ed25519 signature locally if enabled
        signature_info = None
        if verify and content:
            try:
                from rye.utils.metadata_manager import MetadataManager
                from lillux.primitives.signing import verify_signature, compute_key_fingerprint
                from rye.utils.trust_store import TrustStore

                strategy = MetadataManager.get_strategy(item_type)
                sig_info = strategy.extract_signature(content)

                if not sig_info:
                    return {
                        "error": "No signature found on registry content",
                        "hint": "Content may be corrupted or from an older registry version",
                    }

                registry_username = sig_info.get("registry_username")
                if registry_username:
                    if author_username and registry_username != author_username:
                        return {
                            "error": "Signature username mismatch",
                            "signature_says": registry_username,
                            "registry_says": author_username,
                            "hint": "Content may have been tampered with",
                        }

                content_without_sig = strategy.remove_signature(content)
                content_for_hash = strategy.extract_content_for_hash(content)
                computed_hash = hashlib.sha256(content_for_hash.encode()).hexdigest()

                if computed_hash != sig_info["hash"]:
                    return {
                        "error": "Content integrity check failed",
                        "expected_hash": sig_info["hash"],
                        "computed_hash": computed_hash,
                        "hint": "Content was modified after signing",
                    }

                trust_store = TrustStore()
                pubkey_fp = sig_info["pubkey_fp"]
                registry_key = trust_store.get_registry_key()

                if registry_key is None:
                    # TOFU: fetch and pin registry key on first pull
                    try:
                        key_url = f"{REGISTRY_API_URL}/v1/public-key"
                        import urllib.request
                        req = urllib.request.Request(key_url)
                        with urllib.request.urlopen(req, timeout=10) as resp:
                            registry_key = resp.read()
                        trust_store.pin_registry_key(registry_key)
                        logger.info("Pinned registry public key (TOFU)")
                    except Exception as e:
                        logger.warning(f"Failed to fetch registry key: {e}")

                if registry_key:
                    if not verify_signature(sig_info["hash"], sig_info["ed25519_sig"], registry_key):
                        return {
                            "error": "Ed25519 signature verification failed",
                            "hint": "Registry content signature is invalid",
                        }

                signature_info = {
                    "verified": True,
                    "registry_username": registry_username,
                    "timestamp": sig_info.get("timestamp"),
                    "hash": sig_info.get("hash"),
                    "pubkey_fp": sig_info.get("pubkey_fp"),
                }

            except ImportError:
                signature_info = {
                    "verified": False,
                    "reason": "Signing dependencies not available",
                }

        # Determine destination path
        if dest_path:
            # Explicit destination provided
            dest = Path(dest_path)
            if dest.is_dir():
                ext = ".md" if item_type in ["directive", "knowledge"] else ".py"
                dest = dest / f"{name}{ext}"
        else:
            # Use location to determine base directory
            if location == "user":
                base_dir = _get_rye_state_dir()
            else:
                # project (default)
                base_dir = Path(project_path) / AI_DIR if project_path else Path(AI_DIR)
            
            # Build path like {base}/{item_type}s/{category}/{name}.ext
            ext = ".md" if item_type in ["directive", "knowledge"] else ".py"
            dest = base_dir / f"{item_type}s" / category / f"{name}{ext}"

        # Create directory and write content
        ensure_directory(dest.parent)
        dest.write_text(content)

        return {
            "status": "pulled",
            "item_type": item_type,
            "item_id": item_id,
            "namespace": namespace,
            "category": category,
            "name": name,
            "version": item_version,
            "location": location,
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
    item_id: Optional[str],
    version: Optional[str],
    visibility: str = "private",
    project_path: Optional[str] = None,
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
        item_id: Registry identifier (namespace/category/name format)
                 Example: "leolilley/core/bootstrap"
        version: Version string (semver)
        visibility: "public", "private", or "unlisted"
    """
    if not item_type or not item_path or not item_id or not version:
        return {
            "error": "Required: item_type, item_path, item_id, version",
            "usage": "push(item_type='tool', item_path='.ai/tools/test/my_tool.py', item_id='leolilley/test/my_tool', version='1.0.0')",
        }
    
    # Validate item_id format
    try:
        namespace, category, name = parse_item_id(item_id)
    except ValueError as e:
        return {
            "error": str(e),
            "hint": "item_id must be namespace/category/name format",
            "example": "leolilley/core/bootstrap",
        }

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    path = Path(item_path)
    if not path.exists():
        return {"error": f"File not found: {item_path}"}

    # Check auth
    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

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
            project_path=Path(project_path) if project_path else None,
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
                "item_id": item_id,
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
            "item_id": item_id,
            "version": version,
            "visibility": visibility,
            "content_hash": response_body.get("signature", {}).get("hash", ""),
            "registry_username": response_body.get("signature", {}).get(
                "registry_username"
            ),
            "size_bytes": len(signed_content.encode()),
            "local_updated": "signed_content" in response_body,
        }

    except Exception as e:
        await http.close()
        return {"error": f"Push failed: {e}"}


async def _delete(
    item_type: Optional[str],
    item_id: Optional[str],
    version: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Delete item from registry.

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_id: Item identifier (namespace/category/name format)
        version: Specific version to delete (or None for all versions)
    """
    if not item_type or not item_id:
        return {
            "error": "Required: item_type, item_id",
            "usage": "delete(item_type='directive', item_id='leolilley/core/bootstrap')",
        }
    
    # Validate item_id format
    try:
        parse_item_id(item_id)
    except ValueError as e:
        return {"error": str(e)}

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    # Check auth
    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        url = f"/v1/delete/{item_type}/{item_id}"
        if version:
            url += f"?version={version}"

        result = await http.delete(url, auth_token=token)
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            if isinstance(error_body, dict) and "error" in error_body:
                return {
                    "error": error_body["error"],
                    "status_code": result["status_code"],
                }
            return {
                "error": f"Delete failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        return {
            "status": "deleted",
            "item_type": item_type,
            "item_id": item_id,
            "version": version or "all",
        }

    except Exception as e:
        await http.close()
        return {"error": f"Delete failed: {e}"}


async def _publish(
    item_type: Optional[str],
    item_id: Optional[str],
) -> Dict[str, Any]:
    """
    Make item public (set visibility to 'public').

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_id: Item identifier (namespace/category/name format)
    """
    if not item_type or not item_id:
        return {
            "error": "Required: item_type, item_id",
            "usage": "publish(item_type='directive', item_id='leolilley/core/bootstrap')",
        }
    
    # Validate item_id format
    try:
        parse_item_id(item_id)
    except ValueError as e:
        return {"error": str(e)}

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    # Check auth
    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        result = await http.post(
            f"/v1/visibility/{item_type}/{item_id}",
            body={"visibility": "public"},
            auth_token=token,
        )
        await http.close()

        if not result["success"]:
            return {
                "error": f"Publish failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        return {
            "status": "published",
            "item_type": item_type,
            "item_id": item_id,
            "visibility": "public",
        }

    except Exception as e:
        await http.close()
        return {"error": f"Publish failed: {e}"}


async def _unpublish(
    item_type: Optional[str],
    item_id: Optional[str],
) -> Dict[str, Any]:
    """
    Make item private (set visibility to 'private').

    Args:
        item_type: "directive", "tool", or "knowledge"
        item_id: Item identifier (namespace/category/name format)
    """
    if not item_type or not item_id:
        return {
            "error": "Required: item_type, item_id",
            "usage": "unpublish(item_type='directive', item_id='leolilley/core/bootstrap')",
        }
    
    # Validate item_id format
    try:
        parse_item_id(item_id)
    except ValueError as e:
        return {"error": str(e)}

    if item_type not in ["directive", "tool", "knowledge"]:
        return {
            "error": f"Invalid item_type: {item_type}",
            "valid": ["directive", "tool", "knowledge"],
        }

    # Check auth
    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        result = await http.post(
            f"/v1/visibility/{item_type}/{item_id}",
            body={"visibility": "private"},
            auth_token=token,
        )
        await http.close()

        if not result["success"]:
            return {
                "error": f"Unpublish failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        return {
            "status": "unpublished",
            "item_type": item_type,
            "item_id": item_id,
            "visibility": "private",
        }

    except Exception as e:
        await http.close()
        return {"error": f"Unpublish failed: {e}"}


# =============================================================================
# BUNDLE ACTIONS
# =============================================================================


async def _push_bundle(
    bundle_id: Optional[str] = None,
    version: Optional[str] = None,
    project_path: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Upload a bundle to the registry.

    Reads manifest from .ai/bundles/{bundle_id}/manifest.yaml, verifies
    integrity of all listed files, then pushes the bundle to the registry.

    Args:
        bundle_id: Bundle identifier
        version: Version string (optional, defaults to manifest version)
        project_path: Project root path
    """
    if not bundle_id:
        return {
            "error": "Required: bundle_id",
            "usage": "push_bundle(bundle_id='my-bundle', version='1.0.0')",
        }

    base_dir = Path(project_path) / AI_DIR if project_path else Path(AI_DIR)
    bundle_dir = base_dir / "bundles" / bundle_id
    manifest_path = bundle_dir / "manifest.yaml"

    if not manifest_path.exists():
        return {
            "error": f"Manifest not found: {manifest_path}",
            "hint": f"Expected manifest at .ai/bundles/{bundle_id}/manifest.yaml",
        }

    # Load and parse manifest
    import yaml

    manifest_content = manifest_path.read_text()
    try:
        manifest = yaml.safe_load(manifest_content)
    except yaml.YAMLError as e:
        return {"error": f"Invalid manifest YAML: {e}"}

    if not isinstance(manifest, dict) or "files" not in manifest:
        return {
            "error": "Manifest must contain a 'files' key",
            "hint": "manifest.yaml should have a top-level 'files' key (dict or list)",
        }

    # Verify manifest signature
    try:
        from rye.utils.integrity import verify_item, IntegrityError

        verify_item(manifest_path, "tool", project_path=Path(project_path) if project_path else None)
    except IntegrityError as e:
        return {
            "error": f"Manifest signature verification failed: {e}",
            "hint": "Sign the manifest with the sign tool before pushing",
        }
    except ImportError:
        pass  # Integrity module not available, skip verification

    # Read and verify each file
    files: Dict[str, Dict[str, Any]] = {}
    file_entries = manifest.get("files", {})
    if isinstance(file_entries, dict):
        file_iter = list(file_entries.items())
    else:
        # Fallback for list format
        file_iter = [(e if isinstance(e, str) else e.get("path", ""), e if isinstance(e, dict) else {}) for e in file_entries]

    for rel_path, meta in file_iter:
        expected_sha = meta.get("sha256") if isinstance(meta, dict) else None

        # rel_path is relative to project root (e.g. ".ai/tools/..."), not to .ai/
        proj_root = Path(project_path) if project_path else Path(".")
        file_path = proj_root / rel_path
        if not file_path.exists():
            return {
                "error": f"Bundle file not found: {rel_path}",
                "expected_at": str(file_path),
            }

        content = file_path.read_text()
        computed_sha = hashlib.sha256(content.encode()).hexdigest()

        if expected_sha and computed_sha != expected_sha:
            return {
                "error": f"SHA256 mismatch for {rel_path}",
                "expected": expected_sha,
                "computed": computed_sha,
            }

        # Check if file has an inline signature
        inline_signed = "rye:signed:" in content

        files[rel_path] = {
            "content": content,
            "sha256": computed_sha,
            "inline_signed": inline_signed,
        }

    # Auth check
    token = await _resolve_auth_token(scope="registry:write")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

    # Push to registry
    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        result = await http.post(
            "/v1/bundle/push",
            body={
                "bundle_id": bundle_id,
                "version": version,
                "manifest": manifest_content,
                "files": files,
            },
            auth_token=token,
        )
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            if isinstance(error_body, dict) and "error" in error_body:
                return {
                    "error": error_body["error"],
                    "status_code": result["status_code"],
                }
            return {
                "error": f"Push bundle failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        return {
            "status": "pushed",
            "bundle_id": bundle_id,
            "version": version,
            "file_count": len(files),
        }

    except Exception as e:
        await http.close()
        return {"error": f"Push bundle failed: {e}"}


async def _pull_bundle(
    bundle_id: Optional[str] = None,
    version: Optional[str] = None,
    project_path: Optional[str] = None,
) -> Dict[str, Any]:
    """
    Download a bundle from the registry.

    Fetches manifest and all files, writes them under .ai/, then verifies
    manifest signature and file integrity.

    Args:
        bundle_id: Bundle identifier
        version: Specific version (optional)
        project_path: Project root path
    """
    if not bundle_id:
        return {
            "error": "Required: bundle_id",
            "usage": "pull_bundle(bundle_id='my-bundle')",
        }

    # Auth check
    token = await _resolve_auth_token(scope="registry:read")
    if not token:
        return {
            "error": "Authentication required",
            "solution": "Run 'registry login' first",
        }

    # Pull from registry
    config = RegistryConfig.from_env()
    http = RegistryHttpClient(config)

    try:
        url = f"/v1/bundle/pull/{bundle_id}"
        if version:
            url += f"?version={version}"

        result = await http.get(url, auth_token=token)
        await http.close()

        if not result["success"]:
            error_body = result.get("body", {})
            if isinstance(error_body, dict) and "error" in error_body:
                return {
                    "error": error_body["error"],
                    "status_code": result["status_code"],
                }
            return {
                "error": f"Pull bundle failed: {result.get('error', 'Unknown error')}",
                "status_code": result.get("status_code"),
            }

        body = result.get("body", {})
        manifest_content = body.get("manifest", "")
        bundle_files = body.get("files", {})
        pulled_version = body.get("version", version)

        base_dir = Path(project_path) / AI_DIR if project_path else Path(AI_DIR)

        # Write manifest
        bundle_dir = base_dir / "bundles" / bundle_id
        ensure_directory(bundle_dir)
        manifest_path = bundle_dir / "manifest.yaml"
        manifest_path.write_text(manifest_content)

        # Write each file (rel_path is relative to project root, e.g. ".ai/tools/...")
        proj_root = Path(project_path) if project_path else Path(".")
        files_written: List[str] = []
        for rel_path, file_data in bundle_files.items():
            content = file_data.get("content", "") if isinstance(file_data, dict) else file_data
            dest = proj_root / rel_path
            ensure_directory(dest.parent)
            dest.write_text(content)
            files_written.append(rel_path)

        # Verify manifest signature after writing
        try:
            from rye.utils.integrity import verify_item, IntegrityError

            verify_item(manifest_path, "tool", project_path=Path(project_path) if project_path else None)
        except IntegrityError as e:
            return {
                "error": f"Manifest signature verification failed after pull: {e}",
                "hint": "Bundle manifest from registry has invalid signature",
                "files_written": files_written,
            }
        except ImportError:
            pass  # Integrity module not available, skip verification

        # Verify each file's SHA256 against manifest entries
        import yaml

        try:
            manifest = yaml.safe_load(manifest_content)
        except yaml.YAMLError:
            manifest = {}

        if isinstance(manifest, dict) and "files" in manifest:
            file_entries = manifest["files"]
            if isinstance(file_entries, dict):
                file_iter = list(file_entries.items())
            else:
                # Fallback for list format
                file_iter = [(e if isinstance(e, str) else e.get("path", ""), e if isinstance(e, dict) else {}) for e in file_entries]

            for rel_path, meta in file_iter:
                expected_sha = meta.get("sha256") if isinstance(meta, dict) else None
                if expected_sha and rel_path:
                    file_path = proj_root / rel_path
                    if file_path.exists():
                        computed_sha = hashlib.sha256(file_path.read_text().encode()).hexdigest()
                        if computed_sha != expected_sha:
                            return {
                                "error": f"SHA256 mismatch for {rel_path}",
                                "expected": expected_sha,
                                "computed": computed_sha,
                                "files_written": files_written,
                            }

        return {
            "status": "pulled",
            "bundle_id": bundle_id,
            "version": pulled_version,
            "file_count": len(files_written),
            "files_written": files_written,
        }

    except Exception as e:
        await http.close()
        return {"error": f"Pull bundle failed: {e}"}


# CLI entry point for subprocess execution
if __name__ == "__main__":
    import argparse
    import sys

    parser = argparse.ArgumentParser(description="Registry Tool")
    parser.add_argument("--params", required=True, help="Parameters as JSON")
    parser.add_argument("--project-path", required=True, help="Project path")

    args = parser.parse_args()

    try:
        params = json.loads(args.params)
        action = params.pop("action", None)
        if not action:
            print(json.dumps({"success": False, "error": "action required in params"}))
            sys.exit(1)
    except json.JSONDecodeError as e:
        print(json.dumps({"success": False, "error": f"Invalid params JSON: {e}"}))
        sys.exit(1)

    try:
        result = asyncio.run(execute(action, args.project_path, params))
        # Normalize result format
        if "error" in result:
            result["success"] = False
        elif "success" not in result:
            result["success"] = True
        print(json.dumps(result, indent=2), flush=True)
        sys.exit(0 if result.get("success") else 1)
    except Exception as e:
        print(json.dumps({"success": False, "error": str(e)}), flush=True)
        sys.exit(1)
