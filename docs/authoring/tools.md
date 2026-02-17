---
id: tools
title: "Authoring Tools"
description: How to write tool files — executable scripts that do the actual work
category: authoring
tags: [tools, authoring, python, yaml, format]
version: "1.0.0"
---

# Authoring Tools

Tools are executable scripts that **do the actual work** — read files, run commands, make HTTP requests, deploy services. They live in `.ai/tools/` and are called by directives (or directly by agents).

## The Executor Chain

Tools don't run on their own. They declare an executor, which determines the runtime:

```
tool → runtime → primitive
```

For example:
- `write.py` → `rye/core/runtimes/python_function_runtime` → subprocess
- `bash.py` → `rye/core/runtimes/python_script_runtime` → subprocess
- `query-docs.yaml` → `rye/core/runtimes/mcp_http_runtime` → HTTP

The `__executor_id__` (Python) or `executor_id` (YAML) field declares which runtime runs the tool.

## Python Tools — The Primary Format

Python tools are the most common. They use module-level metadata variables and a standard `execute()` entry point.

### Required Structure

```python
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
"""Brief description of what this tool does."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "category/path"
__tool_description__ = "What this tool does"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "param_name": {
            "type": "string",
            "description": "What this param does",
        },
    },
    "required": ["param_name"],
}


def execute(params: dict, project_path: str) -> dict:
    """Main execution function."""
    # Implementation
    return {"success": True, "data": result}
```

### Metadata Variables

| Variable | Purpose | Example |
|----------|---------|---------|
| `__version__` | Semantic version of the tool | `"1.0.0"` |
| `__tool_type__` | Tool classification | `"python"` |
| `__executor_id__` | Runtime that executes this tool | `"rye/core/runtimes/python_function_runtime"` |
| `__category__` | Directory path within `.ai/tools/` | `"rye/file-system"` |
| `__tool_description__` | Human-readable description | `"Create or overwrite a file"` |

### CONFIG_SCHEMA

Defines the JSON Schema for accepted parameters. This is validated by the runtime before calling `execute()`:

```python
CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {
            "type": "string",
            "description": "Path to file (relative to project root or absolute)",
        },
        "content": {
            "type": "string",
            "description": "Content to write to the file",
        },
        "timeout": {
            "type": "integer",
            "description": "Timeout in seconds (default: 120)",
            "default": 120,
        },
    },
    "required": ["file_path", "content"],
}
```

### The `execute()` Function

The entry point. Always takes `params` dict and `project_path` string, returns a dict:

```python
def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["file_path"])

    # Resolve relative paths
    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    # Security: check path is inside project
    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    try:
        # Do the work
        result = do_something(file_path)
        return {"success": True, "output": result}
    except Exception as e:
        return {"success": False, "error": str(e)}
```

The function can be sync or async — both are supported.

### CLI Fallback

Tools also support direct CLI execution via `__main__`:

```python
if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
```

### Common Executor IDs

| Executor ID | When to Use |
|-------------|------------|
| `rye/core/runtimes/python_function_runtime` | Pure Python — imported and called in-process |
| `rye/core/runtimes/python_script_runtime` | Needs subprocess isolation (shell commands, heavy I/O) |

## YAML Tools — Configuration-Driven

YAML tools are used for configuration-driven tools, particularly MCP tool definitions:

```yaml
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
tool_id: category/tool_name
tool_type: yaml
version: "1.0.0"
executor_id: rye/core/runtimes/python_script_runtime
category: category/path
description: What this tool does
parameters:
  - name: param_name
    type: string
    required: true
    description: What this param does
  - name: optional_param
    type: integer
    required: false
    default: 10
config:
  key: value
```

### MCP Tool Definitions (YAML)

MCP tools wrap external MCP servers. They define the server connection and input schema:

```yaml
# rye:signed:2026-02-04T23:57:39Z:placeholder:unsigned:unsigned
tool_type: mcp
executor_id: rye/core/runtimes/mcp_http_runtime
category: mcp/context7
version: 1.0.0
description: 'Retrieves documentation from Context7 for any library.'
config:
  server: mcp/servers/context7
  tool_name: query-docs
input_schema:
  type: object
  properties:
    libraryId:
      type: string
      description: Context7-compatible library ID
    query:
      type: string
      description: The question or task
  required:
    - libraryId
    - query
```

### MCP Server Definitions (YAML)

MCP servers are referenced by MCP tool definitions:

