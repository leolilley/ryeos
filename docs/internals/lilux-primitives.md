```yaml
id: lilux-primitives
title: "Lilux Primitives"
description: The microkernel layer — subprocess, HTTP, signing, and env resolution
category: internals
tags: [lilux, primitives, microkernel, subprocess, http]
version: "1.0.0"
```

# Lilux Primitives

Lilux is the microkernel layer of Rye OS. It provides stateless, async-first primitives for interacting with the operating system. Lilux has **no knowledge** of Rye, `.ai/` directories, tool metadata, or space resolution — it receives fully-resolved configuration and executes it.

All Lilux code lives in `lilux/kernel/`.

## SubprocessPrimitive

**Location:** `lilux/primitives/subprocess.py`

Unified process management primitive. All process operations — inline execution, detached spawning, killing, and status checks — go through the `lilux-proc` Rust binary. No POSIX fallbacks.

### Hard Dependency: lilux-proc

`SubprocessPrimitive.__init__()` resolves `lilux-proc` via `shutil.which()`. If the binary is not found on `$PATH`, it raises `ConfigurationError`. This is intentional — lilux-proc is a hard requirement for all process operations.

```python
class SubprocessPrimitive:
    def __init__(self):
        self._lilux_proc = shutil.which("lilux-proc")
        if not self._lilux_proc:
            raise ConfigurationError("lilux-proc binary not found on PATH.")
```

### Interface

```python
class SubprocessPrimitive:
    async def execute(self, config: Dict, params: Dict) -> SubprocessResult
    async def spawn(self, cmd, args, log_path=None, envs=None) -> SpawnResult
    async def kill(self, pid, grace=3.0) -> KillResult
    async def status(self, pid) -> StatusResult
```

### execute() — Inline Run-and-Wait

Delegates to `lilux-proc exec`. Two-stage templating (env vars then runtime params) stays in Python; the actual process execution is handled by lilux-proc.

**Config keys:**

| Key          | Type      | Default  | Description           |
| ------------ | --------- | -------- | --------------------- |
| `command`    | str       | required | Command to execute    |
| `args`       | list[str] | `[]`     | Command arguments     |
| `cwd`        | str       | None     | Working directory     |
| `input_data` | str       | None     | Data piped to stdin   |
| `env`        | dict      | `{}`     | Environment variables |
| `timeout`    | int       | `300`    | Timeout in seconds    |

### spawn() — Detached Process

Delegates to `lilux-proc spawn`. Returns immediately with a PID.

```python
result = await primitive.spawn("python", ["worker.py"], log_path="/tmp/worker.log")
# SpawnResult(success=True, pid=12345)
```

### kill() — Graceful Then Force

Delegates to `lilux-proc kill`. Sends SIGTERM, waits `grace` seconds, then SIGKILL.

```python
result = await primitive.kill(12345, grace=3.0)
# KillResult(success=True, pid=12345, method="terminated")
```

### status() — Is Process Alive

Delegates to `lilux-proc status`.

```python
result = await primitive.status(12345)
# StatusResult(pid=12345, alive=True)
```

### Two-Stage Templating

1. **Stage 1 — Environment expansion:** `${VAR:-default}` patterns are replaced with environment values
2. **Stage 2 — Parameter substitution:** `{param_name}` patterns are replaced with runtime parameter values

Both stages run on `command`, `args`, `cwd`, and `input_data` within `execute()`.

### Environment Merge Heuristic

```python
if len(config_env) < 50:
    # Merge config env over os.environ (partially resolved)
    result = os.environ.copy()
    result.update(config_env)
else:
    # Use config env directly (assumed fully resolved by orchestrator)
    result = config_env
```

### Result Types

```python
@dataclass
class SubprocessResult:
    success: bool       # True if return code is 0
    stdout: str         # Standard output
    stderr: str         # Standard error
    return_code: int    # Exit code
    duration_ms: float  # Execution time

@dataclass
class SpawnResult:
    success: bool
    pid: int | None = None
    error: str | None = None

@dataclass
class KillResult:
    success: bool
    pid: int = 0
    method: str = ""       # "terminated" | "killed" | "already_dead"
    error: str | None = None

@dataclass
class StatusResult:
    pid: int
    alive: bool
```

### Timeout Handling

lilux-proc handles timeouts natively. On timeout, the child process is killed and a `SubprocessResult` is returned with `success=False` and stderr `"Command timed out after {timeout} seconds"`.

