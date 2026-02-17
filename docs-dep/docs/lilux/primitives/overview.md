# Lilux Primitives Overview

## Purpose

The `primitives/` module contains core execution primitives that power AI agent systems. These primitives provide the **dumb execution foundation** that orchestrator systems build upon.

## Architecture Vision

Lilux follows a microkernel architecture - intentionally "dumb" execution:

```
┌─────────────────────────────────────────┐
│  Orchestrator Layer                     │
│  - Chain resolution                     │
│  - Config validation (before execution) │
│  - Validation & intelligence            │
└──────────────────┬──────────────────────┘
                   │
                   ▼
         ┌──────────────────────────────┐
         │  Lilux Primitives Layer      │
         │  - Dumb execution primitives │
         │  - Pure helper functions     │
         │  - Minimal input checking    │
         └──────────────────────────────┘
                   │
                   ▼
         ┌─────────────────────────────────┐
         │  System Resources (OS)          │
         │  - Shell, HTTP, Filesystem      │
         └─────────────────────────────────┘
```

**Key Separation of Concerns:**

- **Lilux Primitives:** Dumb execution - "Do exactly this thing"
- **Orchestrator:** Intelligence - "Figure out what to do, validate, resolve chains"

## Config Validation Responsibility

**Who validates the `config` dict passed to primitives?**

| Validation                                 | Responsibility   | When                     |
| ------------------------------------------ | ---------------- | ------------------------ |
| Schema validation (types, required fields) | **Orchestrator** | Before calling primitive |
| Tool config structure                      | **Orchestrator** | During chain resolution  |
| Minimal sanity checks                      | **Primitive**    | At execution time        |

**Primitives perform minimal checks only:**

```python
# SubprocessPrimitive only checks:
if not config.get("command"):
    raise ValueError("command is required in config")

# HttpClientPrimitive only checks:
if not config.get("url"):
    raise ValueError("url is required in config")
```

**The orchestrator is responsible for:**

- Validating config against `CONFIG_SCHEMA` before execution
- Ensuring all required fields are present and correctly typed
- Resolving template variables in config
- Raising `ConfigValidationError` for invalid configs

**Why this design?** Primitives are "dumb" - they trust the orchestrator to provide valid config. This keeps primitives simple and fast, with validation logic centralized in the orchestrator.

---

## Module Organization

### 1. Execution Primitives

Core primitives that directly execute operations on system resources.

| Module           | Primitive           | System Resource | Status         | Purpose                         |
| ---------------- | ------------------- | --------------- | -------------- | ------------------------------- |
| `subprocess.py`  | SubprocessPrimitive | Shell           | ✅ Implemented | Execute commands and scripts    |
| `http_client.py` | HttpClientPrimitive | HTTP            | ✅ Implemented | Make HTTP requests with retries |

### 2. Support Modules

Infrastructure that enables safe, reproducible execution.

| Module         | Purpose                    | Status         | Dependencies          |
| -------------- | -------------------------- | -------------- | --------------------- |
| `errors.py`    | Error types                | ✅ Implemented | -                     |
| `lockfile.py`  | LockfileManager            | ✅ Implemented | Hash helpers          |
| `integrity.py` | Pure hash/crypto functions | ✅ Implemented | hashlib, cryptography |
| `__init__.py`  | Module exports             | ✅ Implemented | -                     |

---

## Execution Primitives

### SubprocessPrimitive (`subprocess.py`)

**Purpose:** Execute shell commands and scripts in isolated environments.

**Capabilities:**

- Run shell commands (bash, python, node, etc.)
- Capture stdout/stderr
- Control working directory
- Environment variable injection
- Timeout protection
- Exit code handling

**Key Design Principles:**

- **Minimal abstraction** - Thin wrapper around asyncio subprocess
- **Isolated execution** - Each command in clean environment
- **Environment priority**: CONFIG > .env > os.environ
- **Two-stage templating**:
  - Environment variables: `${VAR:-default}` syntax
  - Runtime params: `{param_name}` syntax (from `params` dict)

**Example Usage:**

