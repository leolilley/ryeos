# Lilux Package Structure

## Overview

The Lilux microkernel is organized into 3 core directories:

```
lilux/
├── primitives/          # Execution primitives (dumb executors)
├── runtime/             # Runtime services (auth, env, locks)
└── schemas/             # JSON Schema validation utilities
```

**Note:** Caching, handlers, discovery, extraction, and orchestration are handled by orchestrator systems (e.g., RYE).

## Directory Details

### 1. Primitives (`lilux/primitives/`)

**Purpose:** Core execution primitives that provide generic execution capabilities.

**Key Files:**

- **`subprocess.py`** - Execute shell commands in isolated environments
- **`http_client.py`** - Make HTTP requests with retry logic, authentication, and cert handling
- **`lockfile.py`** - Manage file-based locks for concurrency control
- **`integrity.py`** - Pure hash and crypto helper functions
- **`errors.py`** - Error types and result dataclasses

**Architecture Role:**
Primitives are the **dumb microkernel layer**. Each primitive:

- Executes exactly what it's told (no intelligence)
- Implements `execute()` method returning result objects
- Never parses metadata, resolves chains, or validates schemas

**What Lilux does NOT contain:**

- Chain validation (in orchestrator: `chain_validator.py`)
- Executor orchestration (in orchestrator: `primitive_executor.py`)
- Integrity verification with caching (in orchestrator: `integrity_verifier.py`)

### 2. Runtime Services (`lilux/runtime/`)

**Purpose:** Shared services that primitives depend on.

**Key Files:**

- **`auth_store.py`** - Keychain integration for credential management
- **`env_resolver.py`** - Template variable resolution (ENV_CONFIG handling)

**Architecture Role:**
Runtime services provide **infrastructure for safe execution**:

- `AuthStore`: Secure credential storage and retrieval
- `EnvResolver`: Variable templating for configuration

### 3. Schemas (`lilux/schemas/`)

**Purpose:** JSON Schema definitions for tool configuration and validation.

**Key Files:**

- **`tool_schema.py`** - JSON Schema for tools (parameters, inputs, outputs)

**Architecture Role:**
Schemas define the **shape of configuration data**:

- What parameters a tool accepts
- What types and constraints
- Used for validation before execution

## Design Principles

### 1. Dumb Microkernel

Each component in Lilux is intentionally simple:

- Subprocess just runs commands
- HTTP client just makes requests
- Schema validator just validates JSON against schemas
- **No intelligence, no orchestration**

### 2. Clear Separation

- Lilux = execution primitives (microkernel)
- Orchestrator systems = content understanding (OS layer)
- No overlap, no duplication

### 3. Library-First

Lilux is installable as a library:

```bash
pip install lilux
```

Not a standalone application—used by orchestrators and other systems.

### 4. Security-First

Security is handled through:

- Credentials stored securely (keychain)
- Locks prevent concurrent conflicts
- Integrity verification supported

## Dependencies

```
lilux/
├── Required: Python 3.10+
├── Required: httpx (for HTTP client)
├── Required: keyring (for keychain integration)
├── Required: python-dotenv (for .env file loading)
└── Standard library: asyncio, hashlib, json, re, os
```

## Logging

Lilux uses Python's standard `logging` module:

```python
import logging

# Configure Lilux logging level
logging.getLogger("lilux").setLevel(logging.DEBUG)

# Or silence Lilux logs
logging.getLogger("lilux").setLevel(logging.WARNING)
```

Log levels used:
- `DEBUG`: Detailed execution flow (useful for development)
- `INFO`: Token storage/retrieval, significant operations
- `WARNING`: Token refresh not implemented, fallback paths
- `ERROR`: Authentication failures, unexpected errors

## Testing Patterns

### For Orchestrator Development

Orchestrators should test with real primitives using test configurations:

```python
import pytest
from lilux.primitives import SubprocessPrimitive, HttpClientPrimitive
from lilux.runtime import EnvResolver, AuthStore

class ToolExecutor:
    """Example orchestrator with dependency injection."""
    def __init__(self, subprocess=None, http=None, auth=None):
        # Dependency injection for testability
        self.subprocess = subprocess or SubprocessPrimitive()
        self.http = http or HttpClientPrimitive()
        self.auth = auth or AuthStore()

    async def execute(self, tool_config, user_params):
        # ... orchestrator logic ...

# Test with real primitives
@pytest.mark.asyncio
async def test_orchestrator_with_real_primitives():
    executor = ToolExecutor()

    result = await executor.execute({
        "command": "echo",
        "args": ["test"]
    }, {})

    assert result.success

# Test with mocks for special cases
@pytest.mark.asyncio
async def test_orchestrator_with_mocked_http():
    executor = ToolExecutor(http=MockHttpClientPrimitive())

    # Test failure handling, rate limits, etc.
    ...
```

**Why Real Primitives?**
- Primitives are lightweight and fast (no need to mock)
- Catches real integration issues
- Simpler and more reliable than mocking
- Dependency injection allows mocking when needed for edge cases

---

## Cross-References

- **Primitives:** `[[lilux/primitives/overview]]`
- **Runtime Services:** `[[lilux/runtime-services/overview]]`
- **Schemas:** `[[lilux/schemas/overview]]`
