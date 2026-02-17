**Source:** Original implementation: `kiwi_mcp/primitives/executor.py` and `kiwi_mcp/runtime/env_resolver.py` in kiwi-mcp

# Executor Routing

## Routing Logic

The executor is **fully data-driven** - no hardcoded executor IDs!

```python
# lilux/primitives/executor.py
class PrimitiveExecutor:
    """
    Data-driven executor - resolves executor chains recursively.

    KEY: All executor IDs are resolved from .ai/tools/ filesystem.
    No hardcoded lists of primitives or runtimes!
    """

    def __init__(self, ai_tools_path: Path, env_resolver: EnvResolver):
        self.ai_tools_path = ai_tools_path  # Path to .ai/tools/
        self.env_resolver = env_resolver
        # NO self.primitives or self.runtimes registries!

    def execute(self, tool_path: Path, parameters: dict) -> Any:
        """
        Execute a tool by resolving its executor chain recursively.

        Chain: tool → executor_id → (runtime → executor_id) → ... → primitive
        """
        # 1. Load tool metadata
        metadata = self._load_metadata(tool_path)
        executor_id = metadata.get("__executor_id__")

        # 2. Check if this is a primitive (no delegation)
        if executor_id is None:
            # LAYER 1: Primitive - execute directly
            return self._execute_primitive(tool_path, parameters)

        # 3. DATA-DRIVEN: Resolve executor path from filesystem
        executor_path = self._resolve_executor_path(executor_id)

        # 4. Load executor metadata
        executor_metadata = self._load_metadata(executor_path)
        executor_type = executor_metadata.get("__tool_type__")

        # 5. Handle runtime environment resolution
        if executor_type == "runtime":
            # LAYER 2: Runtime - resolve ENV_CONFIG at execution time
            resolved_env = self.env_resolver.resolve(
                executor_metadata.get("ENV_CONFIG"),
                context=self._get_context(tool_path)
            )
            # Merge resolved env into parameters
            parameters = {**parameters, **resolved_env}

        # 6. RECURSIVE: Execute executor (resolves chain until primitive)
        return self.execute(executor_path, parameters)

    def _resolve_executor_path(self, executor_id: str, current_space: str) -> Tuple[Path, str]:
        """
        Resolve executor_id to actual path in .ai/tools/ with precedence.

        Uses explicit precedence: project → user → system, starting from the current tool's space.

        Returns:
            (path, space) where space is "project", "user", or "system"
        """
        # Determine search order based on precedence
        # Start from current space (for equal or higher precedence tools)
        search_order = []

        if current_space == "system":
            # System tools can only depend on system tools (immutable)
            search_order = [("system", self.system_space / "tools")]

        elif current_space == "user":
            # User tools can depend on user or system tools
            search_order = [
                ("user", self.user_space / "tools"),
                ("system", self.system_space / "tools")
            ]

        elif current_space == "project":
            # Project tools can depend on project, user, or system tools
            search_order = [
                ("project", self.project_space / "tools"),
                ("user", self.user_space / "tools"),
                ("system", self.system_space / "tools")
            ]

        # Search in precedence order
        for space_name, base_path in search_order:
            if not base_path.exists():
                continue

            # Check root of category (e.g., .ai/tools/rye/subprocess.py)
            potential_path = base_path / f"{executor_id}.py"
            if potential_path.exists():
                return potential_path, space_name

            # Search subdirectories (primitives/, runtimes/, etc.)
            for sub_dir in base_path.rglob("*.py"):
                if sub_dir.stem == executor_id:
                    return sub_dir, space_name

        raise ValueError(f"Executor '{executor_id}' not found in accessible spaces (project/user/system)")

    def _execute_primitive(self, tool_path: Path, parameters: dict) -> Any:
        """
        Execute a primitive by loading from Lilux package
        """
        primitive_name = tool_path.stem
        # Dynamic import from Lilux primitives
        primitive_module = importlib.import_module(f"lilux.primitives.{primitive_name}")
        return primitive_module.execute(parameters)
```

## Example 1: Primitive Execution (Direct)

### Tool Definition