```yaml
# rye:signed:2026-02-04T23:57:39Z:placeholder:unsigned:unsigned
tool_type: mcp_server
executor_id: null
category: mcp/servers
version: 1.0.0
description: 'MCP server: context7'
config:
  transport: http
  timeout: 30
  url: https://mcp.context7.com/mcp
```

## Tool Resolution

Tools are resolved by item_id, which maps to the file path:

```
item_id: "rye/file-system/write"
  → .ai/tools/rye/file-system/write.py

item_id: "mcp/context7/query-docs"
  → .ai/tools/mcp/context7/query-docs.yaml

item_id: "rye/bash/bash"
  → .ai/tools/rye/bash/bash.py
```

The category determines the directory path within `.ai/tools/`.

## Real Examples

### File Write Tool

From `rye/rye/.ai/tools/rye/file-system/write.py`:

```python
# rye:signed:2026-02-15T07:11:41Z:e972...:S2FT...==:440443d0858f0199
"""Create or overwrite a file, invalidating line ID cache."""

import argparse
import hashlib
import json
import sys
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"
__category__ = "rye/file-system"
__tool_description__ = "Create or overwrite a file"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "file_path": {
            "type": "string",
            "description": "Path to file (relative to project root or absolute)",
        },
        "content": {
            "type": "string",
            "description": "Content to write to the file",
        },
    },
    "required": ["file_path", "content"],
}


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["file_path"])
    content = params["content"]

    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    created = not file_path.exists()
    try:
        file_path.parent.mkdir(parents=True, exist_ok=True)
        file_path.write_text(content)
        return {
            "success": True,
            "file_path": str(file_path.relative_to(project)),
            "bytes_written": len(content),
            "created": created,
        }
    except Exception as e:
        return {"success": False, "error": str(e)}


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
```

### Bash Tool

From `rye/rye/.ai/tools/rye/bash/bash.py`:

```python
# rye:signed:2026-02-15T07:32:49Z:5d4a...
"""Execute shell commands."""

import subprocess
from pathlib import Path

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "rye/bash"
__tool_description__ = "Execute shell commands"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "command": {
            "type": "string",
            "description": "Shell command to execute",
        },
        "timeout": {
            "type": "integer",
            "description": "Timeout in seconds (default: 120)",
            "default": 120,
        },
        "working_dir": {
            "type": "string",
            "description": "Working directory (default: project root)",
        },
    },
    "required": ["command"],
}


def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    command = params["command"]
    timeout = params.get("timeout", 120)

    result = subprocess.run(
        command, shell=True, capture_output=True, text=True,
        cwd=str(project), timeout=timeout,
    )
    return {
        "success": result.returncode == 0,
        "stdout": result.stdout,
        "stderr": result.stderr,
        "exit_code": result.returncode,
    }
```

**What to notice:**
- Uses `python_script_runtime` (subprocess isolation) because it runs shell commands
- `working_dir` is optional with a sensible default
- Returns structured output with exit code

### MCP Tool (YAML)

From `.ai/tools/mcp/context7/resolve-library-id.yaml`:

```yaml
# rye:signed:2026-02-04T23:57:39Z:placeholder:unsigned:unsigned
tool_type: mcp
executor_id: rye/core/runtimes/mcp_http_runtime
category: mcp/context7
version: 1.0.0
description: 'Resolves a package/product name to a Context7-compatible library ID.'
config:
  server: mcp/servers/context7
  tool_name: resolve-library-id
input_schema:
  type: object
  properties:
    query:
      type: string
      description: "The user's original question or task."
    libraryName:
      type: string
      description: Library name to search for.
  required:
    - query
    - libraryName
```

## Best Practices

- **Line 1 is the signature** — added by `rye_sign`, never written manually
- **Always return a dict** with at least `success: bool` and either `output`/`data` or `error`
- **Resolve paths relative to project_path** — never use hardcoded absolute paths
- **Security check paths** — verify `file_path.is_relative_to(project)` before operations
- **Category matches directory** — `__category__ = "rye/file-system"` means the file lives at `.ai/tools/rye/file-system/`
- **Use JSON Schema types** in CONFIG_SCHEMA — `string`, `integer`, `boolean`, `object`, `array`
- **Include `if __name__ == "__main__"`** for CLI-runnable tools

## References

- [Tool Metadata Reference](../../rye/rye/.ai/knowledge/rye/core/tool-metadata-reference.md)
- [Terminology](../../rye/rye/.ai/knowledge/rye/core/terminology.md)
