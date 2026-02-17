**Source:** Original implementation: `.ai/tools/rye/agent/threads/capabilities/` in kiwi-mcp

# Capabilities Category

## Purpose

Capabilities are **sandboxing constraints** for LLM-driven agent thread execution. They define what system features an agent thread is allowed to access.

**⚠️ IMPORTANT:** Capabilities are **ONLY enforced when running LLM agent threads**. Direct MCP calls are NOT restricted by capabilities.

**Location:** `.ai/tools/rye/agent/threads/capabilities/`  
**Count:** 5 tools (no mcp capability - that's at thread level)  
**Executor:** All use `python_runtime`  
**Protected:** ❌ No (part of app tools, not core)

## Core Capabilities

### 1. Git (`git.py`)

**Purpose:** Execute git commands

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {"type": "string", "description": "git command (clone, pull, push, etc.)"},
        "args": {"type": "array", "items": {"type": "string"}},
        "repo": {"type": "string", "description": "Repository path"},
    },
    "required": ["command"]
}
```

**Typical Operations:**
- `git clone <url>`
- `git pull`
- `git push`
- `git status`
- `git log`

### 2. Filesystem (`fs.py`)

**Purpose:** Filesystem operations

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["read", "write", "delete", "list", "mkdir"]},
        "path": {"type": "string"},
        "content": {"type": "string"},  # For write
        "recursive": {"type": "boolean", "default": False},
    },
    "required": ["operation", "path"]
}
```

**Typical Operations:**
- Read file
- Write file
- Delete file/directory
- List directory
- Create directory

### 3. Database (`db.py`)

**Purpose:** Database operations

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["query", "execute", "create", "drop"]},
        "connection": {"type": "string"},
        "sql": {"type": "string"},
        "params": {"type": "object"},
    },
    "required": ["operation", "sql"]
}
```

**Typical Operations:**
- Execute SQL query
- Insert/update/delete
- Create/drop tables
- Database transactions

### 4. Network (`net.py`)

**Purpose:** Network operations

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["ping", "dns", "port_scan", "traceroute"]},
        "target": {"type": "string"},
        "timeout": {"type": "integer", "default": 10},
    },
    "required": ["operation", "target"]
}
```

**Typical Operations:**
- Ping host
- DNS resolution
- Port scanning
- Network diagnostics

### 5. Process (`process.py`)

**Purpose:** Process management

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {"type": "string", "enum": ["list", "kill", "info", "start", "stop"]},
        "pid": {"type": "integer"},
        "name": {"type": "string"},
    },
}
```

**Typical Operations:**
- List running processes
- Get process information
- Kill/stop process
- Monitor resource usage

## Metadata Pattern

All capabilities follow this pattern:

```python
# .ai/tools/rye/agent/threads/capabilities/{name}.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"  # All use Python runtime
__category__ = "capabilities"       # All in capabilities

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Capability implementation."""
    pass
```

## Usage Examples

### Git Capability

```bash
Call git with:
  command: "status"
  repo: "/path/to/repo"
```

### Filesystem Capability

```bash
Call fs with:
  operation: "read"
  path: "/path/to/file.txt"
```

### Database Capability

```bash
Call db with:
  operation: "query"
  sql: "SELECT * FROM users WHERE id = ?"
  params: {"id": 123}
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 5 tools |
| **Location** | `.ai/tools/rye/threads/capabilities/` |
| **Executor** | All use `python_runtime` |
| **Purpose** | Sandboxing for thread execution |
| **Enforcement** | ONLY when running LLM threads |
| **Protected** | ❌ No (app tool, replaceable) |

## When Capabilities Apply

```
Direct MCP Call (e.g., from Claude Desktop)
    │
    └─→ NO capability enforcement
        └─→ Full system access

LLM Thread Execution (via threads/)
    │
    ├─→ Thread config specifies allowed capabilities
    │   capabilities: ["git", "fs"]
    │
    └─→ Thread runtime enforces restrictions
        ├─→ git operations: ✅ allowed
        ├─→ fs operations: ✅ allowed
        ├─→ db operations: ❌ blocked
        └─→ net operations: ❌ blocked
```

## Capability Relationships

```
agent/threads/
├── thread_create.py
├── run_create.py
│   └─→ Reads capability config
│
└── capabilities/
    ├─→ git.py (version control)
    ├─→ fs.py (file operations)
    ├─→ db.py (data operations)
    ├─→ net.py (network operations)
    └─→ process.py (system processes)
```

## Related Documentation

- [overview](../overview.md) - All data tools
- [agent/threads](threads.md) - Thread execution (parent of capabilities)
- [../bundle/structure](../bundle/structure.md) - Bundle organization with tool spaces model
- [../../tool-resolution-and-validation.md](../../executor/tool-resolution-and-validation.md) - Tool spaces, precedence, and validation

**Note:** Capabilities are unrelated to tool spaces. Capabilities are a **separate feature** for LLM agent thread sandboxing. Tool spaces (project/user/system) handle tool resolution and shadowing. Direct MCP calls bypass capability restrictions.