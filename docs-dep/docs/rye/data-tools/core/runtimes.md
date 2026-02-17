**Source:** Original implementation: `.ai/tools/rye/core/runtimes/` in kiwi-mcp

# Runtimes Category

## Purpose

Runtimes are **language-specific executors** - Layer 2 of RYE's three-layer architecture. They add environment configuration on top of primitives.

**Location:** `.ai/tools/rye/core/runtimes/`  
**Protected:** ✅ Yes (core tool - cannot be shadowed)

**Key:** `__executor_id__` points to a primitive (e.g., "subprocess", "http_client"), and they declare `ENV_CONFIG`

## Architecture

```
Runtimes (Layer 2)
├─ Environment configuration
├─ Delegate to primitives (Layer 1)
├─ Declare ENV_CONFIG for resolution
└─ All have __executor_id__ pointing to primitives
```

## Core Runtimes

### 1. Python Runtime (`python_runtime.py`)

**Purpose:** Execute Python scripts with environment resolution

```python
__version__ = "2.0.0"
__tool_type__ = "runtime"
__executor_id__ = "subprocess"  # ← Delegates to subprocess primitive
__category__ = "runtimes"

# Declare environment needs
ENV_CONFIG = {
    "interpreter": {
        "type": "venv_python",
        "search": ["project", "user", "system"],
        "var": "RYE_PYTHON",
        "fallback": "python3",
    },
    "env": {
        "PYTHONUNBUFFERED": "1",
        "PROJECT_VENV_PYTHON": "${RYE_PYTHON}",  # ← Template variable
    },
}

# Base command configuration
CONFIG = {
    "command": "${RYE_PYTHON}",  # ← Resolved at runtime
    "args": [],
    "timeout": 300,
}
```

**Implementation:** Pure schema (no code in Lilux - subprocess primitive handles it)

**Features:**
- Finds Python interpreter (project venv → user venv → system)
- Sets `PYTHONUNBUFFERED` for real-time output
- Provides `RYE_PYTHON` template variable for tools
- Supports virtual environments

### 2. Node.js Runtime (`node_runtime.py`)

**Purpose:** Execute Node.js scripts with environment resolution

```python
__version__ = "1.0.0"
__tool_type__ = "runtime"
__executor_id__ = "subprocess"  # ← Delegates to subprocess primitive
__category__ = "runtimes"

ENV_CONFIG = {
    "interpreter": {
        "type": "venv_node",
        "search": ["project", "user", "system"],
        "var": "RYE_NODE",
        "fallback": "node",
    },
    "env": {
        "NODE_ENV": "production",
        "PROJECT_NODE_MODULES": "${PROJECT_NODE_MODULES}",
    },
}

CONFIG = {
    "command": "${RYE_NODE}",
    "args": [],
    "timeout": 300,
}
```

**Features:**
- Finds Node.js interpreter
- Sets `NODE_ENV` appropriately
- Supports node modules path resolution

### 3. MCP HTTP Runtime (`mcp_http_runtime.py`)

**Purpose:** Execute HTTP-based MCP calls

```python
__version__ = "1.0.0"
__tool_type__ = "runtime"
__executor_id__ = "http_client"  # ← Delegates to HTTP client primitive
__category__ = "runtimes"

ENV_CONFIG = {
    "http_client": {
        "type": "http_config",
        "timeout": 30,
        "retries": 3,
        "headers": {
            "Content-Type": "application/json",
            "User-Agent": "RYE/1.0",
        },
    },
}

CONFIG = {
    "method": "POST",
    "url": "${MCP_ENDPOINT}",
    "headers": "${MCP_HEADERS}",
    "timeout": 30,
}
```

**Features:**
- Configures HTTP client for MCP
- Connection pooling
- Retry logic
- Header defaults

## Runtime Metadata Pattern

All runtimes follow this pattern:

```python
# .ai/tools/rye/runtimes/{name}.py

__version__ = "X.Y.Z"
__tool_type__ = "runtime"              # Always "runtime"
__executor_id__ = "primitive_id"       # Points to: subprocess, http_client, etc.
__category__ = "runtimes"              # Always "runtimes"

# REQUIRED: Environment configuration
ENV_CONFIG = {
    "resource_type": {
        "type": "...",                 # Resource type (venv_python, binary, etc.)
        "search": [...],               # Search order (project, user, system)
        "var": "VAR_NAME",             # Environment variable to set
        "fallback": "default_value",   # Default if not found
    },
    "env": {
        "STATIC_VAR": "value",         # Static environment variables
        "TEMPLATE_VAR": "${VAR_NAME}", # Template variables (resolved)
    },
}

# Configuration passed to delegate primitive
CONFIG = {
    "command": "${RESOLVED_VAR}",      # Template variables resolved here
    "args": [],
    "timeout": 300,
}
```

