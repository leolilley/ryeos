---
category: ryeos/core
tags: [reference, runtimes, execution, subprocess]
version: "1.0.0"
description: >
  Tool runtimes — bash, python/function, python/script, and
  state-graph. How tools are actually executed.
---

# Runtimes

Runtimes are the execution environments that run tools and directives.
The core bundle provides tool runtimes; the standard bundle provides
directive and graph runtimes.

## Tool Runtimes (Core Bundle)

### Bash (`tool:ryeos/core/runtimes/bash`)
Runs shell commands directly.

- **Command:** `/bin/bash -c "{command}"`
- **Input:** `command` (string, required)
- **Timeout:** 300s (configurable via execution config)
- **Use case:** Quick shell operations, one-liners

### Python Function (`tool:ryeos/core/runtimes/python/function`)
Loads a Python module and calls its `execute(params, project_path)`.

- **Interpreter:** `.venv/bin/python3` → `RYE_PYTHON` → `python3`
- **PYTHONPATH:** `{tool_dir}`, `{runtime_dir}/lib`
- **Async support:** Yes (auto-detects and wraps with `asyncio.run`)
- **Timeout:** 300s
- **Use case:** Structured Python tools with function entry point

### Python Script (`tool:ryeos/core/runtimes/python/script`)
Runs a Python script as `__main__`.

- **Command:** `{interpreter} {tool_path} --project-path {project_path}`
- **Same interpreter and PYTHONPATH resolution as function runtime**
- **Timeout:** 300s
- **Use case:** Self-contained Python scripts

### State Graph (`tool:ryeos/core/runtimes/state-graph`)
The most complex runtime — walks YAML state machine definitions.

- **Execution owner:** `callee` (manages own subprocess)
- **Async:** Yes (`native_async: true`)
- **Resume:** Yes (`native_resume: true`)
- **Timeout:** 600s
- **Features:**
  - Node-by-node graph traversal
  - Conditional edges (`when` conditions)
  - Foreach iteration (sequential or parallel)
  - State persistence (CAS snapshots)
  - Transcript logging (JSONL)
  - Knowledge rendering (signed markdown)
  - Hook evaluation (conditions + actions)
  - Permission enforcement per node
  - Retry and error handling (fail/continue modes)
  - Cancellation (SIGTERM)

## Execution Config

All runtimes inherit defaults from `config:execution/execution`:

| Setting                  | Default  |
|--------------------------|----------|
| `timeout`                | 300s     |
| `max_steps`              | 100      |
| `max_concurrency`        | 10       |
| `cancellation_mode`      | graceful |
| `cancellation_grace_secs`| 5        |

Override at project level in `.ai/config/execution/execution.yaml`
or user level in `~/.ai/config/execution/execution.yaml`.

## The `@subprocess` Alias

When a tool declares `executor_id: "@subprocess"`, it resolves to
`tool:ryeos/core/subprocess/execute`. This is the terminal subprocess
spawner that actually forks and execs the configured command.

The chain looks like:
```
Tool action → @subprocess → subprocess/execute → fork + exec → result
```

## Runtime Helpers (Python)

The `python/lib/` directory provides shared libraries:

### `condition_evaluator.py`
Evaluates conditions for graph edges and hooks:
- Combinators: `any`, `all`, `not`
- Operators: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `contains`, `regex`, `exists`
- Path resolution: dotted paths with bracket indices

### `interpolation.py`
Template variable interpolation:
- `${path.to.value}` — context path resolution
- `{input:name}` — input parameter reference
- `||` fallback chains
- Pipe filters: `json`, `from_json`, `length`, `keys`, `upper`, `lower`

### `module_loader.py`
Python module loading with proper `sys.modules` registration so
relative imports work across subdirectories.