```python
# .ai/tools/rye/primitives/subprocess.py

__version__ = "1.0.1"
__tool_type__ = "primitive"
__executor_id__ = None      # ← No delegation
__category__ = "primitives"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {"type": "string", "description": "Command to run"},
        "args": {"type": "array", "items": {"type": "string"}},
        "env": {"type": "object", "description": "Environment variables"},
        "cwd": {"type": "string", "description": "Working directory"},
        "timeout": {"type": "integer", "default": 300},
    },
    "required": ["command"]
}
```

### Execution Flow

```
Tool Call: subprocess(config={"command": "echo", "args": ["{message}"]}, params={"message": "hello"})
    │
    ├─→ PrimitiveExecutor loads .ai/tools/rye/primitives/subprocess.py
    ├─→ Checks __executor_id__ = None
    │
    └─→ ROUTE 1: Execute Primitive Directly
        │
        ├─→ _execute_primitive(subprocess.py)
        │   │
        │   └─→ import lilux.primitives.subprocess
        │       │
        │       └─→ lilux.primitives.subprocess.execute(
        │               config={"command": "echo", "args": ["{message}"], "timeout": 300},
        │               params={"message": "hello"}
        │           )
        │           # Args templated: ["{message}"] → ["hello"]
        │
        └─→ Result: {"stdout": "hello\n", "returncode": 0}
```

### Code Path

```
lilux/primitives/executor.py:execute(subprocess.py)
    │
    ├─→ _load_metadata(subprocess.py)
    │   └─→ __executor_id__ = None
    │
    └─→ _execute_primitive(subprocess.py, parameters)
        │
        ├─→ importlib.import_module("lilux.primitives.subprocess")
        │
        └─→ lilux/primitives/subprocess.py:execute(parameters)
            │
            └─→ subprocess.run(["echo", "hello"])
```

## Example 2: Runtime Execution with Environment Resolution

### Tool Definition

```python
# .ai/tools/rye/runtimes/python_runtime.py

__version__ = "2.0.0"
__tool_type__ = "runtime"
__executor_id__ = "subprocess"  # ← Delegates to subprocess primitive
__category__ = "runtimes"

# Declares how to find and configure Python
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

# Base command configuration (resolved at runtime)
CONFIG = {
    "command": "${RYE_PYTHON}",  # ← Will be resolved to actual path
    "args": [],
    "timeout": 300,
}
```

### Execution Flow

```
Tool Call: python_runtime(script="print('hello')")
    │
    ├─→ PrimitiveExecutor loads .ai/tools/rye/runtimes/python_runtime.py
    ├─→ Checks __executor_id__ = "subprocess"
    │
    └─→ ROUTE 2: Runtime with Environment Resolution
        │
        ├─→ _resolve_executor_path("subprocess")
        │   └─→ Finds: .ai/tools/rye/primitives/subprocess.py
        │
        ├─→ _load_metadata(subprocess.py)
        │   └─→ __executor_id__ = None (primitive!)
        │
        ├─→ env_resolver.resolve(ENV_CONFIG from python_runtime.py)
        │   │
        │   ├─→ Find Python interpreter
        │   │   ├─ Search 1: project/.venv/bin/python3
        │   │   ├─ Search 2: ~/.venv/bin/python3
        │   │   └─ Search 3: /usr/bin/python3 ✓ Found
        │   │
        │   └─→ Return resolved environment:
        │       {
        │           "RYE_PYTHON": "/usr/bin/python3",
        │           "PYTHONUNBUFFERED": "1",
        │           "PROJECT_VENV_PYTHON": "/usr/bin/python3"
        │       }
        │
        ├─→ Merge resolved env into parameters
        │
        └─→ RECURSIVE: execute(subprocess.py, parameters_with_env)
            │
            ├─→ _execute_primitive(subprocess.py)
            │   │
            │   └─→ lilux.primitives.subprocess.execute({
            │           "command": "/usr/bin/python3",
            │           "args": ["-c", "print('hello')"],
            │           "env": {
            │               "RYE_PYTHON": "/usr/bin/python3",
            │               "PYTHONUNBUFFERED": "1",
            │               "PROJECT_VENV_PYTHON": "/usr/bin/python3"
            │           },
            │           "timeout": 300
            │       })
            │
            └─→ Result: {"stdout": "hello\n", "returncode": 0}
```

### Code Path

