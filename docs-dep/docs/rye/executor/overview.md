**Source:** Original implementation: `kiwi_mcp/primitives/executor.py` in kiwi-mcp

# Executor Overview

## Purpose

The executor is RYE's core routing mechanism. It reads tool metadata and intelligently routes execution to the appropriate Lilux primitive or runtime based on:

- `__tool_type__` - What kind of tool (primitive, runtime, python, etc.)
- `__executor_id__` - Which executor to delegate to (None, "subprocess", "http_client", "python_runtime", etc.)

## Architecture

```
┌─────────────────────────────────┐
│   Tool Invocation (LLM/User)    │
└──────────────┬──────────────────┘
               │
               ▼
┌──────────────────────────────────────┐
│  RYE Executor                        │
│  (DATA-DRIVEN - No hardcoded IDs!)   │
│                                      │
│  1. Load tool metadata               │
│  2. Check __executor_id__            │
│  3. Resolve executor from filesystem │
│  4. Route recursively to primitive   │
└───────────┬──────────────────────────┘
            │
     ┌──────┴──────┬──────────────┬────────────┐
     │             │              │            │
     ▼             ▼              ▼            ▼
┌─────────┐  ┌──────────┐  ┌────────────┐  ┌───────────┐
│Primitive│  │ Runtime  │  │ Subprocess │  │HTTP Client│
│(No Del) │  │Delegated │  │ Primitive  │  │Primitive  │
│Schema   │  │w/ ENV    │  │(exec cmds) │  │(http req) │
└─────────┘  └──────────┘  └────────────┘  └───────────┘
     ▲            ▲              ▲              ▲
     │            │              │              │
     └────────────┴──────────────┴──────────────┘
            LILUX MICROKERNEL
```

**KEY:** PrimitiveExecutor loads executors on-demand from `.ai/tools/` filesystem, not from hardcoded registries.

## Three-Layer Routing

### Layer 1: Primitives (`__executor_id__ = None`)

**Direct execution - no delegation**

```python
# .ai/tools/rye/core/primitives/subprocess.py
__tool_type__ = "primitive"
__executor_id__ = None  # ← No delegation
__category__ = "primitives"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {"type": "string"},
        "args": {"type": "array", "items": {"type": "string"}},
        "timeout": {"type": "integer", "default": 300},
    },
    "required": ["command"]
}
```

**Execution:** PrimitiveExecutor calls Lilux primitive directly

```python
# In rye/executor.py (pseudocode)
if executor_id is None:
    # Primitive - execute directly
    return lilux.primitives.subprocess.execute(config)
```

### Layer 2: Runtimes (`__executor_id__ = "subprocess"`)

**Environment-configured delegation**

```python
# .ai/tools/rye/core/runtimes/python_runtime.py
__tool_type__ = "runtime"
__executor_id__ = "subprocess"  # ← Delegates to subprocess primitive
__category__ = "runtimes"

ENV_CONFIG = {
    "interpreter": {
        "type": "venv_python",
        "search": ["project", "user", "system"],
        "var": "RYE_PYTHON",
        "fallback": "python3",
    },
    "env": {
        "PYTHONUNBUFFERED": "1",
        "PROJECT_VENV_PYTHON": "${RYE_PYTHON}",
    },
}

CONFIG = {
    "command": "${RYE_PYTHON}",
    "args": [],
    "timeout": 300,
}
```

**Execution:**

1. PrimitiveExecutor loads Python runtime metadata
2. Calls env_resolver to resolve `ENV_CONFIG`
3. Resolves template variables: `${RYE_PYTHON}` → `/path/to/.venv/bin/python3`
4. Passes resolved config to subprocess primitive

```python
# In rye/executor.py (pseudocode)
if executor_id == "subprocess":
    resolved_env = env_resolver.resolve(ENV_CONFIG, context)
    resolved_config = resolve_templates(config, resolved_env)
    return lilux.primitives.subprocess.execute(resolved_config)
```

### Layer 3: Tools (`__executor_id__ = "python_runtime"`)

**User tools delegating to runtimes**

```python
# .ai/tools/rye/agent/threads/capabilities/git.py
__tool_type__ = "python"
__executor_id__ = "python_runtime"  # ← Delegates to python runtime
__category__ = "capabilities"

def main(command: str, args: list = None) -> dict:
    """Execute git command."""
    return {"result": subprocess.run([command] + (args or []))}
```

**Execution:**

1. PrimitiveExecutor loads git tool metadata
2. Checks `__executor_id__ = "python_runtime"`
3. Loads Python runtime metadata (layer 2)
4. Resolves environment via env_resolver
5. Passes to subprocess primitive (layer 1)