### Error Cases

| Condition            | Return Code | Stderr                                        |
| -------------------- | ----------- | --------------------------------------------- |
| lilux-proc not on PATH | N/A         | `ConfigurationError` raised at `__init__`     |
| Command not found    | -1          | `"Failed to spawn: {error}"`                 |
| Timeout              | -1          | `"Command timed out after {timeout} seconds"` |
| No command specified | -1          | `"No command specified"`                      |

## HttpClientPrimitive

**Location:** `lilux/primitives/http_client.py`

Makes HTTP requests with retry logic, authentication, and SSE streaming support. Uses `httpx` for async HTTP with connection pooling.

### Interface

```python
class HttpClientPrimitive:
    async def execute(self, config: Dict, params: Dict) -> HttpResult
```

### Modes

**Sync mode** (`mode: "sync"`, default):

Standard request/response. Config keys:

| Key       | Type | Default  | Description                                                   |
| --------- | ---- | -------- | ------------------------------------------------------------- |
| `method`  | str  | `"GET"`  | HTTP method                                                   |
| `url`     | str  | required | Request URL (supports `{param}` templating)                   |
| `headers` | dict | `{}`     | Request headers                                               |
| `body`    | any  | None     | Request body (JSON-serialized for POST/PUT/PATCH)             |
| `timeout` | int  | `30`     | Request timeout in seconds                                    |
| `retry`   | dict | `{}`     | Retry config: `max_attempts`, `backoff` (exponential/linear)  |
| `auth`    | dict | `{}`     | Auth config: `type` (bearer/api_key), `token`/`key`, `header` |

**Stream mode** (`mode: "stream"`):

SSE streaming with destination fan-out. Reads `data:` lines from the response and dispatches to sink objects. Supports `ReturnSink` for buffering events into the result.

### Authentication

```yaml
# Bearer token:
auth:
  type: bearer
  token: "${API_KEY}"

# API key header:
auth:
  type: api_key
  key: "${API_KEY}"
  header: X-API-Key  # default
```

Environment variables in auth values are resolved via `${VAR:-default}` syntax.

### Retry Logic

```yaml
retry:
  max_attempts: 3
  backoff: exponential # 1s, 2s, 4s...
```

Retries on `TimeoutException`, `ConnectError`, and `RequestError`. Exponential backoff uses `2^attempt` seconds.

### Result

```python
@dataclass
class HttpResult:
    success: bool          # True if 200 <= status < 400
    status_code: int
    body: Any              # Parsed JSON or raw text
    headers: Dict[str, str]
    duration_ms: int
    error: Optional[str]   # None on success, "HTTP {code}: {reason}" on failure
    stream_events_count: Optional[int]     # For stream mode
    stream_destinations: Optional[List[str]]  # Sink class names
```

### Connection Pooling

The HTTP client is lazily initialized and reused:

```python
httpx.AsyncClient(
    limits=httpx.Limits(max_keepalive_connections=10, max_connections=20),
    timeout=httpx.Timeout(30.0),
)
```

## Signing Primitives

**Location:** `lilux/primitives/signing.py`

Pure Ed25519 cryptographic operations. No policy, no I/O beyond key material.

### Functions

| Function                                                  | Purpose                                                     |
| --------------------------------------------------------- | ----------------------------------------------------------- |
| `generate_keypair()`                                      | Generate new Ed25519 key pair → `(private_pem, public_pem)` |
| `sign_hash(content_hash, private_key_pem)`                | Sign a SHA256 hex digest → base64url-encoded signature      |
| `verify_signature(content_hash, sig_b64, public_key_pem)` | Verify signature → `True`/`False`                           |
| `compute_key_fingerprint(public_key_pem)`                 | SHA256 of public key PEM → first 16 hex chars               |
| `save_keypair(private_pem, public_pem, key_dir)`          | Save to disk with restricted permissions                    |
| `load_keypair(key_dir)`                                   | Load from `private_key.pem` and `public_key.pem`            |
| `ensure_keypair(key_dir)`                                 | Load or generate if missing                                 |

### Key Storage

```
{key_dir}/
  private_key.pem  (mode 0600 — owner read/write only)
  public_key.pem   (mode 0644 — world readable)
```

The key directory itself is set to mode `0700`.

### Implementation

