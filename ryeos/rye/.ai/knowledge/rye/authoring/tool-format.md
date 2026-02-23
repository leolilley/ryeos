<!-- rye:signed:2026-02-23T05:24:41Z:ca420dd4d9f38b2dfc6bdcc1992b27d3baa8271d9a9dd865b08cf620989540db:VMQG1wEsOqXtJakntKv1QzzZon5hWopdiTlKhFVqK-R9xwNr07-3AcQlQv_H6xMwThAgwRJR2O8A4n1D6rQ2AQ==:9fbfabe975fa5a7f -->

```yaml
name: tool-format
title: "Tool Format Specification"
entry_type: reference
category: rye/authoring
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - tools
  - format
  - authoring
  - metadata
  - specification
  - python
  - javascript
  - node
  - yaml
  - bash
  - execute-function
  - CONFIG_SCHEMA
  - executor-id
  - create-tool
  - writing-tools
references:
  - directive-format
  - knowledge-format
  - "docs/authoring/tools.md"
```

# Tool Format Specification

Canonical format and metadata reference for tool files — executable scripts stored in `.ai/tools/`.

## Overview

Tools are executable components that **do the actual work** — read files, run commands, make HTTP requests, deploy services. They declare an executor (runtime), metadata for discovery, and a schema for parameter validation.

```
tool → runtime → primitive
```

Examples:
- `write.py` → `rye/core/runtimes/python/function` → in-process
- `bash.py` → `rye/core/runtimes/python/script` → subprocess
- `query-docs.yaml` → `rye/core/runtimes/mcp/http` → HTTP

---

## Python Tools — Primary Format

### File Structure

```
Line 1:  Signature comment (added by rye_sign)
         Module docstring
         Metadata variables (__version__, __tool_type__, etc.)
         CONFIG_SCHEMA dict
         execute() function
         Optional: if __name__ == "__main__" CLI block
```

### Required Structure

```python
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
"""Brief description of what this tool does."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python/function"
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

---

## Metadata Variables (Python)

| Variable | Type | Required | Description | Example |
|----------|------|----------|-------------|---------|
| `__version__` | string | **Yes** | Semantic version | `"1.0.0"` |
| `__tool_type__` | string | **Yes** | Tool classification | `"python"` |
| `__executor_id__` | string | **Yes** | Runtime that executes this tool | `"rye/core/runtimes/python/function"` |
| `__category__` | string | **Yes** | Directory path within `.ai/tools/` | `"rye/file-system"` |
| `__tool_description__` | string | **Yes** | Human-readable description | `"Create or overwrite a file"` |

### `__executor_id__` — The Executor Chain

The executor ID determines the runtime that runs the tool. Tools don't run on their own — they always chain through a runtime.

| Executor ID | Isolation | When to Use |
|-------------|-----------|-------------|
| `rye/core/runtimes/python/function` | In-process | Pure Python — imported and called directly |
| `rye/core/runtimes/python/script` | Subprocess | Needs isolation (shell commands, heavy I/O) |
| `rye/core/runtimes/mcp/http` | HTTP | MCP tool wrapping external server |

- Use `python/function` for most tools — faster, no subprocess overhead
- Use `python/script` when the tool runs shell commands or needs isolation
- `null` executor is only valid for `primitive` tool types

---

## CONFIG_SCHEMA

JSON Schema dict defining accepted parameters. Validated by the runtime **before** calling `execute()`.

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
        "mode": {
            "type": "string",
            "description": "Write mode",
            "enum": ["overwrite", "append"],
            "default": "overwrite",
        },
    },
    "required": ["file_path", "content"],
}
```

### Supported JSON Schema Types

| Type | Python Type | Notes |
|------|-------------|-------|
| `string` | `str` | Supports `minLength`, `maxLength`, `pattern`, `enum` |
| `integer` | `int` | Supports `minimum`, `maximum` |
| `number` | `float` | Supports `minimum`, `maximum` |
| `boolean` | `bool` | — |
| `object` | `dict` | Supports `properties`, `required`, `additionalProperties` |
| `array` | `list` | Supports `items`, `minItems`, `maxItems` |