```
lilux/primitives/executor.py:execute(python_runtime.py)
    │
    ├─→ _load_metadata(python_runtime.py)
    │   └─→ __executor_id__ = "subprocess"
    │
    ├─→ _resolve_executor_path("subprocess")
    │   └─→ Returns: .ai/tools/rye/primitives/subprocess.py
    │
    ├─→ env_resolver.resolve(ENV_CONFIG)
    │   └─→ Returns: {"RYE_PYTHON": "/usr/bin/python3", ...}
    │
    ├─→ Merge resolved env into parameters
    │
    └─→ RECURSIVE: execute(subprocess.py, parameters_with_env)
        │
        └─→ _execute_primitive(subprocess.py)
            │
            ├─→ importlib.import_module("lilux.primitives.subprocess")
            │
            └─→ lilux/primitives/subprocess.py:execute()
                │
                └─→ subprocess.run(["/usr/bin/python3", "-c", "print('hello')"])
```

## Example 3: Tool Delegating to Runtime

### Tool Definition

```python
# .ai/tools/rye/capabilities/git.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"  # ← Delegates to Python runtime
__category__ = "capabilities"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {"type": "string", "description": "git command (clone, pull, etc.)"},
        "args": {"type": "array", "items": {"type": "string"}},
    },
    "required": ["command"]
}

def main(command: str, args: list = None) -> dict:
    """Execute git command."""
    import subprocess
    result = subprocess.run(
        ["git", command] + (args or []),
        capture_output=True,
        text=True
    )
    return {
        "stdout": result.stdout,
        "stderr": result.stderr,
        "returncode": result.returncode,
    }
```

### Execution Flow

```
Tool Call: git(command="status")
    │
    ├─→ PrimitiveExecutor loads .ai/tools/rye/capabilities/git.py
    ├─→ Checks __executor_id__ = "python_runtime"
    │
    └─→ ROUTE 3: Tool Delegating to Runtime
        │
        ├─→ _resolve_executor_path("python_runtime")
        │   └─→ Finds: .ai/tools/rye/runtimes/python_runtime.py
        │
        ├─→ _load_metadata(python_runtime.py)
        │   └─→ __executor_id__ = "subprocess"
        │
        ├─→ env_resolver.resolve(ENV_CONFIG from python_runtime.py)
        │   └─→ Returns: {"RYE_PYTHON": "/usr/bin/python3", ...}
        │
        ├─→ Merge resolved env into parameters
        │
        ├─→ _resolve_executor_path("subprocess")
        │   └─→ Finds: .ai/tools/rye/primitives/subprocess.py
        │
        ├─→ _load_metadata(subprocess.py)
        │   └─→ __executor_id__ = None (primitive!)
        │
        └─→ Build final configuration:
            │
            ├─→ _execute_primitive(subprocess.py)
            │   │
            │   └─→ lilux.primitives.subprocess.execute({
            │           "command": "/usr/bin/python3",
            │           "args": ["-m", "git", "status"],
            │           "env": {"RYE_PYTHON": "/usr/bin/python3", ...},
            │           "timeout": 300
            │       })
            │
            └─→ subprocess.run(["/usr/bin/python3", "-m", "git", "status"])
```

### Code Path

```
lilux/primitives/executor.py:execute(git.py)
    │
    ├─→ _load_metadata(git.py)
    │   └─→ __executor_id__ = "python_runtime"
    │
    ├─→ _resolve_executor_path("python_runtime")
    │   └─→ Returns: .ai/tools/rye/runtimes/python_runtime.py
    │
    ├─→ env_resolver.resolve(ENV_CONFIG)
    │   └─→ Returns: {"RYE_PYTHON": "/usr/bin/python3", ...}
    │
    ├─→ Merge resolved env into parameters
    │
    └─→ RECURSIVE: execute(python_runtime.py, parameters_with_env)
        │
        ├─→ _load_metadata(python_runtime.py)
        │   └─→ __executor_id__ = "subprocess"
        │
        ├─→ _resolve_executor_path("subprocess")
        │   └─→ Returns: .ai/tools/rye/primitives/subprocess.py
        │
        ├─→ _load_metadata(subprocess.py)
        │   └─→ __executor_id__ = None (primitive!)
        │
        └─→ _execute_primitive(subprocess.py, parameters_with_env)
            │
            ├─→ importlib.import_module("lilux.primitives.subprocess")
            │
            └─→ lilux/primitives/subprocess.py:execute()
                │
                └─→ subprocess.run(["/usr/bin/python3", "-m", "git", "status"])
```