Uses the `cryptography` library's `Ed25519PrivateKey` and `Ed25519PublicKey`. Private keys are stored in PKCS8 PEM format without encryption. Signatures are base64url-encoded (URL-safe base64 without padding issues).

## Integrity Hashing

**Location:** `lilux/primitives/integrity.py`

Deterministic SHA256 hashing with canonical JSON serialization for arbitrary data. Lilux is type-agnostic — callers (e.g., rye) structure the data dict for their item types.

### Functions

| Function                  | Purpose                                                           |
| ------------------------- | ----------------------------------------------------------------- |
| `canonical_json(data)`    | Serialize any data to canonical JSON (sorted keys, no whitespace) |
| `compute_integrity(data)` | SHA256 hex digest of canonical JSON of any dict                   |

### Canonical JSON

All hashing uses canonical JSON serialization to ensure deterministic output:

```python
json.dumps(data, sort_keys=True, separators=(",", ":"), ensure_ascii=True)
```

This guarantees the same input always produces the same hash, regardless of dict ordering or whitespace in the original data.

## Lockfile I/O

**Location:** `lilux/primitives/lockfile.py`

Pure lockfile I/O with explicit paths. No path resolution, no creation logic — that's handled by Rye's `LockfileResolver`.

### Data Structures

```python
@dataclass
class LockfileRoot:
    tool_id: str       # Tool identifier
    version: str       # Semver
    integrity: str     # SHA256 hash

@dataclass
class Lockfile:
    lockfile_version: int            # Format version (currently 1)
    generated_at: str                # ISO timestamp
    root: LockfileRoot               # Root tool metadata
    resolved_chain: List[Any]        # Chain element dicts with integrity hashes
    registry: Optional[Dict]         # Optional registry metadata
    verified_deps: Optional[Dict]    # Optional dependency verification hashes
```

### LockfileManager

```python
class LockfileManager:
    def load(self, path: Path) -> Lockfile    # Load and validate JSON structure
    def save(self, lockfile: Lockfile, path: Path) -> Path  # Save as indented JSON
    def exists(self, path: Path) -> bool      # Check existence
```

The manager validates required fields on load (`lockfile_version`, `generated_at`, `root`, `resolved_chain`) and raises `LockfileError` for invalid structure.

## EnvResolver

**Location:** `lilux/runtime/env_resolver.py`

Resolves environment variables from multiple sources. Pure resolver with no side effects — it doesn't create venvs or install packages.

### Resolution Order

1. **System environment** — Start with `os.environ.copy()`
2. **`.env` files** — Load from project root (simple `KEY=value` parsing, skip comments and `export` lines)
3. **ENV_CONFIG rules** — Apply interpreter resolution and static variables
4. **Tool-level overrides** — Highest priority, direct key-value merge

### Interpreter Resolution

The resolver supports four interpreter discovery strategies:

| Type              | Config Key           | Search Method                                                             |
| ----------------- | -------------------- | ------------------------------------------------------------------------- |
| `venv_python`     | `venv_path`          | Check `.venv/bin/python`, `.venv/Scripts/python.exe`, `.venv/bin/python3` |
| `node_modules`    | `search_paths`       | Check `node_modules/.bin/node`                                            |
| `system_binary`   | `binary`             | Run `which`/`where` to find on PATH                                       |
| `version_manager` | `manager`, `version` | Query pyenv, nvm, rbenv, or asdf                                          |

Each strategy resolves to a path and sets the environment variable named by `var`. If resolution fails, the `fallback` value is used.

### Static Variable Expansion

Static variables in `env_config.env` support `${VAR:-default}` expansion:

```yaml
env_config:
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"
    MY_VAR: "${OTHER_VAR:-fallback_value}"
```

Variables are applied in order, so later variables can reference earlier ones.

## Auth

**Location:** `lilux/runtime/auth.py`

Authentication primitives for the runtime layer.

## SchemaValidator

**Location:** `lilux/schemas/schema_validator.py`

JSON Schema validation for tool configs, runtime parameters, and other structured data.

## Design Principle

Lilux is intentionally minimal. It provides OS-level capabilities that any orchestrator could use. The separation means:

- Lilux can be tested independently of Rye
- Lilux primitives can be reused in other projects
- Policy decisions (what to sign, when to verify, which spaces to check) live in Rye, not Lilux
- Lilux never imports from `rye.*` — the dependency is strictly one-way