### Parameter Constraints

| Constraint | Applies To | Example |
|-----------|-----------|---------|
| `required` | Top-level | `"required": ["file_path"]` |
| `default` | Any property | `"default": 120` |
| `minimum` / `maximum` | integer, number | `"minimum": 1, "maximum": 10` |
| `minLength` / `maxLength` | string | `"minLength": 3` |
| `pattern` | string | `"pattern": "^[a-z][a-z0-9-]*$"` |
| `enum` | string, integer | `"enum": ["overwrite", "append"]` |

---

## The `execute()` Function

**Signature:** `def execute(params: dict, project_path: str) -> dict`

| Parameter | Type | Description |
|-----------|------|-------------|
| `params` | `dict` | Validated parameters matching CONFIG_SCHEMA |
| `project_path` | `str` | Absolute path to the project root |

**Returns:** `dict` with at least `success: bool`.

### Return Dict Format

**Success:**

```python
return {"success": True, "output": result}
return {"success": True, "data": {"key": "value"}, "file_path": "relative/path"}
return {"success": True, "stdout": output, "exit_code": 0}
```

**Error:**

```python
return {"success": False, "error": "Human-readable error message"}
return {"success": False, "error": str(e), "exit_code": 1}
```

### Standard Pattern

```python
def execute(params: dict, project_path: str) -> dict:
    project = Path(project_path).resolve()
    file_path = Path(params["file_path"])

    # Resolve relative paths against project root
    if not file_path.is_absolute():
        file_path = project / file_path
    file_path = file_path.resolve()

    # Security: verify path is inside project
    if not file_path.is_relative_to(project):
        return {"success": False, "error": "Path is outside the project workspace"}

    try:
        result = do_something(file_path)
        return {"success": True, "output": result}
    except Exception as e:
        return {"success": False, "error": str(e)}
```

### Async Support

Both sync and async `execute()` are supported:

```python
async def execute(params: dict, project_path: str) -> dict:
    result = await async_operation(params["url"])
    return {"success": True, "data": result}
```

---

## CLI Fallback

Tools should include `if __name__ == "__main__"` for direct CLI execution:

```python
if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
```

---

## YAML Tools — Configuration-Driven

### Standard YAML Tool

```yaml
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
tool_id: category/tool_name
tool_type: yaml
version: "1.0.0"
executor_id: rye/core/runtimes/python/script
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

### MCP Tool Definition (YAML)

Wraps an external MCP server tool:

```yaml
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
tool_type: mcp
executor_id: rye/core/runtimes/mcp/http
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

### MCP Server Definition (YAML)

Referenced by MCP tool definitions:

```yaml
# rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID
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

---

## YAML Metadata Fields

### Required Fields

| Field | Type | Description | Example |
|-------|------|-------------|---------|
| `tool_id` | string (kebab-case) | Unique identifier | `deploy-service` |
| `tool_type` | string | Classification | `script`, `mcp`, `mcp_server`, `primitive`, `runtime`, `library` |
| `version` | string (semver) | Semantic version | `"1.0.0"` |
| `description` | string | What the tool does | `"Deploy service to cluster"` |
| `executor_id` | string or null | Runtime that runs this tool | `python_runtime` (null for primitives) |
| `category` | string | Directory path in `.ai/tools/` | `deployment/kubernetes` |

### Optional Fields

| Field | Type | Description |
|-------|------|-------------|
| `requires` | list of strings | Capabilities needed (e.g., `fs.write`, `shell.execute`, `net.http`) |
| `parameters` | list of param specs | Input parameter definitions |
| `outputs` | object/schema | Output schema |
| `actions` | dict of action defs | Sub-actions within the tool |
| `tags` | list of strings | Searchable tags |
| `config` | object | Tool-specific configuration |
| `input_schema` | JSON Schema object | Complete parameter validation schema |
| `output_schema` | JSON Schema object | Complete output validation schema |
| `timeout` | integer (seconds) | Default execution timeout |
| `retry_policy` | object | Retry behavior on failure |
| `cost` | object | Cost estimation |
| `documentation` | object | Links and examples |
| `metadata` | object | Additional metadata (author, maintainer, stability) |

### `requires` — Capability Declarations

```yaml
requires:
  - rye.execute.spawn.thread
  - rye.execute
  - fs.write
  - shell.execute
  - net.http