```python
from lilux.primitives.subprocess import SubprocessPrimitive

primitive = SubprocessPrimitive()

# Execute simple command
result = await primitive.execute(
    config={
        "command": "echo",
        "args": ["hello"],
        "capture_output": True,
        "timeout": 30
    },
    params={}
)

# With runtime params templating
result = await primitive.execute(
    config={
        "command": "python",
        "args": ["process.py", "--input", "{input_file}"]
    },
    params={"input_file": "data.csv"}  # Args become: ["process.py", "--input", "data.csv"]
)

# With environment variables
result = await primitive.execute(
    config={
        "command": "python",
        "args": ["-c", "import os; print(os.getenv('MY_VAR', 'default'))"],
        "env": {"MY_VAR": "custom_value"}
    },
    params={}
)
```

**Returns:** `SubprocessResult`

- `success: bool` - Execution succeeded
- `stdout: str` - Standard output
- `stderr: str` - Error output
- `return_code: int` - Process exit code
- `duration_ms: int` - Execution time

### HttpClientPrimitive (`http_client.py`)

**Purpose:** Make HTTP requests with intelligent retry logic and authentication.

**Capabilities:**

- GET, POST, PUT, DELETE, PATCH requests
- Request/response streaming (SSE, WebSocket)
- Authentication (API keys, OAuth, JWT)
- Automatic retry with exponential backoff
- Certificate handling
- Response timeout handling

**Key Design Principles:**

- **Resilience**: Automatic retries on transient failures
- **Flexibility**: Multiple auth methods (API key, OAuth, basic)
- **Streaming**: Support for long-running operations
- **Timeout**: Configurable request timeout

**Example Usage:**

```python
from lilux.primitives.http_client import HttpClientPrimitive, HttpResult

client = HttpClientPrimitive()

# Simple GET request
result = await client.execute({
    "url": "https://api.example.com/data",
    "method": "GET"
    "timeout": 30
})

# POST with JSON body
result = await client.execute({
    "url": "https://api.example.com/create",
    "method": "POST",
    "headers": {"Content-Type": "application/json"},
    "json": {"key": "value"},
    "timeout": 60
})

# With authentication
result = await client.execute({
    "url": "https://api.example.com/protected",
    "auth": {
        "type": "bearer",
        "token": "secret_token"
    },
    "timeout": 30
})

# Streaming request
result = await client.execute({
    "url": "https://api.example.com/stream",
    "method": "GET",
    "stream": True,  # Enable SSE streaming
    "timeout": 300
})
```

**Returns:** `HttpResult`

- `success: bool` - Request succeeded
- `status_code: int` - HTTP status code
- `body: dict|str` - Response data
- `duration_ms: int` - Request time

---

## Support Modules

### LockfileManager (`lockfile.py`)

**Purpose:** Manage concurrency and provide reproducible execution through file-based locks.

**Capabilities:**

- File-level locking (`.lock` files)
- Timeout handling (auto-expire stale locks)
- Multiple lock types (process, file, resource)
- Hierarchical lockfile structure

**Lock Types:**

- **Process Lock**: Prevent concurrent execution of same tool
- **File Lock**: Prevent concurrent modifications
- **Resource Lock**: Protect shared resources

**Key Design Principles:**

- **Process-level**: Lock before reading/writing
- **File-level**: Use OS file locking (fcntl, msvcrt)
- **Timeout-based**: Auto-expire locks after inactivity
- **Stale detection**: Detect and clean up expired locks

**Lockfile Structure:**

```json
{
  "tool_id": "my_tool",
  "version": "1.0.0",
  "files": [
    {
      "path": "src/main.py",
      "sha256": "abc123...",
      "size": 2048
    }
  ],
  "signature": "crypto-signature...",
  "locked_at": 1704102400,
  "expires_at": 1704188800
}
```

### Integrity Helpers (`integrity.py`)

**Purpose:** Provide pure cryptographic hash utilities for integrity verification.

**Capabilities:**

- Content hashing (SHA256)
- Signature verification
- Content signing (using cryptography library)
- Hash comparison utilities

**Key Functions:**

- `compute_file_hash(path: Path) -> str` - SHA256 hash
- `verify_signature(content: str, signature: str) -> bool` - Verify cryptographic signature
- `compute_integrity_hash(metadata: dict) -> str` - Hash for verification

**Example:**

### Error Types (`errors.py`)

**Purpose:** Provide consistent error types for all Lilux modules.

**Key Exceptions:**

