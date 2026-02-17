# Runtime Services Overview

## Purpose

Runtime services provide shared infrastructure that execution primitives depend on. These services handle credential management and environment variable resolution—the supporting infrastructure needed for safe execution.

## Architecture Vision

Runtime services sit between primitives and system resources:

```
┌─────────────────────────────────────────┐
│  Execution Primitives Layer             │
│  - SubprocessPrimitive                  │
│  - HttpClientPrimitive                  │
└──────────────────┬──────────────────────┘
                   │
                   ▼
          ┌──────────────────────────────┐
          │  Runtime Services Layer      │
          │  - AuthStore                 │
          │  - EnvResolver               │
          └──────────────────────────────┘
                   │
                   ▼
          ┌─────────────────────────────────┐
          │  System Resources (OS)          │
          │  - Keychains, ENV vars          │
          └─────────────────────────────────┘
```

**Note:** Lockfile I/O is in `primitives/lockfile.py`. Lockfile validation and creation logic belongs in the orchestrator.

## Core Services

### 1. AuthStore (`auth-store.md`)

**Purpose:** Secure credential management using OS keychain integration.

**Responsibilities:**
- Store authentication tokens securely
- Retrieve tokens with automatic refresh on expiry
- Support multiple authentication methods (bearer, API key, OAuth2, basic)
- Integrate with OS-level encryption (macOS Keychain, Windows Credential Manager, Linux Secret Service)

**Key Classes:**
- `AuthStore` - Main credential management service
- `AuthenticationRequired` - Exception for missing tokens
- `RefreshError` - Exception for refresh failures

**Example:**
```python
from lilux.runtime import AuthStore

auth = AuthStore()

# Store token
auth.set_token(
    service="registry",
    access_token="eyJhbGc...",
    expires_in=3600
)

# Retrieve with auto-refresh
token = await auth.get_token("registry")
```

---

### 2. EnvResolver (`env-resolver.md`)

**Purpose:** Resolve environment variables from multiple sources using templating and interpreter discovery.

**Responsibilities:**
- Load environment from `os.environ`, .env files, and configuration
- Apply static environment variables
- Resolve interpreter paths (Python, Node.js, Ruby, etc.)
- Expand template variables with `${VAR}` syntax
- Apply tool-level overrides

**Key Classes:**
- `EnvResolver` - Main environment resolution service

**Resolver Types:**
- `venv_python` - Find Python in virtual environments
- `node_modules` - Find Node.js in node_modules
- `system_binary` - Find any binary in system PATH
- `version_manager` - Find via version managers (pyenv, nvm, rbenv, asdf)

**Example:**
```python
from lilux.runtime import EnvResolver

resolver = EnvResolver(project_path="/home/user/project")

env_config = {
    "interpreter": {
        "type": "venv_python",
        "var": "PYTHON_PATH",
        "venv_path": ".venv",
        "fallback": "/usr/bin/python3"
    },
    "env": {
        "DEBUG": "1",
        "PROJECT_HOME": "/home/user/project"
    }
}

env = resolver.resolve(env_config=env_config)
# env["PYTHON_PATH"] = "/home/user/project/.venv/bin/python"
# env["DEBUG"] = "1"
```

---

## Service Interaction

Runtime services work together to enable safe execution:

```
SubprocessPrimitive
├─ Uses AuthStore → Get credentials for authenticated commands
└─ Uses EnvResolver → Build execution environment

HttpClientPrimitive
└─ Uses AuthStore → Get bearer tokens, API keys
```

**Note:** Lockfile I/O is handled by `primitives/lockfile.py` (LockfileManager). Lockfile validation and creation are orchestrator responsibilities.

---

## Design Principles

### 1. Separation of Concerns

Each service handles one responsibility:
- **AuthStore** - Credential security only
- **EnvResolver** - Variable resolution only

### 2. Pure Services

Services have no side effects:
- No file creation (unless explicitly requested)
- No authentication flows (store/retrieve only)
- No environment modification (resolution only)

### 3. Explicit Control

Orchestrators explicitly pass all configuration:
- No implicit directory discovery
- No automatic file lookups
- No environment variable assumptions

### 4. Security-First

Security is built in:
- Credentials encrypted at rest (OS keychain)
- Locks prevent concurrent conflicts
- Integrity verification supported

---

## Usage Pattern

### For Subprocess Execution

```python
from lilux.primitives import SubprocessPrimitive
from lilux.primitives.lockfile import LockfileManager
from lilux.runtime import AuthStore, EnvResolver

# 1. Initialize services
auth = AuthStore()
env_resolver = EnvResolver(project_path="/project")
lockfile_manager = LockfileManager()  # I/O only

# 2. Get credentials if needed
if tool_needs_auth:
    token = await auth.get_token("service_name")

# 3. Build environment
env = env_resolver.resolve(env_config=tool.env_config)

# 4. Load lockfile (orchestrator handles validation)
lockfile = lockfile_manager.load(lockfile_path)

# 5. Execute
subprocess = SubprocessPrimitive()
result = await subprocess.execute(
    config={"command": "python", "env": env},
    params={}
)
```

---

## Limitations and Design

### By Design (Not a Bug)

