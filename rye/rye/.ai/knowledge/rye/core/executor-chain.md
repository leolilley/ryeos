<!-- rye:signed:2026-02-18T01:03:26Z:41e161392bd02d25ac23a081219df7c36d596e500dfbcad5c23f38b62aae3268:dhHS5-m_iVexWyWeI2a1Huo9NdDeaycINmS2BEIfhJwP3OqDfX59GqJKXDYoC49_NFl66Tzbb9NSKO22Su1hCg==:440443d0858f0199 -->

```yaml
id: executor-chain
title: Executor Chain
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - executor
  - chain
  - tools
  - runtime
  - primitives
references:
  - three-tier-spaces
  - templating-systems
  - "docs/internals/executor-chain.md"
```

# Executor Chain

How tools are resolved and executed through the three-layer chain.

## The Three Layers

```
Layer 3: Tool        __executor_id__ = "rye/core/runtimes/python_script_runtime"
                              │
Layer 2: Runtime     __executor_id__ = "rye/core/primitives/subprocess"
                              │
Layer 1: Primitive   __executor_id__ = None  →  direct Lilux execution
```

### Layer 1: Primitives

Terminal nodes. `__executor_id__ = None`. Map to Lilux classes via `PRIMITIVE_MAP`:

```python
PRIMITIVE_MAP = {
    "rye/core/primitives/subprocess": SubprocessPrimitive,
    "rye/core/primitives/http_client": HttpClientPrimitive,
}
```

No file needed — resolved by key matching.

### Layer 2: Runtimes

YAML configs at `.ai/tools/rye/core/runtimes/`. Point to a primitive. Add configuration: interpreter resolution, command templates, timeout, anchor setup, dependency verification.

```yaml
tool_type: runtime
executor_id: rye/core/primitives/subprocess

env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON
    fallback: python3

config:
  command: "${RYE_PYTHON}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300
```

Available runtimes:

| Runtime                   | Language/Protocol | Execution Method                   |
| ------------------------- | ----------------- | ---------------------------------- |
| `python_script_runtime`   | Python            | Subprocess with venv resolution    |
| `python_function_runtime` | Python            | In-process import + call           |
| `node_runtime`            | JavaScript        | Subprocess with node resolution    |
| `bash_runtime`            | Bash              | Subprocess                         |
| `mcp_stdio_runtime`       | MCP (stdio)       | Launch MCP server, call via stdio  |
| `mcp_http_runtime`        | MCP (HTTP)        | Connect to MCP server via HTTP     |

### Layer 3: Tools

Python scripts, shell scripts, or other executables with metadata headers:

```python
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__version__ = "1.0.0"
__tool_type__ = "python"
__category__ = "rye/bash"
```

## Chain Building Algorithm

`PrimitiveExecutor._build_chain(item_id)`:

1. **Cache check** — if cached chain exists and all file hashes match, return cached
2. **Resolve path** — `_resolve_tool_path(item_id)` using three-tier space precedence
3. **Load metadata** — parse via AST (Python) or YAML to extract `__executor_id__`, `ENV_CONFIG`, `CONFIG`, `anchor`, `verify_deps`
4. **Create ChainElement** — store item_id, path, space, extracted metadata
5. **Check termination** — if `executor_id is None`, this is a primitive; stop
6. **Recurse** — set `current_id = executor_id`, repeat from step 2
7. **Cache result** — store chain with combined SHA-256 hash of all chain files

### Safety Guards

| Guard              | Limit | Effect                              |
| ------------------ | ----- | ----------------------------------- |
| Max depth          | 10    | `MAX_CHAIN_DEPTH` prevents nesting  |
| Circular detection | —     | `visited` set catches A → B → A     |
| Missing executor   | —     | `ValueError` if intermediate not found |

Result ordering: `[tool, runtime, ..., primitive]` — tool at index 0, primitive last.

## Concrete Example

```
Step 1: Resolve "rye/bash/bash"
  → .ai/tools/rye/bash/bash.py (system space)
  → __executor_id__ = "rye/core/runtimes/python_script_runtime"

Step 2: Resolve "rye/core/runtimes/python_script_runtime"
  → .ai/tools/rye/core/runtimes/python_script_runtime.yaml (system space)
  → executor_id = "rye/core/primitives/subprocess"

Step 3: Resolve "rye/core/primitives/subprocess"
  → Matches PRIMITIVE_MAP key (no file needed)
  → executor_id = None (terminal)

Chain: [bash.py, python_script_runtime.yaml, subprocess primitive]
```

## Chain Validation

`ChainValidator.validate_chain()` checks every adjacent pair `(child, parent)`:

### Space Compatibility

```
SPACE_PRECEDENCE = {"project": 3, "user": 2, "system": 1}
```

**Rule:** A child can only depend on elements with equal or lower precedence.

| Child Space | Can Depend On         |
| ----------- | --------------------- |
| project (3) | project, user, system |
| user (2)    | user, system          |
| system (1)  | system only           |

### I/O Compatibility

Parent's required inputs must be a subset of child's outputs. Missing declarations treated as compatible.

### Version Constraints

Parent can specify `child_constraints` with `min_version`/`max_version`. Checked via semver comparison (`packaging` library).

## ENV_CONFIG Resolution

After validation, `_resolve_chain_env()` merges env vars from all chain elements:

1. Process chain in **reverse** order (primitive → runtime → tool)
2. For each element with `env_config`, call `EnvResolver.resolve()`:
   - Load `.env` from project root
   - Resolve interpreter path (venv, node_modules, system binary, version manager)
   - Apply static env vars with `${VAR:-default}` expansion
3. Merge into accumulated environment (later elements override)

Result: fully-resolved environment dict passed to the Lilux primitive.

## Anchor System

Runtimes can declare an `anchor` config for module resolution:

```yaml
anchor:
  enabled: true
  mode: auto        # auto, always, or never
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir    # tool_dir, tool_parent, project_path
  lib: lib/python
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]
```

When active:
1. Check marker files in tool's directory (`mode: auto`)
2. Resolve anchor root path
3. Prepend/append to env vars (e.g., `PYTHONPATH`)
4. Run `verify_deps` — walk anchor directory, call `verify_item()` on all matching files

## Execution Config Building

`_build_execution_config()` merges configs from the entire chain:

1. Start from primitive, merge upward (tool configs override runtime configs)
2. Inject execution context: `tool_path`, `project_path`, `user_space`, `system_space`
3. Serialize parameters as `params_json`
4. Two-pass templating:
   - Pass 1: `${VAR}` — env var substitution (shell-escaped via `shlex.quote`)
   - Pass 2: `{param}` — config value substitution (iterates up to 3 times until stable)

## Caching

| Cache             | Key                                          | Invalidation                               |
| ----------------- | -------------------------------------------- | ------------------------------------------ |
| `_chain_cache`    | item_id → `CacheEntry(chain, combined_hash)` | Combined SHA-256 of all chain files        |
| `_metadata_cache` | file path → `CacheEntry(metadata, file_hash)` | SHA-256 of file content                   |

Cache invalidation is automatic: if any file hash differs, the cached entry is discarded and rebuilt.

## Validated Pairs (Dry Run Output)

```json
{
  "status": "validation_passed",
  "chain": ["rye/file-system/write", "rye/core/runtimes/python_script_runtime", "rye/core/primitives/subprocess"],
  "validated_pairs": [
    {"child": "rye/file-system/write", "parent": "python_script_runtime", "space_ok": true, "io_ok": true},
    {"child": "python_script_runtime", "parent": "subprocess", "space_ok": true, "io_ok": true}
  ]
}
```
