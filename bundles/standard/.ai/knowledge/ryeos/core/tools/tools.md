<!-- ryeos:signed:2026-07-14T10:12:30Z:e21e7d28c21b0eb7dca89fabf7df16317dc5c81cba77151d0876127a83a72b50:O4jcwwSgZBMKvuLGZAHDzMrHeVWdrDOQ6aTp4ckoZGcVwAZvc7AIj2HiJIFUmeg6a9z/XgdiWqV0Wxh8susuDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [fundamentals, tools, execution, subprocess]
version: "1.0.0"
description: >
  How tools work — executable scripts, the subprocess model,
  executor chains, and runtime environments.
---

# Tools

Tools are a generic executable unit in Rye OS. A tool is a Python,
JavaScript/TypeScript, JSON, or YAML descriptor that receives JSON
parameters and returns results.

## File Formats

Tools can be authored in multiple formats:

| Format   | Extension   | Parser                      |
|----------|-------------|-----------------------------|
| Python   | `.py`       | `parser:ryeos/core/python/tool-header` |
| YAML     | `.yaml`     | `parser:ryeos/core/yaml/yaml` |
| JS/TS    | `.js`, `.ts` | `parser:ryeos/core/javascript/javascript` |

Python tools extract metadata from a `# ryeos-tool:` comment-YAML header.
YAML tools have metadata inline. All formats produce the same normalized
metadata shape.

## Tool Metadata

Every tool declares:

| Field             | Purpose                                       |
|-------------------|-----------------------------------------------|
| `category`        | Namespaced category (e.g., `ryeos/core`)      |
| `version`         | Semantic version                              |
| `description`     | What the tool does                            |
| `executor_id`     | How to run it (usually `@subprocess`)         |
| `required_caps`   | Capabilities needed to invoke                 |
| `config_schema`   | Input parameter schema                        |
| `config`          | Execution config (command, env, timeout)      |

## The Subprocess Model

Most tools use `executor_id: "@subprocess"`, which resolves to
`tool:ryeos/core/subprocess/execute`. The execution chain:

```
directive action → @subprocess → tool:ryeos/core/subprocess/execute → fork+exec
```

The subprocess executor:
1. Receives the tool's `config.command` template
2. Interpolates parameters (`{tool_dir}`, `{project_path}`, etc.)
3. Injects the params JSON on stdin
4. Forks and execs the command
5. Captures stdout as the result
6. Enforces timeout (default 300s)

## Python Tool Anatomy

```python
#!/usr/bin/env python3
# ryeos-tool:
#   category: my/project
#   version: "1.0.0"
#   tool_type: python
#   executor_id: "tool:ryeos/core/runtimes/python/function"
#   description: "Tool description."
"""Tool description."""

def execute(params, project_path):
    """Execute the tool. params is a dict from the caller."""
    name = params.get("name", "world")
    return {"greeting": f"Hello, {name}!"}
```

For async tools:
```python
async def execute(params, project_path):
    result = await some_async_work(params)
    return result
```

## YAML Tool Anatomy

```yaml
category: my/project
version: "1.0.0"
description: Run a shell command
executor_id: "@subprocess"
config_schema:
  command:
    type: string
    required: true
config:
  command: '/bin/bash -c "{command}"'
  timeout_secs: 300
```

## Runtime Environments

The active core tool runtime descriptors are Python function and Python script.
Shell commands are represented as YAML tools that use the subprocess executor;
there is no separate Bash runtime descriptor.

### Python Function (`ryeos/core/runtimes/python/function`)
Loads a Python module, calls its `execute(params, project_path)`
function. Resolves interpreter from `.venv/bin/python3` or `RYE_PYTHON`.
Prepends runtime-derived bundle-local roots to `sys.path` without setting
`PYTHONPATH`.

### Python Script (`ryeos/core/runtimes/python/script`)
Runs Python scripts that manage their own `__main__` entry point.
Same interpreter resolution as function runtime.

## Streaming Tools

`streaming_tool` is a variant that emits length-prefixed JSON frames
on stdout during execution. Used for long-running tools that need
to report progress incrementally.

## Environment Variables

The default `tool_callback_v1` and callback-free streaming protocol both
declare the basic tool variables:

| Variable              | Description                        |
|-----------------------|------------------------------------|
| `RYE_THREAD_ID`       | Current thread ID                  |
| `RYE_PROJECT_PATH`    | Absolute path to project root      |

`tool_callback_v1` additionally declares these callback bindings (the managed
`runtime_v1` protocol uses the same callback names):

| Variable                    | Description                        |
|-----------------------------|------------------------------------|
| `RYEOSD_SOCKET_PATH`        | Daemon Unix socket path            |
| `RYEOSD_CALLBACK_TOKEN`     | Auth token for callback channel    |
| `RYEOSD_THREAD_ID`          | Thread ID (redundant with RYE_)    |
| `RYEOSD_PROJECT_PATH`       | Callback authorization/state anchor; may differ from `RYE_PROJECT_PATH` under a state-root override |
| `RYEOSD_THREAD_AUTH_TOKEN`  | Thread-specific auth token         |

Environment is protocol-authoritative. The default tool protocol explicitly
requests callback credentials so manifest-backed bundle-event, vault, and item-
authoring operations work; empty effective capabilities deny capability-gated
resource operations unless verified authority grants them. Exact-thread and
chain-local lifecycle methods retain their documented access class. A schema selecting callback-free
`opaque`, `tool_streaming_v1`, or `cli_exec` receives no undeclared `RYEOSD_*`
credentials and no daemon-socket access inside an enforced sandbox.

## Dependency Scanning

Python tools automatically have their dependencies scanned for
CAS tracking. The scanner walks `.py`, `.yaml`, `.yml`, `.json`
files in the tool directory (excluding `__pycache__`, `.venv`,
`node_modules`, `.git`, `config`).
