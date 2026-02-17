```yaml
id: executor-chain
title: "Executor Chain"
description: How tools are resolved and executed through the three-layer chain
category: internals
tags: [executor, chain, primitive, runtime, resolution]
version: "1.0.0"
```

# Executor Chain

The executor chain is how Rye routes a tool call from an AI agent down to an OS-level operation. Every tool declares an `__executor_id__` that points to the next element in the chain. The chain terminates at a primitive, where `__executor_id__` is `None`.

## The Three Layers

```
Layer 3: Tool          __executor_id__ = "rye/core/runtimes/python_script_runtime"
                                │
Layer 2: Runtime       __executor_id__ = "rye/core/primitives/subprocess"
                                │
Layer 1: Primitive     __executor_id__ = None  →  direct Lilux execution
```

### Layer 1: Primitives

Primitives are the terminal nodes. They have `__executor_id__ = None` and map directly to Lilux classes via `PrimitiveExecutor.PRIMITIVE_MAP`:

```python
PRIMITIVE_MAP = {
    "rye/core/primitives/subprocess": SubprocessPrimitive,
    "rye/core/primitives/http_client": HttpClientPrimitive,
}
```

### Layer 2: Runtimes

Runtimes are YAML configs in `.ai/tools/rye/core/runtimes/`. They point to a primitive and add configuration: interpreter resolution via `ENV_CONFIG`, command templates, timeout, anchor setup, and dependency verification.

Example — `python_script_runtime.yaml`:

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

### Layer 3: Tools

Tools are Python scripts, shell scripts, or other executables with metadata headers. They point to a runtime:

```python
__executor_id__ = "rye/core/runtimes/python_script_runtime"
```

## Chain Building Algorithm

`PrimitiveExecutor._build_chain(item_id)` resolves a chain by following `__executor_id__` recursively:

1. **Cache check** — If the chain is cached and all file hashes still match, return cached chain.
2. **Resolve path** — Call `_resolve_tool_path(item_id)` to find the file using three-tier space precedence (project → user → system).
3. **Load metadata** — Parse the file using AST (Python) or YAML to extract `__executor_id__`, `ENV_CONFIG`, `CONFIG`, `anchor`, `verify_deps`, etc.
4. **Create ChainElement** — Store the item_id, path, space, and all extracted metadata.
5. **Check termination** — If `executor_id` is `None`, this is a primitive; stop.
6. **Recurse** — Set `current_id = executor_id` and repeat from step 2.
7. **Cache result** — Store the chain with a combined SHA256 hash of all chain files.

Safety guards:

- **Max depth**: `MAX_CHAIN_DEPTH = 10` — prevents runaway nesting
- **Circular detection**: A `visited` set catches `A → B → A` cycles
- **Missing executor**: If an intermediate executor is not found, raises `ValueError`

The resulting chain is ordered `[tool, runtime, ..., primitive]` — the tool is at index 0, the primitive at the last index.

## Concrete Example

Executing the bash tool `rye/bash/bash`:

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

Before execution, `ChainValidator.validate_chain()` checks every adjacent pair `(child, parent)` in the chain:

### Space Compatibility

Each space has a precedence number: project=3, user=2, system=1.

**Rule**: A child can only depend on elements with equal or lower precedence.

| Child Space | Can Depend On         |
| ----------- | --------------------- |
| project (3) | project, user, system |
| user (2)    | user, system          |
| system (1)  | system only           |

A user-space tool cannot depend on a project-space runtime — that would make the user tool break when used in a different project.

Additionally, the validator checks for "system → mutable" transitions within the chain. A system tool delegating to a project or user tool is always invalid.

### I/O Compatibility

If both child and parent declare input/output types, the parent's required inputs must be a subset of the child's outputs. Missing declarations are allowed (treated as compatible).

### Version Constraints

A parent element can specify `child_constraints` with `min_version` and `max_version`. The child's `__version__` is checked against these constraints using semver comparison via the `packaging` library.

## Environment Resolution

After chain validation, `_resolve_chain_env()` merges environment variables from all chain elements:

1. Process the chain in **reverse** order (primitive → runtime → tool)
2. For each element with `env_config`, call `EnvResolver.resolve()`:
   - Load `.env` file from project root
   - Resolve interpreter path (venv, node_modules, system binary, or version manager)
   - Apply static env vars with `${VAR:-default}` expansion
3. Merge into the accumulated environment (later elements override)

The result is a fully-resolved environment dict passed to the Lilux primitive.

## Anchor System

Runtimes can declare an `anchor` config for module resolution:

```yaml
anchor:
  enabled: true
  mode: auto # auto, always, or never
  markers_any: ["__init__.py", "pyproject.toml"] # activation markers
  root: tool_dir # anchor root: tool_dir, tool_parent, project_path
  lib: lib/python # runtime library path
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]
```

When active, the anchor system:

1. Checks if marker files exist in the tool's directory (`mode: auto`)
2. Resolves the anchor root path based on `root` config
3. Prepends/appends paths to environment variables like `PYTHONPATH`
4. Runs dependency verification (`verify_deps`) — walks the anchor directory and calls `verify_item()` on every matching file before subprocess spawn

This allows tools with multi-file dependencies (e.g., a tool with a `lib/` directory) to have their entire dependency tree verified and their module paths set up correctly.

## Execution Config Building

`_build_execution_config()` merges configs from the entire chain:

1. Start from the primitive and merge configs upward (tool configs override runtime configs)
2. Inject execution context: `tool_path`, `project_path`, `user_space`, `system_space`
3. Serialize runtime parameters as `params_json`
4. Run two-pass templating:
   - Pass 1: `${VAR}` — environment variable substitution (with shell escaping via `shlex.quote`)
   - Pass 2: `{param}` — config value substitution (iterates up to 3 times until stable)

## Caching

Two caches with hash-based invalidation:

| Cache             | Key                                           | Invalidation                                              |
| ----------------- | --------------------------------------------- | --------------------------------------------------------- |
| `_chain_cache`    | item_id → `CacheEntry(chain, combined_hash)`  | Combined SHA256 of all chain files — recomputed on lookup |
| `_metadata_cache` | file path → `CacheEntry(metadata, file_hash)` | SHA256 of file content — recomputed on lookup             |

Cache invalidation is automatic: if any file in the chain changes (content hash differs), the cached entry is discarded and the chain is rebuilt from the filesystem.

## Lockfile Verification

Before building the chain, `PrimitiveExecutor.execute()` checks for an existing lockfile:

1. Load lockfile via `LockfileResolver.get_lockfile(item_id, version)`
2. Verify root integrity: compute current hash of the tool file, compare to `lockfile.root.integrity`
3. Verify chain elements: for each entry in `lockfile.resolved_chain`, resolve the path and compare its current hash to the pinned `integrity` value
4. **Any mismatch** → return `ExecutionResult(success=False)` with message to re-sign and delete stale lockfile

After successful execution (if no lockfile existed), a new lockfile is created with `LockfileResolver.create_lockfile()` and saved to the project space.

Lockfile format (`{tool_id}@{version}.lock.json`):

```json
{
  "lockfile_version": 1,
  "generated_at": "2026-02-15T12:00:00+00:00",
  "root": {
    "tool_id": "rye/bash/bash",
    "version": "1.0.0",
    "integrity": "a1b2c3..."
  },
  "resolved_chain": [
    { "item_id": "rye/bash/bash", "space": "system", "integrity": "a1b2c3..." },
    {
      "item_id": "rye/core/runtimes/python_script_runtime",
      "space": "system",
      "integrity": "d4e5f6..."
    }
  ]
}
```
