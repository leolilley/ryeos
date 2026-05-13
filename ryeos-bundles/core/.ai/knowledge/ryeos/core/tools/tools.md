---
category: ryeos/core
tags: [fundamentals, tools, execution, subprocess]
version: "1.0.0"
description: >
  How tools work — executable scripts, the subprocess model,
  executor chains, and runtime environments.
---

# Tools

Tools are the primary executable unit in Rye OS. A tool is a script
(Python, Bash, JavaScript) or YAML descriptor that receives JSON
parameters and returns results.

## File Formats

Tools can be authored in multiple formats:

| Format   | Extension   | Parser                      |
|----------|-------------|-----------------------------|
| Python   | `.py`       | `parser:ryeos/core/python/ast` |
| YAML     | `.yaml`     | `parser:ryeos/core/yaml/yaml` |
| JS/TS    | `.js`, `.ts` | `parser:ryeos/core/javascript/javascript` |

Python tools extract metadata from dunder constants (`__version__`,
`__category__`, etc.) via AST parsing. YAML tools have metadata
inline. All formats produce the same normalized metadata shape.

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
"""Tool description."""
__version__ = "1.0.0"
__category__ = "my/project"
__tool_type__ = "python"

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

The core bundle provides three tool runtimes:

### Bash (`ryeos/core/runtimes/bash`)
Runs shell commands via `/bin/bash -c`. Input: `command` string.

### Python Function (`ryeos/core/runtimes/python/function`)
Loads a Python module, calls its `execute(params, project_path)`
function. Resolves interpreter from `.venv/bin/python3` or `RYE_PYTHON`.
Adds `{tool_dir}` and `{runtime_dir}/lib` to `PYTHONPATH`.

### Python Script (`ryeos/core/runtimes/python/script`)
Runs Python scripts that manage their own `__main__` entry point.
Same interpreter resolution as function runtime.

## Streaming Tools

`streaming_tool` is a variant that emits length-prefixed JSON frames
on stdout during execution. Used for long-running tools that need
to report progress incrementally.

## Environment Variables

All subprocess tools receive these environment variables:

| Variable              | Description                        |
|-----------------------|------------------------------------|
| `RYE_THREAD_ID`       | Current thread ID                  |
| `RYE_PROJECT_PATH`    | Absolute path to project root      |

Runtime subprocesses additionally receive:

| Variable                    | Description                        |
|-----------------------------|------------------------------------|
| `RYEOSD_SOCKET_PATH`        | Daemon Unix socket path            |
| `RYEOSD_CALLBACK_TOKEN`     | Auth token for callback channel    |
| `RYEOSD_THREAD_ID`          | Thread ID (redundant with RYE_)    |
| `RYEOSD_PROJECT_PATH`       | Project path (redundant with RYE_) |
| `RYEOSD_THREAD_AUTH_TOKEN`  | Thread-specific auth token         |

## Dependency Scanning

Python tools automatically have their dependencies scanned for
CAS tracking. The scanner walks `.py`, `.yaml`, `.yml`, `.json`
files in the tool directory (excluding `__pycache__`, `.venv`,
`node_modules`, `.git`, `config`).
