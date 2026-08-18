<!-- ryeos:signed:2026-08-18T22:04:51Z:10b72249a5ccc04e0552ab08ecd4a55999a74bbfca962afe53e4b7ef7420a8fe:Cid0U3WNlzQy5yX7w64qLO2zD+r9XPapj98DagvKHBOYsT641ObQgumOzz1cL8xES1iabVrjkuiAoZ4FrsPCDw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, runtimes, execution, subprocess]
version: "1.1.0"
description: >
  Active tool runtime descriptors and subprocess execution helpers.
---

# Runtimes

Runtimes are execution environments. Core provides generic tool helper
descriptors and the subprocess executor; standard provides directive, graph,
and knowledge runtime binaries.

## Tool Runtimes (Core Bundle)

### Python Function (`tool:ryeos/core/runtimes/python/function`)
Loads a Python module and calls its `execute(params, project_path)`.

- **Interpreter:** `RYE_PYTHON` override → engine-resolved `python3`. Project
  virtualenvs are not an implicit execution dependency.
- **Imports:** prepends runtime-derived bundle-local roots to `sys.path`
- **Async support:** Yes (auto-detects and wraps with `asyncio.run`)
- **Timeout:** 300s
- **Use case:** Structured Python tools with function entry point

### Python Script (`tool:ryeos/core/runtimes/python/script`)
Runs a Python script as `__main__`.

- **Command:** runtime launcher invokes `${tool_path}` as `__main__` with `--project-path ${project_path}`
- **Same interpreter and bundle-local `sys.path` setup as function runtime**
- **Timeout:** 300s
- **Use case:** Self-contained Python scripts

Shell commands run through `tool:ryeos/core/subprocess/execute`, and
graph workflows run through `runtime:graph-runtime` in the standard bundle.

## Execution Config

All runtimes inherit defaults from `config:execution/execution`:

| Setting                  | Default       |
|--------------------------|---------------|
| `timeout`                | 86400s (1 day)|
| `max_steps`              | 100           |
| `max_concurrency`        | 10            |
| `cancellation_mode`      | graceful      |
| `cancellation_grace_secs`| 5             |

Override at project level in `.ai/config/execution/execution.yaml`.

Per-item overrides are kind-keyed so the policy format extends without an
engine code change:

```yaml
items:
  tool:
    my/project/action:
      timeout: 600
      max_steps: 20
```

## The `@subprocess` Alias

When a tool declares `executor_id: "@subprocess"`, it resolves to
`tool:ryeos/core/subprocess/execute`. This is the terminal subprocess
spawner that actually forks and execs the configured command.

The chain looks like:
```
Tool action → @subprocess → subprocess/execute → fork + exec → result
```

## Runtime implementation ownership

Active interpolation, conditions, graph traversal, and resume behavior
live in Rust runtimes and engine crates.