1. **AuthStore requires OS keychain**
   - Relies on OS-level encryption
   - Not available in some containers
   - Use environment variables as fallback

2. **EnvResolver is pure resolver**
   - Doesn't create virtual environments
   - Doesn't install packages
   - Only finds existing interpreters



---

## Integration Points

### With Primitives

- **SubprocessPrimitive** uses AuthStore and EnvResolver
- **HttpClientPrimitive** uses AuthStore for authentication

### With Orchestrators

Orchestrators determine:
- When to refresh tokens
- How to resolve environment variables
- Where to store lockfiles
- When to validate lockfiles

---

## Error Handling Strategy

### Why Different Error Models?

Lilux uses two different error handling patterns:

| Layer | Error Model | Rationale |
|--------|-------------|------------|
| **Primitives** | Result objects | Execution failures are expected outcomes, not exceptions |
| **Runtime Services** | Exceptions | Infrastructure failures are exceptional conditions |

### Execution Layer (Primitives) - Result Objects

**Rationale:** Primitives execute external operations (shell commands, HTTP requests). These operations can legitimately fail for expected reasons:

| Failure Type | Example | Why Result Object? |
|--------------|---------|---------------------|
| **Command not found** | `nonexistent_cmd` | Legitimate execution outcome |
| **Permission denied** | `/root/script` | Legitimate execution outcome |
| **HTTP 404** | API endpoint doesn't exist | Legitimate execution outcome |
| **HTTP 500** | Server error | Legitimate execution outcome |
| **Timeout** | Operation took too long | Legitimate execution outcome |

**Pattern:**
```python
result = await primitive.execute(config, params)

if result.success:
    # Success path
    handle_success(result)
else:
    # Failure path (normal control flow)
    handle_failure(result.error, result.return_code, ...)
```

**Never:** Try/catch for expected failures.

### Infrastructure Layer (Runtime Services) - Exceptions

**Rationale:** Runtime services manage infrastructure resources (keychain, files, environment). These failures indicate infrastructure problems, not execution outcomes:

| Failure Type | Example | Why Exception? |
|--------------|---------|-----------------|
| **Token not stored** | User hasn't authenticated | Infrastructure precondition |
| **Lockfile missing** | File doesn't exist | Infrastructure precondition |
| **Keychain locked** | OS keychain unavailable | Infrastructure precondition |
| **Configuration invalid** | YAML parse error | Infrastructure error |

**Pattern:**
```python
try:
    token = await auth.get_token("service")
except AuthenticationRequired:
    # Infrastructure precondition
    prompt_user_to_authenticate()
```

**Always:** Try/catch for runtime service failures.

### Edge Cases and Clarifications

#### 1. What if primitive throws exception?

**Answer:** Primitives should only throw for unexpected errors (type errors, internal bugs). For expected failures, they return `success=False`.

```python
# In SubprocessPrimitive
async def execute(self, config, params):
    # Validate input types (unexpected if wrong)
    if not isinstance(config, dict):
        raise TypeError(f"Config must be dict, got {type(config)}")

    # Execute command
    process = await run_subprocess(config["command"])

    # Expected failure: return result object
    if process.returncode != 0:
        return SubprocessResult(
            success=False,
            stderr=process.stderr,
            returncode=process.returncode,
            error=f"Command failed with code {process.returncode}"
        )
```

#### 2. What if runtime service throws exception?

**Answer:** Orchestrator catches and decides what to do.

```python
# In orchestrator
try:
    env = env_resolver.resolve(env_config)
except ConfigurationError as e:
    # Infrastructure error - abort execution
    logger.error(f"Configuration error: {e}")
    return ExecutionResult(success=False, error=str(e))
```

#### 3. Can result object contain exception info?

**Answer:** No, result objects contain error strings. Exceptions are separate.

```python
# Result object structure
@dataclass
class SubprocessResult:
    success: bool
    stdout: str
    stderr: str
    returncode: int
    error: Optional[str]  # String, not exception
    metadata: Optional[Dict]
```

#### 4. When to raise vs return?

| Situation | Approach | Example |
|-----------|-----------|---------|
| **Expected execution outcome** | Return result with success=False | Command returns exit code 1 |
| **Unexpected internal error** | Raise exception | Config is not a dict |
| **Infrastructure precondition** | Raise exception | Lockfile doesn't exist |
| **User input validation** | Return result or raise? | **Return result with success=False** (preferred) |

**Principle:** Use exceptions for "this should never happen" cases, not for "this might fail" cases.

---

## Limitations and Design

### By Design (Not a Bug)

1. **AuthStore requires OS keychain**
   - Relies on OS-level encryption
   - Not available in some containers
   - Use environment variables as fallback

2. **EnvResolver is pure resolver**
   - Doesn't create virtual environments
   - Doesn't install packages
   - Only finds existing interpreters

---

## Related Documentation

- **AuthStore:** `[[lilux/runtime-services/auth-store]]`
- **EnvResolver:** `[[lilux/runtime-services/env-resolver]]`
- **Lockfile Primitive:** `[[lilux/primitives/lockfile]]`
- **Primitives:** `[[lilux/primitives/overview]]`
- **Integrity Helpers:** `[[lilux/primitives/integrity]]`