## Environment Resolution

When a tool uses a runtime, RYE resolves the environment:

```
Tool invocation with __executor_id__ = "python_runtime"
    │
    ├─→ Load python_runtime.py
    ├─→ Get ENV_CONFIG
    │
    ├─→ env_resolver.resolve(ENV_CONFIG)
    │   │
    │   ├─→ Find Python interpreter
    │   │   ├─ project/.venv/bin/python3 → Not found
    │   │   ├─ ~/.venv/bin/python3 → Found ✓
    │   │   └─ RYE_PYTHON="/home/user/.venv/bin/python3"
    │   │
    │   └─→ Resolve template variables
    │       └─ PROJECT_VENV_PYTHON="${RYE_PYTHON}"
    │           → "/home/user/.venv/bin/python3"
    │
    └─→ Pass resolved env to subprocess primitive
        │
        command: "/home/user/.venv/bin/python3"
        env: {
            "RYE_PYTHON": "/home/user/.venv/bin/python3",
            "PYTHONUNBUFFERED": "1",
            "PROJECT_VENV_PYTHON": "/home/user/.venv/bin/python3",
        }
```

## Template Variables

Runtimes define template variables that can be used in `CONFIG`:

### Python Runtime Variables

| Variable | Resolved By | Example |
|----------|------------|---------|
| `${RYE_PYTHON}` | venv_python search | `/home/user/.venv/bin/python3` |
| `${PYTHONUNBUFFERED}` | Static | `1` |
| `${PROJECT_VENV_PYTHON}` | Template | `/home/user/.venv/bin/python3` |

### Node.js Runtime Variables

| Variable | Resolved By | Example |
|----------|------------|---------|
| `${RYE_NODE}` | venv_node search | `/home/user/.nvm/versions/node/v18.0.0/bin/node` |
| `${NODE_ENV}` | Static | `production` |
| `${PROJECT_NODE_MODULES}` | Template | `./node_modules` |

## Usage in Tools

### Example: Tool Using Python Runtime

```python
# .ai/tools/rye/capabilities/git.py

__tool_type__ = "python"
__executor_id__ = "python_runtime"  # ← Use Python runtime
__category__ = "capabilities"

def main(command: str, args: list = None) -> dict:
    """Execute git command."""
    import subprocess
    result = subprocess.run(
        ["git", command] + (args or []),
        capture_output=True
    )
    return {"stdout": result.stdout}
```

**When invoked:**
1. RYE loads git.py
2. Sees `__executor_id__ = "python_runtime"`
3. Loads python_runtime.py
4. Resolves ENV_CONFIG → finds Python interpreter
5. Executes via subprocess primitive with resolved environment
6. Python script runs with correct Python binary and environment

## Runtime Features

| Feature | Python Runtime | Node.js Runtime | MCP HTTP Runtime |
|---------|---|---|---|
| **Delegates to** | subprocess | subprocess | http_client |
| **Environment** | venv resolution | npm/node resolution | HTTP config |
| **Key Variable** | RYE_PYTHON | RYE_NODE | MCP_ENDPOINT |
| **Default Command** | python3 | node | curl/httpx |
| **Use Case** | Python tools | Node.js tools | MCP calls |

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Delegation** | Points to primitives |
| **Environment** | Declares ENV_CONFIG |
| **Language** | Python (schema location) |
| **Discoverability** | Loaded on demand by RYE |
| **Schema/Code** | Schema only (code in primitive) |
| **Template Variables** | Resolved at execution time |

## How Runtimes Work in Routing

```
Tool with __executor_id__ = "python_runtime"
    │
    ├─→ Executor loads python_runtime.py
    ├─→ Sees __executor_id__ = "subprocess"
    │
    ├─→ Load subprocess primitive
    ├─→ Resolve ENV_CONFIG from python_runtime
    │   └─ env_resolver.resolve() → finds Python, resolves variables
    │
    └─→ Execute subprocess primitive with resolved environment
        │
        └─→ lilux.primitives.subprocess.execute(resolved_config)
            │
            └─→ Result
```

## Search Order for Interpreters

### Project Search (Highest Priority)
```
{project}/.venv/bin/python3
{project}/.virtualenv/bin/python3
{project}/venv/bin/python3
```

### User Search (Medium Priority)
```
~/.venv/bin/python3
~/.virtualenv/bin/python3
~/.pyenv/versions/*/bin/python3
```

### System Search (Lowest Priority)
```
/usr/bin/python3
/usr/local/bin/python3
/opt/python/bin/python3
```

## Related Documentation

- [core/primitives](primitives.md) - Layer 1 primitives that runtimes delegate to
- [overview](overview.md) - All categories
- [../executor/routing](../executor/routing.md) - How routing works with runtimes
- [../bundle/structure](../bundle/structure.md) - Bundle organization