## Metadata Parsing

The executor parses metadata from multiple sources:

### Python Files

```python
# Any .ai/tools/{category}/{name}.py
__version__ = "1.0.0"
__tool_type__ = "python"          # Type: primitive, runtime, python, python_lib
__executor_id__ = "python_runtime" # Executor: None, "subprocess", "http_client", "python_runtime"
__category__ = "capabilities"      # Category: rye, python, utility, etc.

CONFIG_SCHEMA = {
    "type": "object",
    "properties": { ... },
}

ENV_CONFIG = { ... }  # Optional for runtimes

def main(**kwargs) -> dict:
    """Tool implementation."""
    pass
```

### YAML Files

```yaml
# .ai/tools/rye/mcp/mcp_stdio.yaml
name: mcp_stdio
version: "1.0.0"
tool_type: runtime
executor_id: subprocess
category: mcp

config:
  command: "${RYE_MCP_RUNNER}"
  args: ["--type", "stdio"]

env_config:
  mcp_runner:
    type: binary
    search: ["project", "system"]
    var: "RYE_MCP_RUNNER"
    fallback: "mcp"
```

## On-Demand Loading

**IMPORTANT: Tools are loaded when needed, not at startup**

RYE does NOT scan `.ai/tools/` at startup. Instead, tools are loaded on-demand when the LLM requests them through the 5 primary MCP tools.

### How On-Demand Loading Works

```
LLM Request
    │
    ▼
┌─────────────────────────────────────────────┐
│  execute(item_type="tool", item_id="git")   │
│  (One of 5 primary MCP tools)               │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  Executor loads .ai/tools/.../git.py        │
│  - Parse metadata at load time              │
│  - Extract __tool_type__, __executor_id__   │
│  - Extract CONFIG_SCHEMA, ENV_CONFIG        │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  Resolve executor chain                     │
│  - git → python_runtime → subprocess        │
│  - Each step loaded on-demand               │
└──────────────────┬──────────────────────────┘
                   │
                   ▼
┌─────────────────────────────────────────────┐
│  Execute via Lilux primitives               │
└─────────────────────────────────────────────┘
```

### Benefits of On-Demand Loading

| Benefit              | Description                                       |
| -------------------- | ------------------------------------------------- |
| **Fast startup**     | No filesystem scanning at initialization          |
| **Memory efficient** | Only loaded tools consume memory                  |
| **Dynamic updates**  | Tool changes take effect immediately              |
| **No stale cache**   | Metadata parsed fresh on each execution           |

**See Also:** [../principles.md](../principles.md) for on-demand loading principles, [./tool-resolution-and-validation.md](./tool-resolution-and-validation.md) for tool spaces and validation

## Tool Spaces Model

RYE uses a three-space model for tool resolution with explicit precedence:

| Space    | Location                              | Mutability | Precedence |
| --------- | ------------------------------------- | ------------ | ----------- |
| **Project** | `{project}/.ai/`                   | Mutable     | 1 (highest)  |
| **User**    | `~/.ai/`                            | Mutable     | 2 (medium)   |
| **System**  | `site-packages/rye/.ai/`          | Immutable   | 3 (lowest)   |

**Key Principles:**
- Project tools have highest precedence (shadow user/system)
- User tools can shadow system tools
- System tools are immutable (read-only, installed via pip)
- Resolution is deterministic: always searches project → user → system

**Shadowing Behavior:** Users CAN shadow system tools by creating同名工具 in project or user space. This is INTENTIONAL - allows customization and experimentation.

**See Also:** [[./tool-resolution-and-validation.md]] for complete details on cross-space chain validation.

---

## Executor Resolution Order

When executing a tool:

1. **Receive `item_id` from MCP execute tool call**
2. **Resolve tool path using precedence:**
   - Search project space (`.ai/tools/`)
   - If not found, search user space (`~/.ai/tools/`)
   - If not found, search system space (`site-packages/rye/.ai/tools/`)
   - Return first match with `(path, space)` tuple
3. **Parse metadata at load time:**
   - Extract `__tool_type__`, `__executor_id__`, `__category__`
   - Extract `CONFIG_SCHEMA`, `ENV_CONFIG`
4. **If `__executor_id__` is None:**
   - Execute as primitive directly via Lilux
5. **Else:**
   - **ON-DEMAND:** Load executor from `.ai/tools/**/` by ID using same precedence rules
   - Parse executor metadata (not from registry—fresh from filesystem!)
   - Track space of each tool in the chain
   - If executor is runtime:
     - Resolve `ENV_CONFIG` via env_resolver
     - Merge resolved environment into parameters
   - **RECURSIVE:** Continue chain until reaching a primitive

