# Lilux API Reference

Complete API reference for all public Lilux classes and functions.

---

## Primitives

### SubprocessPrimitive

```python
from lilux.primitives import SubprocessPrimitive

class SubprocessPrimitive:
    async def execute(
        self,
        config: Dict[str, Any],
        params: Dict[str, Any]
    ) -> SubprocessResult:
        """
        Execute a subprocess command.

        Args:
            config: Static tool configuration
                - command (str, required): Executable name or path
                - args (List[str]): Command arguments
                - env (Dict[str, str]): Environment variables
                - cwd (str): Working directory
                - timeout (int): Timeout in seconds (default: 300)
                - capture_output (bool): Capture stdout/stderr (default: True)
                - input_data (str): Data to send to stdin

            params: Runtime parameters for {param} templating
                - Any key-value pairs to substitute in command/args/cwd/input_data

        Returns:
            SubprocessResult

        Templating:
            1. Environment variables: ${VAR:-default} (resolved first)
            2. Runtime params: {param_name} (resolved second)
        """
```

### SubprocessResult

```python
@dataclass
class SubprocessResult:
    success: bool        # Whether exit code was 0
    stdout: str          # Standard output
    stderr: str          # Standard error
    return_code: int     # Process exit code
    duration_ms: int     # Execution time in milliseconds
```

---

### HttpClientPrimitive

```python
from lilux.primitives import HttpClientPrimitive

class HttpClientPrimitive:
    async def execute(
        self,
        config: Dict[str, Any],
        params: Dict[str, Any]
    ) -> HttpResult:
        """
        Execute an HTTP request.

        Args:
            config: Static tool configuration
                - url (str, required): Target URL (supports {param} and ${VAR:-default})
                - method (str): HTTP method (default: "GET")
                - headers (Dict[str, str]): Request headers (supports ${VAR:-default})
                - body (Any): Request body with recursive {param} templating
                - timeout (int): Timeout in seconds (default: 30)
                - retry (Dict): Retry configuration
                    - max_attempts (int): Max retries (default: 1)
                    - backoff (str): "exponential" or "linear"
                - auth (Dict): Authentication configuration
                    - type (str): "bearer" or "api_key"
                    - token (str): For bearer auth (supports ${VAR})
                    - key (str): For api_key auth (supports ${VAR})
                    - header (str): Custom header for api_key (default: "X-API-Key")

            params: Runtime parameters
                - mode (str): "sync" or "stream" (default: "sync")
                - __sinks (List[Sink]): Pre-instantiated sink objects (streaming only)
                - Any other keys for URL/body {param} templating

        Returns:
            HttpResult
        """

    async def close(self) -> None:
        """Close the HTTP client connection pool."""
```

### HttpResult

```python
@dataclass
class HttpResult:
    success: bool                           # Request succeeded (2xx/3xx)
    status_code: int                        # HTTP status code
    body: Any                               # Response (dict for JSON, str for text, List[str] for streaming)
    headers: Dict[str, str]                 # Response headers
    duration_ms: int                        # Request time in milliseconds
    error: Optional[str]                    # Error message if failed
    stream_events_count: Optional[int]      # Number of SSE events (streaming only)
    stream_destinations: Optional[List[str]] # Sink class names used (streaming only)
```

### StreamDestination & StreamConfig

```python
@dataclass
class StreamDestination:
    type: str                          # "return" (built-in) or tool-based sinks
    path: Optional[str] = None
    config: Optional[Dict[str, Any]] = None
    format: str = "jsonl"

@dataclass
class StreamConfig:
    transport: str                     # "sse" only (WebSocket is a separate tool sink)
    destinations: List[StreamDestination]
    buffer_events: bool = False
    max_buffer_size: int = 10_000
```

### ReturnSink

```python
from lilux.primitives.http_client import ReturnSink

class ReturnSink:
    """Built-in sink that buffers streaming events in memory."""

    def __init__(self, max_size: int = 10000):
        """Initialize with maximum buffer size."""

    async def write(self, event: str) -> None:
        """Buffer an event (drops if buffer full)."""

    async def close(self) -> None:
        """No-op for ReturnSink."""

    def get_events(self) -> List[str]:
        """Return buffered events."""
```

---

### LockfileManager

```python
from lilux.primitives.lockfile import LockfileManager, Lockfile, LockfileRoot

class LockfileManager:
    def load(self, path: Path) -> Lockfile:
        """
        Load a lockfile from an explicit path.

        Args:
            path: Full path to lockfile (provided by orchestrator)

        Returns:
            Parsed Lockfile object

        Raises:
            FileNotFoundError: If lockfile doesn't exist
            LockfileError: If lockfile is malformed
        """

    def save(self, lockfile: Lockfile, path: Path) -> Path:
        """
        Save a lockfile to an explicit path.

        Args:
            lockfile: Lockfile object to save
            path: Full path where to save

        Returns:
            Path where lockfile was saved

        Raises:
            FileNotFoundError: If parent directory doesn't exist

        Note:
            Does NOT create parent directories.
        """

    def exists(self, path: Path) -> bool:
        """Check if lockfile exists at path."""
```

### Lockfile Data Structures

