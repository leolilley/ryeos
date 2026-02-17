**Source:** Original implementation: `kiwi_mcp/primitives/` in kiwi-mcp

# Primitives Category

## Purpose

Primitives are the **hardcoded execution engines** in Lilux. Only **2 primitives** contain actual execution code:

- `subprocess` - Execute shell commands
- `http_client` - Execute HTTP requests

Everything else in the system is data-driven configuration that eventually routes to one of these two primitives.

**Location:** `lilux/primitives/` (code) + `.ai/tools/rye/core/primitives/` (schemas)  
**Protected:** ✅ Yes (core tool - cannot be shadowed)

## Architecture

```
Tool Execution Flow
    │
    └─→ PrimitiveExecutor
        │
        ├─→ Resolve executor chain (tool → runtime → primitive)
        ├─→ Verify integrity at each step
        ├─→ Validate parameters against CONFIG_SCHEMA
        │
        └─→ Execute via hardcoded primitive
            ├─→ SubprocessPrimitive (shell commands)
            └─→ HttpClientPrimitive (HTTP requests)
```

## The 2 Execution Primitives

### 1. Subprocess (`subprocess.py`)

**Purpose:** Execute shell commands

**Location:** `lilux/primitives/subprocess.py`

```python
class SubprocessPrimitive:
    async def execute(
        self,
        command: str,
        args: List[str] = None,
        env: Dict[str, str] = None,
        cwd: str = None,
        stdin: str = None,
        timeout: int = 300
    ) -> SubprocessResult:
        """Execute shell command."""
```

**Result:**
```python
@dataclass
class SubprocessResult:
    stdout: str
    stderr: str
    return_code: int
    duration_ms: int
```

### 2. HTTP Client (`http_client.py`)

**Purpose:** Execute HTTP requests with retry logic

**Location:** `lilux/primitives/http_client.py`

```python
class HttpClientPrimitive:
    async def execute(
        self,
        method: str,
        url: str,
        headers: Dict[str, str] = None,
        body: str = None,
        timeout: int = 30,
        retries: int = 3
    ) -> HttpResult:
        """Execute HTTP request."""
```

**Result:**
```python
@dataclass
class HttpResult:
    status_code: int
    headers: Dict[str, str]
    body: str
    duration_ms: int
```

## Helper Modules (Not Execution Primitives)

These modules support the execution pipeline but don't execute tools:

| Module | Purpose |
|--------|---------|
| `executor.py` | PrimitiveExecutor - orchestrates chain resolution and routing |
| `chain_validator.py` | Validates executor chains |
| `integrity_verifier.py` | Verifies file integrity via hashes |
| `lockfile.py` | Lockfile data structures |
| `errors.py` | Error types for the primitive system |

## How Tools Route to Primitives

```
Tool: scraper.py
  └─→ __executor_id__ = "python_runtime"

Runtime: python_runtime.py
  └─→ __executor_id__ = "subprocess"

Primitive: subprocess.py
  └─→ __executor_id__ = None (terminal)
      │
      └─→ SubprocessPrimitive.execute()
```

## Schema Files vs Code

| Type | Location | Purpose |
|------|----------|---------|
| **Schema** | `.ai/tools/rye/primitives/*.py` | Metadata for discovery |
| **Code** | `lilux/primitives/*.py` | Actual execution logic |

Schemas define `CONFIG_SCHEMA`, `__executor_id__ = None`, etc.
Code contains the actual `execute()` implementation.

## Key Points

1. **Only 2 execution primitives:** subprocess and http_client
2. **Hardcoded in Lilux:** Not data-driven, actual Python code
3. **Terminal nodes:** `__executor_id__ = None` means no delegation
4. **All tools eventually route here:** Through the executor chain
5. **Helper modules:** chain_validator, integrity_verifier are utilities, not executors

## Related Documentation

- [overview](overview.md) - All categories
- [core/runtimes](runtimes.md) - Runtimes that delegate to primitives
- [../executor/overview](../executor/overview.md) - How routing works
- [../mcp-tools/execute](../mcp-tools/execute.md) - Tool execution via MCP
