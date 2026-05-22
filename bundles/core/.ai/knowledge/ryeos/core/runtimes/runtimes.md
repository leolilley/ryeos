<!-- ryeos:signed:2026-05-22T04:30:07Z:75e604db730ce8b39d54054658db8c38fd303a6112efdc7e83f1c9ba53637033:p8mrnCPJXDBjAU5HOze59HUJ/+9UjiITKEuWJr60NarzYbGyhPsX4t7Akjevw4XnM+C4gKE2Ko0LM3/naTNDCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, runtimes, execution, subprocess]
version: "1.0.0"
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

- **Interpreter:** `.venv/bin/python3` → `RYE_PYTHON` → `python3`
- **PYTHONPATH:** `{tool_dir}`
- **Async support:** Yes (auto-detects and wraps with `asyncio.run`)
- **Timeout:** 300s
- **Use case:** Structured Python tools with function entry point

### Python Script (`tool:ryeos/core/runtimes/python/script`)
Runs a Python script as `__main__`.

- **Command:** `{interpreter} {tool_path} --project-path {project_path}`
- **Same interpreter and PYTHONPATH resolution as function runtime**
- **Timeout:** 300s
- **Use case:** Self-contained Python scripts

Shell commands run through `tool:ryeos/core/subprocess/execute`, and
graph workflows run through `runtime:graph-runtime` in the standard bundle.

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
or user level in `~/.ryeos/.ai/config/execution/execution.yaml`.

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