```python
@dataclass
class LockfileRoot:
    tool_id: str
    version: str
    integrity: str

@dataclass
class Lockfile:
    lockfile_version: int
    generated_at: str
    root: LockfileRoot
    resolved_chain: List[dict]
    registry: Optional[dict] = None
```

---

### Integrity Functions

```python
from lilux.primitives.integrity import (
    compute_tool_integrity,
    compute_directive_integrity,
    compute_knowledge_integrity,
)

def compute_tool_integrity(
    tool_id: str,
    version: str,
    manifest: Dict[str, Any],
    files: Optional[List[Dict[str, Any]]] = None
) -> str:
    """Compute SHA256 integrity hash for a tool. Returns 64-char hex digest."""

def compute_directive_integrity(
    directive_name: str,
    version: str,
    xml_content: str,
    metadata: Optional[Dict[str, Any]] = None
) -> str:
    """Compute SHA256 integrity hash for a directive. Returns 64-char hex digest."""

def compute_knowledge_integrity(
    id: str,
    version: str,
    content: str,
    metadata: Optional[Dict[str, Any]] = None
) -> str:
    """Compute SHA256 integrity hash for a knowledge entry. Returns 64-char hex digest."""
```

---

## Runtime Services

### AuthStore

```python
from lilux.runtime import AuthStore, AuthenticationRequired, RefreshError

class AuthStore:
    def __init__(self, service_name: str = "lilux"):
        """Initialize auth store with service name for keyring."""

    def set_token(
        self,
        service: str,
        access_token: str,
        refresh_token: Optional[str] = None,
        expires_in: int = 3600,
        scopes: Optional[List[str]] = None
    ) -> None:
        """Store token securely in OS keychain."""

    async def get_token(
        self,
        service: str,
        scope: Optional[str] = None
    ) -> str:
        """
        Retrieve token with automatic refresh on expiry.

        Raises:
            AuthenticationRequired: No valid token available
            RefreshError: Token refresh failed (not yet implemented)
        """

    def is_authenticated(self, service: str) -> bool:
        """Check if service has valid (non-expired) token."""

    def clear_token(self, service: str) -> None:
        """Remove token from keychain (logout)."""

class AuthenticationRequired(Exception):
    """Raised when authentication is required but token is missing/invalid."""

class RefreshError(Exception):
    """Raised when token refresh fails."""
```

---

### EnvResolver

```python
from lilux.runtime import EnvResolver

class EnvResolver:
    def __init__(self, project_path: Optional[Path] = None):
        """
        Initialize with project context.

        Args:
            project_path: Root directory for relative path resolution.
                         If None, uses current working directory.
        """

    def resolve(
        self,
        env_config: Optional[Dict[str, Any]] = None,
        tool_env: Optional[Dict[str, str]] = None,
        include_dotenv: bool = True
    ) -> Dict[str, str]:
        """
        Resolve environment from all sources.

        Args:
            env_config: Environment configuration with interpreter and env sections
            tool_env: Tool-level environment overrides (highest priority)
            include_dotenv: Whether to load .env files

        Returns:
            Complete environment dictionary

        Resolution order (lowest to highest priority):
            1. os.environ
            2. .env files (if include_dotenv=True)
            3. env_config["env"] static vars
            4. env_config["interpreter"] resolved paths
            5. tool_env overrides
        """
```

---

## Schemas

```python
from lilux.schemas import (
    SchemaValidator,
    SchemaExtractor,
    extract_tool_metadata,
    validate_tool_metadata,
    extract_and_validate,
)

class SchemaValidator:
    def __init__(self, schema: Optional[Dict] = None):
        """Initialize with optional validation schema."""

    def validate(self, data: Dict[str, Any]) -> Dict[str, Any]:
        """
        Validate data against schema.

        Returns:
            {
                "valid": bool,
                "issues": List[str],    # Validation errors
                "warnings": List[str]   # Non-fatal warnings
            }
        """

class SchemaExtractor:
    def extract(
        self,
        file_path: Path,
        item_type: str = "tool",
        project_path: Optional[Path] = None
    ) -> Dict[str, Any]:
        """Extract metadata from a file using schema-driven rules."""

# Convenience functions
def extract_tool_metadata(
    file_path: Path,
    item_type: str = "tool",
    project_path: Optional[Path] = None
) -> Dict[str, Any]:
    """Extract metadata from a tool file."""

def validate_tool_metadata(data: Dict[str, Any]) -> Dict[str, Any]:
    """Validate extracted metadata."""

def extract_and_validate(
    file_path: Path,
    item_type: str = "tool",
    project_path: Optional[Path] = None
) -> Dict[str, Any]:
    """
    Extract and validate in one call.

    Returns:
        {
            "data": Dict,           # Extracted metadata
            "valid": bool,
            "issues": List[str],
            "warnings": List[str]
        }
    """
```

---

## Related Documentation

- **Primitives:** [[lilux/primitives/overview]]
- **Runtime Services:** [[lilux/runtime-services/overview]]
- **Schemas:** [[lilux/schemas/overview]]
- **Error Handling:** [[lilux/primitives/overview#error-handling-pattern]]
