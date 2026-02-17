# End-to-End Execution Flow

## Overview

This document shows the complete flow from LLM request to Lilux primitive execution.

## Three-Layer Architecture

```
┌─────────────────────────────────────────┐
│  LLM / MCP Client                        │
│  Calls: execute(item_type="tool", ...)     │
└──────────────┬──────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────┐
│  RYE PrimitiveExecutor (Orchestrator)        │
│  - Loads tool metadata                      │
│  - Resolves executor chain                  │
│  - Validates parameters                      │
│  - Merges environment variables             │
│  - Expands templates                       │
└──────────────┬──────────────────────────┘
               │
               ▼
┌───────────────────────────────────────────┐
│  Lilux Primitives (Microkernel)             │
│  - SubprocessPrimitive                      │
│  - HttpClientPrimitive                      │
└──────────────┬──────────────────────────┘
               │
               ▼
          System Resources
```

## Detailed Flow

### Step 1: LLM Request

```python
# LLM calls MCP "execute" tool
execute(
    item_type="tool",
    action="run",
    item_id="my_git_tool",
    parameters={"command": "status"},
    project_path="/path/to/project"
)
```

### Step 2: RYE Loads Tool

```python
# RYE PrimitiveExecutor loads on-demand
tool_path = Path("/path/to/project/.ai/tools/my_git_tool.py")
metadata = load_metadata(tool_path)
# Returns: __tool_type__, __executor_id__, CONFIG_SCHEMA, etc.
```

### Step 3: RYE Resolves Executor Chain

```python
# Example: my_git_tool -> python_runtime -> subprocess
chain = []
current_tool = metadata

while True:
    executor_id = current_tool.get("__executor_id__")

    if executor_id is None:
        # Primitive reached
        chain.append(current_tool)
        break

    # Load executor
    executor_path = resolve_executor_path(executor_id)
    executor_metadata = load_metadata(executor_path)
    chain.append(executor_metadata)
    current_tool = executor_metadata

# Result: [my_git_tool, python_runtime, subprocess]
```

### Step 4: RYE Validates

```python
# Validate parameters against CONFIG_SCHEMA
validator = SchemaValidator()
result = validator.validate(parameters, CONFIG_SCHEMA)

if not result.valid:
    raise ValidationError(result.errors)
```

### Step 5: RYE Resolves Environment (for Runtimes)

```python
# For each runtime in chain:
for runtime in [tool for tool in chain if tool["__tool_type__"] == "runtime"]:
    env_config = runtime.get("ENV_CONFIG")
    resolved_env = env_resolver.resolve(env_config)

    # Merge into parameters
    parameters = {**parameters, **resolved_env}
```

### Step 6: RYE Expands Templates

```python
# For each config in chain:
for tool in chain:
    config = tool.get("CONFIG", {})
    expanded_config = resolve_templates(config, resolved_env)
    # ${RYE_PYTHON} -> /usr/bin/python3
```

### Step 7: RYE Calls Lilux Primitive

```python
# Execute primitive (last in chain)
primitive_name = chain[-1]["__executor_id__"]  # "subprocess"
primitive_module = importlib.import_module(f"lilux.primitives.{primitive_name}")

# Call Lilux primitive with resolved config
result = await primitive_module.execute(
    config=expanded_config,
    parameters=parameters
)
```

### Step 8: Lilux Executes

```python
# In Lilux SubprocessPrimitive
async def execute(self, config: Dict, params: Dict) -> SubprocessResult:
    command = config.get("command")
    args = config.get("args", [])
    env = config.get("env", {})

    # Execute shell command
    process = await asyncio.create_subprocess_exec(
        command, args, env=env, ...
    )

    return SubprocessResult(
        success=process.returncode == 0,
        stdout=process.stdout.decode(),
        stderr=process.stderr.decode(),
        returncode=process.returncode,
        duration_ms=elapsed_time
    )
```

## Summary

| Layer | Responsibility | Example |
|--------|----------------|----------|
| **LLM** | Initiates request | Calls `execute()` |
| **RYE** | Orchestrates | Loads, validates, resolves env, routes |
| **Lilux** | Executes | `SubprocessPrimitive.execute()` |
| **System** | Runs process | OS shell, HTTP, filesystem |

---

## Related Documentation

- **Executor Routing:** `[[rye/executor/routing]]`
- **Primitives Overview:** `[[lilux/primitives/overview]]`
- **Runtime Services:** `[[lilux/runtime-services/overview]]`
- **Lockfiles:** `[[lilux/primitives/lockfile]]`