```

### `parameters` — YAML Parameter Specs

```yaml
parameters:
  - name: service_name
    type: string
    required: true
    description: "Name of service to deploy"
  - name: replicas
    type: integer
    required: false
    default: 3
    description: "Number of replicas (1-10)"
    minimum: 1
    maximum: 10
```

Parameter properties: `name`, `type`, `required`, `default`, `description`, `minimum`, `maximum`, `minLength`, `maxLength`, `pattern`, `enum`.

### `input_schema` — Full JSON Schema

```yaml
input_schema:
  type: object
  properties:
    service_name:
      type: string
      minLength: 3
      pattern: "^[a-z][a-z0-9-]*$"
    replicas:
      type: integer
      minimum: 1
      maximum: 10
  required:
    - service_name
  additionalProperties: false
```

### `output_schema`

```yaml
output_schema:
  type: object
  properties:
    deployment_id:
      type: string
      pattern: "^[a-f0-9-]+$"
    status:
      type: string
      enum: ["success", "failed", "pending"]
  required:
    - deployment_id
    - status
```

### `retry_policy`

```yaml
retry_policy:
  max_attempts: 3
  backoff_type: exponential
  initial_delay: 1
  max_delay: 60
  backoff_multiplier: 2
```

### `cost`

```yaml
cost:
  per_invocation: 0.01
  per_minute: 0.05
  estimated_duration: 60
  currency: USD
```

### `config` — Tool-Specific Configuration

For subprocess tools:

```yaml
config:
  command: python
  args:
    - deploy.py
  env:
    KUBECONFIG: /etc/kubernetes/config
    LOG_LEVEL: INFO
```

For Docker tools:

```yaml
config:
  image: python:3.11
  command: python
  entrypoint: /app/deploy.py
  env:
    - KUBECONFIG
    - AWS_CREDENTIALS
```

For MCP tools:

```yaml
config:
  server: mcp/servers/context7
  tool_name: resolve-library-id
```

### `actions` — Sub-Actions

```yaml
actions:
  deploy:
    description: "Deploy service"
    parameters:
      - name: service_name
        type: string
        required: true
  rollback:
    description: "Rollback to previous version"
    parameters:
      - name: deployment_id
        type: string
        required: true
```

### `documentation`

```yaml
documentation:
  url: "https://docs.example.com/tools/deploy"
  examples:
    - "Deploy to production"
    - "Scale service replicas"
```

### `metadata`

```yaml
metadata:
  author: platform-team
  maintainer: ops@example.com
  deprecated: false
  experimental: false
  stability: stable
```

---

## Tool Types

| Type | Executor | Description |
|------|----------|-------------|
| `python` | `python/function` or `python/script` | Python executable script |
| `script` | Language-specific runtime | Executable script in any language |
| `mcp` | `mcp/http` | MCP tool wrapping external server |
| `mcp_server` | `null` | MCP server definition (referenced by MCP tools) |
| `primitive` | `null` | Atomic, low-level operation (subprocess, http_client, filesystem) |
| `runtime` | `null` | Language runtime/execution environment |
| `library` | `null` | Reusable code without direct execution |
| `yaml` | Varies | Configuration-driven tool |
| `http` | `http_client` | HTTP-based tool |

---

## Supported Languages / Runtimes

| Language | File Extension | Executor Pattern |
|----------|---------------|------------------|
| Python | `.py` | `python/function`, `python/script` |
| YAML | `.yaml`, `.yml` | `mcp/http`, `python/script` |
| JavaScript | `.js`, `.mjs`, `.cjs` | `node/node` |
| TypeScript | `.ts` | `node/node` |
| Bash | `.sh` | `subprocess` |
| TOML | `.toml` | Configuration-only |

### JavaScript / TypeScript Tool Example

```typescript
// .ai/tools/utility/hello_node.ts
// rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID

export const __version__ = "1.0.0";
export const __tool_type__ = "javascript";
export const __executor_id__ = "rye/core/runtimes/node/node";
export const __category__ = "utility";
export const __tool_description__ = "Greet a user by name";

export const CONFIG_SCHEMA = {
  type: "object",
  properties: {
    name: { type: "string", description: "Name to greet" },
  },
  required: ["name"],
};

function main(params: Record<string, unknown>) {
  return { success: true, output: `Hello, ${params.name}!` };
}
export default main;
```

Metadata is extracted by the `javascript/javascript` parser via regex — no JS runtime needed at parse time. The same `export const __dunder__` convention used by Python tools applies, with `export const` instead of bare assignment.

### Class-Based Python Tool (Runtime)

```python
__tool_type__ = "runtime"
__version__ = "1.0.0"
__executor_id__ = "python"
__category__ = "sinks"

class FileSink:
    """Append streaming events to file."""
    def __init__(self, path: str, format: str = "jsonl", flush_every: int = 10):
        self.path = Path(path)
    async def write(self, event: str) -> None:
        # ...
```

---

## Tool Resolution

Tools resolve by `item_id` to file path:

```
item_id: "rye/file-system/write"
  → .ai/tools/rye/file-system/write.py

item_id: "mcp/context7/query-docs"
  → .ai/tools/mcp/context7/query-docs.yaml

item_id: "rye/bash/bash"
  → .ai/tools/rye/bash/bash.py
```

The `category`/`__category__` determines the directory path within `.ai/tools/`.

---

## Validation Rules

1. **Python:** `__version__`, `__tool_type__`, `__executor_id__`, `__category__`, `__tool_description__` are all required
2. **YAML:** `tool_id`, `category`, `tool_type`, `version`, `description`, `executor_id` are all required
3. `tool_id` must be kebab-case alphanumeric
4. `category` / `__category__` must match the file path relative to `tools/` directory
5. `version` must be semantic version (`X.Y.Z`)
6. `executor_id` can be `null` only for `primitive`, `runtime`, `library`, and `mcp_server` types
7. Parameter `type` must be valid JSON Schema type
8. `requires` list uses dotted notation (e.g., `fs.write`)
9. `pattern` fields must be valid regex
10. `enum` must have at least one value
11. `execute()` must return a dict with at least `success: bool`

---

## Best Practices

### Naming
- kebab-case for `tool_id`: `deploy-service`, not `DeployService`
- Specific and descriptive: `deploy-kubernetes`, not `deploy`
- Include action/domain: `resize-pool`, not `resize`
- snake_case for Python parameter names

### Structure
- **Line 1 is the signature** — added by `rye_sign`, never written manually
- **Always return a dict** with `success: bool` and either `output`/`data` or `error`
- **Resolve paths relative to project_path** — never hardcode absolute paths
- **Security check paths** — verify `file_path.is_relative_to(project)` before operations
- **Category matches directory** — `__category__ = "rye/file-system"` → `.ai/tools/rye/file-system/`
- **Include `if __name__ == "__main__"`** for CLI-runnable tools

### Parameters
- Provide `description` for every parameter
- Always specify `required` status
- Add constraints (`minimum`, `maximum`, `pattern`) for clarity
- Use `enum` for constrained values
- Use `default` for optional parameters with sensible defaults

### Error Handling
- Define expected error conditions
- Use `retry_policy` for transient failures
- Document timeout behavior
- Return structured errors: `{"success": False, "error": "message"}`