- `ToolExecutionError` - Generic tool execution failure
- `IntegrityError` - Integrity verification failure
- `LockfileError` - Lockfile operation failure
- `ConfigurationError` - Configuration parsing/validation error

**Example:**

```python
from lilux.primitives.errors import ToolExecutionError

try:
    result = await primitive.execute(...)
except ToolExecutionError as e:
    logger.error(f"Tool execution failed: {e}")
    raise
```

---

## Error Propagation Strategy

Lilux primitives use a **consistent error propagation pattern** across all execution primitives. This ensures predictable error handling and easy debugging.

### Core Principle: Result Objects, Not Exceptions

**Key Design Decision:** Primitives return result objects instead of raising exceptions for execution failures.

**Why This Pattern:**

- Explicit error handling in orchestration layer
- Easy to chain multiple primitives without try/except
- Consistent interface across all primitives
- Allows orchestrator to decide error handling strategy
- Better error context and metadata

### Result Object Pattern

All primitive result objects share the same structure:

```python
@dataclass
class SubprocessResult:
    """Result from subprocess execution."""
    success: bool              # True if execution succeeded
    stdout: str               # Standard output (or empty on error)
    stderr: str               # Standard error output
    return_code: int          # Process exit code (0 on success)
    duration_ms: int          # Execution time in milliseconds
    error: Optional[str]       # Error message if success=False
    metadata: Optional[Dict]  # Additional context

@dataclass
class HttpResult:
    """Result from HTTP execution."""
    success: bool              # True if request succeeded
    status_code: int          # HTTP status code
    body: Any                 # Response body (or None on error)
    headers: Dict             # Response headers
    duration_ms: int          # Request time in milliseconds
    error: Optional[str]       # Error message if success=False
    metadata: Optional[Dict]  # Additional context (retry count, etc.)
```

### Error Handling Patterns

**Pattern 1: Primitive Success**

```python
result = await subprocess_primitive.execute(config, params)

if result.success:
    print(f"Output: {result.stdout}")
else:
    print(f"Failed: {result.error}")
    print(f"Exit code: {result.return_code}")
```

**Pattern 2: Chained Execution (handled by orchestrator)**

```python
# Orchestrator chains primitives
result1 = await subprocess.execute(config1, params1)
if not result1.success:
    return ExecutionResult(success=False, error=result1.error, ...)

result2 = await http_client.execute(config2, params2)
if not result2.success:
    return ExecutionResult(success=False, error=result2.error, ...)

# All succeeded
return ExecutionResult(success=True, data={...}, ...)
```

### Error Type Hierarchy

Lilux defines two categories of errors:

#### 1. Result Objects (Primitives)

Execution primitives return result objects with `success: bool`. They never raise exceptions for expected failures.

```
SubprocessResult / HttpResult
├── success: bool
├── error: Optional[str]     # Human-readable error message
├── return_code / status_code
└── duration_ms
```

#### 2. Exceptions (Runtime Services & Validation)

Runtime services and validation raise exceptions for precondition failures:

```
Exception
├── AuthenticationRequired     # No valid token in keychain
├── RefreshError               # Token refresh failed
├── ConfigValidationError      # Tool config validation failed
│   ├── tool_id: str
│   └── errors: List[ValidationError]
│
└── ToolChainError             # Execution chain failed (raised by orchestrator)
    ├── code: str              # Error code (e.g., "TOOL_CHAIN_FAILED")
    ├── message: str           # Human-readable message
    ├── chain: List[str]       # Tool chain path
    ├── failed_at: FailedToolContext
    │   ├── tool_id: str
    │   ├── config_path: str
    │   └── validation_errors: List[ValidationError]
    └── cause: Optional[Exception]
```

#### ValidationError (dataclass)

```python
@dataclass
class ValidationError:
    field: str              # Field name that failed validation
    error: str              # Error message
    value: Optional[Any]    # The invalid value (for debugging)
```

#### When Each Is Used

| Scenario             | Error Type                        | Example                           |
| -------------------- | --------------------------------- | --------------------------------- |
| Command not found    | `SubprocessResult(success=False)` | `stderr="Command not found: foo"` |
| HTTP 404             | `HttpResult(success=False)`       | `status_code=404`                 |
| Timeout              | `SubprocessResult(success=False)` | `stderr="Command timed out"`      |
| No token in keychain | `raise AuthenticationRequired`    | User must authenticate            |
| Invalid tool config  | `raise ConfigValidationError`     | Missing required field            |
| Tool chain failure   | `raise ToolChainError`            | Chain resolution failed           |