**Key Point:** No startup registry, no pre-built tool index—everything loaded on-demand with explicit precedence!

---

## Primary MCP Tools (`rye/tools/`)

**The 5 primary MCP tools are the ONLY tools exposed to the LLM via MCP.**

All `.ai/tools/` contents are accessed through these 5 gateway tools—they are never directly exposed to MCP.

### The 5 Primary Tools

| Tool          | Purpose                                      |
| ------------- | -------------------------------------------- |
| **search**    | Search `.ai/tools/` by query/metadata        |
| **load**      | Load tool content from `.ai/tools/`          |
| **execute**   | Execute tools via executor chains            |
| **sign**      | Sign tools with cryptographic validation     |


### Architecture

```
┌─────────────────────────────────────────────────────────┐
│                         LLM                             │
└────────────────────────┬────────────────────────────────┘
                         │ MCP Protocol
                         ▼
┌─────────────────────────────────────────────────────────┐
│              5 Primary MCP Tools                        │
│  ┌────────┐ ┌──────┐ ┌─────────┐ ┌──────┐ ┌──────┐     │
│  │ search │ │ load │ │ execute │ │ sign │ │ help │     │
│  └────┬───┘ └───┬──┘ └────┬────┘ └───┬──┘ └───┬──┘     │
└───────┼─────────┼─────────┼──────────┼────────┼────────┘
        │         │         │          │        │
        └─────────┴────┬────┴──────────┴────────┘
                       │ On-demand loading
                       ▼
┌─────────────────────────────────────────────────────────┐
│                  .ai/tools/                             │
│  (RYE bundled + user custom tools)                      │
│  Loaded on-demand, NOT at startup                       │
└────────────────────────┬────────────────────────────────┘
                         │
                         ▼
┌─────────────────────────────────────────────────────────┐
│                 Lilux Primitives                        │
└─────────────────────────────────────────────────────────┘
```

### How It Works

```python
# LLM calls the "execute" primary tool
execute(
  item_type="tool",
  action="run",
  item_id="git",           # Tool in .ai/tools/
  parameters={"cmd": "status"}
)

# Executor loads git.py on-demand from .ai/tools/
# Parses metadata, resolves chain, executes via Lilux
```

**Key Points:**
- ✅ Only 5 primary tools exposed to MCP (LLM-visible)
- ✅ `.ai/tools/` accessed only through primary tools
- ✅ No startup scanning or registry building
- ✅ Tools loaded on-demand when requested

---

## Adding New Runtimes

1. Create file: `.ai/tools/rye/core/runtimes/rust_runtime.py`

2. Add metadata:

```python
__version__ = "1.0.0"
__tool_type__ = "runtime"
__executor_id__ = "subprocess"
__category__ = "runtimes"

ENV_CONFIG = {
    "interpreter": {
        "type": "binary",
        "search": ["project", "user", "system"],
        "var": "RYE_RUSTC",
        "fallback": "rustc",
    },
    "env": {
        "RUST_BACKTRACE": "1",
    },
}

CONFIG = {
    "command": "${RYE_RUSTC}",
    "args": ["--edition", "2021"],
    "timeout": 300,
}
```

3. **Done!** The executor loads it on-demand when first used.

### Why This Works

| Step              | What Happens                                          |
| ----------------- | ----------------------------------------------------- |
| File created      | Ready to use immediately                              |
| Tool uses runtime | `__executor_id__ = "rust_runtime"` in tool            |
| First execution   | Executor loads rust_runtime.py on-demand              |
| Metadata parsed   | `__executor_id__` identifies delegation chain         |
| Execution         | Recursive: tool → rust_runtime → subprocess → execute |

No code changes. No hardcoded lists. No restart needed. Pure data-driven extensibility.

**Note:** New runtimes go in `core/runtimes/` because they're essential infrastructure. Adding a runtime is a core extension, not an app feature.

## Related Documentation

 - [routing](routing.md) - Detailed routing examples
- [bundle/structure](../bundle/structure.md) - Tool organization in .ai/ with tool spaces model
- [./tool-resolution-and-validation.md](./tool-resolution-and-validation.md) - Tool spaces, precedence, and validation
- [categories/overview](../categories/overview.md) - All tool categories
- [categories/extractors](../categories/extractors.md) - Schema-driven metadata extraction
- [categories/parsers](../categories/parsers.md) - Content preprocessors
- [../principles.md](../principles.md) - On-demand tool loading architecture
- [cache/overview](../cache/overview.md) - RYE caching system with automatic invalidation