## Environment Resolution Details

### How ENV_CONFIG Works

```python
# Runtime declares environment needs
ENV_CONFIG = {
    "interpreter": {
        "type": "venv_python",           # Type of resource
        "search": ["project", "user", "system"],  # Search order
        "var": "RYE_PYTHON",             # Variable name
        "fallback": "python3",           # Default if not found
    },
    "env": {
        "PYTHONUNBUFFERED": "1",         # Static env var
        "PROJECT_VENV_PYTHON": "${RYE_PYTHON}",  # Template variable
    },
}
```

### Resolution Process

```
env_resolver.resolve(ENV_CONFIG, context)
    │
    ├─→ Process "interpreter" section:
    │   │
    │   ├─→ type="venv_python" → Search for Python venv
    │   ├─→ search=["project", "user", "system"] → Try each in order
    │   │
    │   ├─→ Search 1: {project}/.venv/bin/python3
    │   │   └─→ Not found
    │   │
    │   ├─→ Search 2: ~/.venv/bin/python3
    │   │   └─→ Not found
    │   │
    │   ├─→ Search 3: /usr/bin/python3
    │   │   └─→ ✓ Found!
    │   │
    │   └─→ Set: RYE_PYTHON="/usr/bin/python3"
    │
    ├─→ Process "env" section:
    │   │
    │   ├─→ PYTHONUNBUFFERED="1" → Static, add as-is
    │   ├─→ PROJECT_VENV_PYTHON="${RYE_PYTHON}" → Template
    │   │   └─→ Substitute with resolved value → "/usr/bin/python3"
    │   │
    │   └─→ Set: PROJECT_VENV_PYTHON="/usr/bin/python3"
    │
    └─→ Return resolved environment:
        {
            "RYE_PYTHON": "/usr/bin/python3",
            "PYTHONUNBUFFERED": "1",
            "PROJECT_VENV_PYTHON": "/usr/bin/python3",
        }
```

## Executor ID Reference

| `__executor_id__` | Type | Description | Layer |
|------------------|------|-------------|-------|
| `None` | Primitive | Direct execution, no delegation | Layer 1 |
| `"subprocess"` | Runtime | Execute shell commands | Layer 2 |
| `"http_client"` | Runtime | Execute HTTP requests | Layer 2 |
| `"python_runtime"` | Runtime | Execute Python code | Layer 2 |
| `"node_runtime"` | Runtime | Execute Node.js code | Layer 2 |

## Common Routing Patterns

### Pattern 1: Direct Primitive

```
Your Tool → [__executor_id__ = None]
    → _execute_primitive()
    → lilux.primitives.{name}.execute()
    → Result
```

### Pattern 2: Via Subprocess Primitive

```
Your Tool → [__executor_id__ = "subprocess"]
    → _resolve_executor_path("subprocess")
    → Finds: .ai/tools/rye/primitives/subprocess.py
    → _execute_primitive(subprocess.py)
    → lilux.primitives.subprocess.execute()
    → Result
```

### Pattern 3: Via Language Runtime

```
Your Tool → [__executor_id__ = "python_runtime"]
    → _resolve_executor_path("python_runtime")
    → Finds: .ai/tools/rye/runtimes/python_runtime.py
    → env_resolver.resolve(ENV_CONFIG)
    → _resolve_executor_path("subprocess")
    → Finds: .ai/tools/rye/primitives/subprocess.py
    → _execute_primitive(subprocess.py)
    → lilux.primitives.subprocess.execute()
    → Result
```

**KEY:** All executor IDs are resolved via `_resolve_executor_path()` which searches `.ai/tools/` filesystem. No hardcoded lists!

## Related Documentation

- [overview](overview.md) - Executor architecture
- [bundle/structure](../bundle/structure.md) - Tool organization
- [categories/primitives](../categories/primitives.md) - Primitive implementations
- [categories/runtimes](../categories/runtimes.md) - Runtime specifications