### Error Metadata

Primitives include rich metadata in errors for debugging:

| Metadata Field     | Description                 | Example             |
| ------------------ | --------------------------- | ------------------- |
| `return_code`      | Process exit code           | `1`, `127`, `255`   |
| `status_code`      | HTTP status code            | `404`, `500`, `503` |
| `timeout_exceeded` | Whether timeout was hit     | `True`              |
| `retry_count`      | Number of retries attempted | `3`                 |
| `duration_ms`      | Time before failure         | `5000`              |

### Benefits of This Pattern

1. **Explicit Error Handling** - Orchestrator always checks `success` field
2. **No Uncaught Exceptions** - Primitives never raise for expected failures
3. **Rich Error Context** - Metadata aids debugging
4. **Consistent Interface** - Same pattern across all primitives
5. **Composable** - Easy to chain multiple primitives
6. **Testable** - Mock results easily in tests

### When to Use Exceptions

Primitives raise exceptions only for **unexpected failures**:

```python
# Expected failure → return result with success=False
if return_code != 0:
    return SubprocessResult(success=False, error="Command failed", ...)

# Unexpected failure → raise exception
if not isinstance(config, dict):
    raise TypeError(f"Config must be dict, got {type(config)}")
```

---

## Error Handling Pattern

Lilux uses two different error handling patterns across the system:

### Primitives (Execution Layer) - Result Objects

**Execution primitives** (`SubprocessPrimitive`, `HttpClientPrimitive`) return result objects with a `success` field:

```python
result = await subprocess.execute(config, params)

if result.success:
    print(f"Output: {result.stdout}")
else:
    print(f"Failed: {result.error}")
```

**Why result objects?**

- Explicit error handling in orchestration layer
- Easy to chain multiple primitives without try/except
- Consistent interface across all primitives
- Allows orchestrator to decide error handling strategy
- Better error context and metadata

**Primitives never raise exceptions for expected failures** (command not found, HTTP error, timeout). They return `success=False` with detailed error information.

### Runtime Services (Infrastructure Layer) - Exceptions

**Runtime services** (`AuthStore`, `LockfileManager`, `EnvResolver`) may raise exceptions for precondition failures:

```python
# AuthStore raises for missing tokens
try:
    token = await auth.get_token("service")
except AuthenticationRequired:
    print("No token stored - user must authenticate first")

# LockfileManager raises for missing files
try:
    lockfile = lockfile_manager.load(path)
except FileNotFoundError:
    print("Lockfile doesn't exist - will create new one")
```

**Why exceptions for runtime services?**

- Follows Python conventions for resource access
- Precondition failures (no token, no file) are not execution failures
- Clear separation: execution = result objects, infrastructure = exceptions

---

## Instantiation and Thread Safety

### Primitive Instantiation

Primitives are lightweight and can be instantiated per-request or reused:

```python
# Option 1: Create once, reuse (recommended for HttpClientPrimitive)
http = HttpClientPrimitive()  # Has connection pooling
for request in requests:
    result = await http.execute(config, params)
await http.close()  # Clean up connection pool

# Option 2: Create per-request (fine for SubprocessPrimitive)
for task in tasks:
    subprocess = SubprocessPrimitive()
    result = await subprocess.execute(config, {})
```

### Thread Safety

- **SubprocessPrimitive**: Stateless, thread-safe. Create new instances or share freely.
- **HttpClientPrimitive**: Uses internal connection pool (`httpx.AsyncClient`). Safe to share within the same async event loop. Call `close()` when done.
- **LockfileManager**: Stateless, thread-safe.
- **AuthStore**: Uses in-memory cache + OS keychain. Safe to share; keyring operations are thread-safe.
- **EnvResolver**: Stateless, thread-safe.

**Best practice:** Create one instance per service and reuse within your application.

## Design Principles

### 1. Modularity

Each primitive is self-contained with minimal dependencies:

| Module           | Dependencies             | Rationale             |
| ---------------- | ------------------------ | --------------------- |
| `subprocess.py`  | asyncio, os, dataclasses | Standard library only |
| `http_client.py` | asyncio, httpx, json     | Minimal HTTP client   |
| `lockfile.py`    | os, json                 | File locking + JSON   |
| `integrity.py`   | hashlib, cryptography    | Pure crypto functions |
| `errors.py`      | dataclasses              | Type safety           |

### 2. Type Safety

All modules use Python's type system:

- `dataclass` for result objects
- `Dict`, `List`, `Optional`, `Any` for parameters
- Type hints on all public methods
- Strict validation of input types

### 3. Consistency

Uniform patterns across all primitives:

| Pattern           | Implementation                                                                      |
| ----------------- | ----------------------------------------------------------------------------------- |
| Result objects    | All `*Result` dataclasses with same fields (success, data, duration_ms, error)      |
| Error propagation | Consistent result-object pattern (never throw for expected failures)                |
| Async interfaces  | All primitives are async (async def execute)                                        |
| Error types       | Hierarchical error types (ToolExecutionError → ValidationError, TimeoutError, etc.) |
| Configuration     | All use dict-based config objects                                                   |

**See Error Propagation section** for detailed error handling patterns.

### 4. Testing Support

All primitives designed for easy testing:

| Feature               | Implementation                                  |
| --------------------- | ----------------------------------------------- |
| Dry run mode          | Pass `check=True` to validate without execution |
| Capturable timeout    | Easy to set different timeouts for tests        |
| Environment isolation | Use `env=` to control test environment          |
| Stdio capture         | Capture output for debugging                    |

---

## Future Expansion Areas

These areas represent opportunities for extending Lilux capabilities:

1. **Additional Execution Primitives**
   - **File Operations**: Copy, move, delete, stat, read
   - **Database Operations**: Query, transaction support
   - **SSH Operations**: Remote command execution
   - **Cloud Services**: AWS, GCP, Azure integration
   - **Streaming**: WebSocket client, gRPC
   - **Message Queue**: Redis, RabbitMQ integration

2. **Enhanced Support Modules**
   - **Metrics Collection**: Prometheus export, custom metrics
   - **Distributed Locking**: Redis-based cluster locks
   - **Circuit Breakers**: Request rate limiting

3. **Security Enhancements**
   - **Sandbox**: Namespace isolation, seccomp
   - **Audit Logging**: Detailed execution logs
   - **Secrets Rotation**: Automatic credential rotation

4. **Observability**
   - **Distributed Tracing**: OpenTelemetry, Jaeger
   - **Structured Logging**: JSON logs with correlation IDs
   - **Performance Monitoring**: Request latency histograms
   - **Health Checks**: Kubernetes liveness/readiness probes

---

## Related Documentation

- **[[subprocess]]** - Detailed subprocess primitive documentation
- **[[http-client]]** - Detailed HTTP client documentation
- **[[lockfile]]** - Lockfile management documentation
- **[[integrity]]** - Integrity helpers documentation

---

## Implementation Status

| Component           | Status         | Notes                      |
| ------------------- | -------------- | -------------------------- |
| SubprocessPrimitive | ✅ Implemented | Core shell execution       |
| HttpClientPrimitive | ✅ Implemented | HTTP with retries and auth |
| LockfileManager     | ✅ Implemented | Concurrency control        |
| Integrity Helpers   | ✅ Implemented | Pure hash/crypto utilities |
| Error Types         | ✅ Implemented | Exception definitions      |
| File Operations     | ❌ Not started | File system primitives     |
| SSH Operations      | ❌ Not started | Remote command execution   |
| Cloud Services      | ❌ Not started | Cloud provider integration |

---

## Summary

Lilux primitives provide a **dumb execution microkernel** for AI agent systems through:

1. **Two core execution primitives** (subprocess, HTTP)
2. **Pure helper functions** (integrity hashing)
3. **Concurrency control** (lockfile management)
4. **Consistent error types**
5. **Type-safe, modular, testable** design

Lilux is intentionally "dumb" - it executes exactly what it's told. All intelligence (chain resolution, validation, metadata parsing, orchestration) belongs in the orchestrator.

This architecture enables the orchestrator to focus on **what to execute** (intelligence) while Lilux handles **how to execute** (dumb but reliable).
