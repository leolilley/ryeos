<!-- rye:signed:2026-02-22T23:38:13Z:31a539793212a2d9dd2cc97f71eb789b457e992852063c52a3e45c07fbac749d:YOvpSBDfwEdwbolg6Qn1HccQis-qU81y8fC_fDIP6ffbh0x-PvLHG0jraCo7d5ErNnpW3pKjnvVXAGUkX0bLAg==:9fbfabe975fa5a7f -->

```yaml
id: standard-runtimes
title: "Standard Runtimes Reference"
entry_type: reference
category: rye/core/runtimes
version: "1.0.0"
author: rye-os
created_at: 2026-02-23T00:00:00Z
tags:
  - runtime
  - runtimes
  - python
  - node
  - javascript
  - typescript
  - bash
  - shell
  - mcp
  - subprocess
  - execution
  - how-tools-run
  - interpreter
  - venv
  - node-modules
  - function-runtime
  - script-runtime
  - state-graph
  - tool-execution
  - parameter-passing
  - args
  - command
references:
  - executor-chain
  - templating-systems
  - "docs/internals/executor-chain.md"
  - "docs/standard-library/tools/core.md"
```

# Standard Runtimes Reference

The 7 built-in runtimes that execute tools in Rye OS — from subprocess-based script execution to in-process function calls to MCP protocol bridges.

## What Runtimes Are

A runtime is a YAML configuration that defines **how** a tool is executed. It maps a tool type (Python, Node, Bash, etc.) to a primitive execution layer (subprocess, HTTP, etc.) and configures:

- **Interpreter resolution** — where to find the language binary (venv, node_modules, system PATH, version manager)
- **Command templates** — how to invoke the tool with parameters
- **Environment setup** — env vars, module paths, anchoring
- **Dependency verification** — integrity checks before execution

All runtimes point to an underlying **primitive** — the Lilux layer that actually executes (subprocess, HTTP client, state graph walker).

```
Tool (Python/JS/Bash)
    ↓
Runtime (python_script_runtime, node_runtime, bash_runtime, ...)
    ↓
Primitive (subprocess, http_client, state_graph)
    ↓
Lilux execution layer
```

## The 7 Standard Runtimes

| Runtime | Language | Execution | Interpreter Resolution | Use When |
|---------|----------|-----------|------------------------|----------|
| **python_script_runtime** | Python | Subprocess | venv (`.venv/bin/python`) | Scripts, I/O, isolation needed |
| **python_function_runtime** | Python | In-process | venv (same as script) | Pure functions, fast execution |
| **node_runtime** | JavaScript/TypeScript | Subprocess | node_modules (`.bin`) or system | Node.js tools, npm packages |
| **bash_runtime** | Bash/Shell | Subprocess | System binary (`which bash`) | Shell scripts, CLI tools |
| **mcp_stdio_runtime** | MCP (stdio protocol) | Subprocess + stdio | N/A (launches MCP server) | MCP servers via stdio |
| **mcp_http_runtime** | MCP (HTTP protocol) | HTTP client | N/A (connects via HTTP) | Remote MCP servers, APIs |
| **state_graph_runtime** | State graphs | In-process state machine | N/A (orchestration) | Multi-step workflows, graphs |

## Python Script Runtime

**File:** `.ai/tools/rye/core/runtimes/python_script_runtime.yaml`

Executes Python scripts in a subprocess. Scripts receive parameters as CLI args.

### Config

```yaml
env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib/python
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{anchor_path}/lib/python"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", ".git", "dist", "build"]

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

### Tool Signature

```python
# rye:signed:TIMESTAMP:HASH:SIG:FP
"""Tool description."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_script_runtime"
__category__ = "category/path"
__tool_description__ = "Description"

CONFIG_SCHEMA = { ... }

if __name__ == "__main__":
    import argparse, json
    parser = argparse.ArgumentParser()
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()
    result = execute(json.loads(args.params), args.project_path)
    print(json.dumps(result))
```

## Python Function Runtime

**File:** `.ai/tools/rye/core/runtimes/python_function_runtime.yaml`

Imports Python modules and calls functions directly (in-process). No subprocess overhead.

### Config

```yaml
env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON

config:
  timeout: 300
```

### Tool Signature

Same as script runtime — `execute(params: dict, project_path: str) → dict`. The runtime imports the module and calls `execute()` directly.

### When to Use

- Pure Python functions with no subprocess needs
- Fast execution required
- No isolation needed
- Tool is stateless

**Don't use for:**
- Shell commands
- Heavy I/O requiring async
- Need for process isolation

## Node Runtime

**File:** `.ai/tools/rye/core/runtimes/node_runtime.yaml`

Executes JavaScript/TypeScript via Node.js in a subprocess. Tools receive parameters as CLI args.

### Config

```yaml
env_config:
  interpreter:
    type: node_modules
    search_paths: [node_modules/.bin]
    var: RYE_NODE
    fallback: node
  env:
    NODE_ENV: development

anchor:
  enabled: true
  mode: auto
  markers_any: ["package.json"]
  root: tool_dir
  lib: lib/node
  cwd: "{anchor_path}"
  env_paths:
    NODE_PATH:
      prepend: ["{anchor_path}", "{anchor_path}/node_modules"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".js", ".ts", ".mjs", ".cjs", ".json", ".yaml", ".yml"]
  exclude_dirs: ["node_modules", "__pycache__", ".git", "dist", "build"]

config:
  command: "${RYE_NODE}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300
```

### Tool Signature (JSDoc)

```javascript
/**
 * @version 1.0.0
 * @tool_type javascript
 * @executor_id rye/core/runtimes/node_runtime
 * @category category/path
 * @description Tool description
 */

function execute(params, project_path) {
  // Implementation
  return { success: true, data: result };
}

if (require.main === module) {
  const args = require('minimist')(process.argv.slice(2));
  const params = JSON.parse(args['params'] || '{}');
  const projectPath = args['project-path'];
  const result = execute(params, projectPath);
  console.log(JSON.stringify(result));
}

module.exports = { execute };
```

### TypeScript Support

Use `tsx` (TypeScript executor) via `node_modules/.bin/tsx` or system fallback:

```yaml
env_config:
  interpreter:
    type: node_modules
    search_paths: [node_modules/.bin]
    binary: tsx
    var: RYE_TSX
    fallback: node
```

## Bash Runtime

**File:** `.ai/tools/rye/core/runtimes/bash_runtime.yaml`

Executes shell scripts directly via bash.

### Config

```yaml
env_config:
  interpreter:
    type: system_binary
    binary: bash
    var: RYE_BASH
    fallback: /bin/bash

config:
  command: "${RYE_BASH}"
  args:
    - "{tool_path}"
    - "{params_json}"
    - "{project_path}"
  timeout: 300
```

### Tool Format

Shell scripts receive three arguments:

1. `$1` — Parameters as JSON string
2. `$2` — Project path

```bash
#!/bin/bash
# rye:signed:TIMESTAMP:HASH:SIG:FP

params="$1"
project_path="$2"

# Parse params (jq recommended)
name=$(echo "$params" | jq -r '.name')

# Do work
result="{\"success\": true, \"output\": \"$output\"}"

echo "$result"
```

## MCP Stdio Runtime

**File:** `.ai/tools/rye/core/runtimes/mcp_stdio_runtime.yaml`

Launches an MCP server process and communicates via stdio (JSON-RPC 2.0). The server process handles all communication.

### Config

```yaml
config:
  command: node
  args:
    - "{mcp_server_path}"
  timeout: 300
```

### When to Use

- Wrapping local MCP servers
- CLI tools that speak MCP
- Subprocess-based MCP integration

## MCP HTTP Runtime

**File:** `.ai/tools/rye/core/runtimes/mcp_http_runtime.yaml`

Connects to a remote MCP server via HTTP and makes tool calls over HTTP.

### Config

```yaml
config:
  base_url: "http://localhost:3000"
  timeout: 300
```

### When to Use

- Remote MCP servers
- SaaS APIs
- Services already exposed via HTTP

## State Graph Runtime

**File:** `.ai/tools/rye/core/runtimes/state_graph_runtime.yaml`

Executes state machine graphs in-process. Graphs coordinate multi-step workflows with conditional branching.

### Config

```yaml
config:
  timeout: 300
  max_steps: 1000
```

### When to Use

- Complex orchestration workflows
- Conditional multi-step execution
- State-driven logic

## Interpreter Resolution Types

| Type | Config | Resolution Strategy |
|------|--------|---------------------|
| `venv_python` | `venv_path: .venv` | Check `.venv/bin/python`, fallback to `fallback` |
| `node_modules` | `search_paths: [node_modules/.bin]` | Check `node_modules/.bin/<binary>`, fallback |
| `system_binary` | `binary: ruby` | Run `which ruby` / `where ruby`, fallback |
| `version_manager` | `manager: pyenv, version: 3.11` | Query pyenv/nvm/rbenv/asdf |

All types resolve to an absolute path stored in the env var named by `var`. If resolution fails, `fallback` is used.

## Template Variables in Args

All runtimes support template variables in `config.args`:

| Variable | Source | Description |
|----------|--------|-------------|
| `{tool_path}` | Tool file path | Absolute path to the tool file |
| `{tool_dir}` | Tool directory | Directory containing the tool |
| `{params_json}` | Serialized parameters | JSON string of validated parameters |
| `{project_path}` | Project root | Absolute path to project root |
| `{anchor_path}` | Anchor resolution | Module resolution root (if anchor enabled) |
| `{runtime_lib}` | Anchor config | Runtime library path (if anchor enabled) |
| `{user_space}` | Executor context | User space path |
| `{system_space}` | Executor context | System space path |

Template substitution happens in two passes:
1. **Pass 1:** `${VAR}` environment variable expansion
2. **Pass 2:** `{param}` runtime parameter substitution (up to 3 iterations)

## Anchor System

The `anchor` config enables module resolution for multi-file tools:

```yaml
anchor:
  enabled: true
  mode: auto                    # auto, always, or never
  markers_any: ["__init__.py"]  # Root markers to find
  root: tool_dir                # tool_dir, tool_parent, or project_path
  lib: lib/python               # Library subdir relative to anchor
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}"]
```

When active:
1. Search from tool directory upward for marker files
2. Resolve anchor root (where `__init__.py` was found)
3. Prepend/append to env vars (e.g., `PYTHONPATH`)
4. Run `verify_deps` to check all files in anchor scope

## Dependency Verification

The `verify_deps` config walks the anchor directory and verifies all matching files:

```yaml
verify_deps:
  enabled: true
  scope: anchor              # anchor, tool_dir, tool_siblings, tool_file
  recursive: true
  extensions: [".py", ".yaml"]
  exclude_dirs: ["__pycache__", ".git"]
```

Detects:
- Tampered files (hash mismatch)
- Unsigned files
- Symlink escapes
- Missing dependencies

Any mismatch raises `IntegrityError` and halts execution.

## Creating a Custom Runtime

1. Create YAML at `.ai/tools/<category>/<name>.yaml`
2. Set `tool_type: runtime` and `executor_id` to a primitive
3. Configure interpreter resolution in `env_config.interpreter`
4. Define `config.command` and `config.args` with template variables
5. Sign: `rye_sign(item_type="tool", item_id="<category>/<name>")`
6. Tools use `__executor_id__ = "<category>/<name>"` to reference it

See the knowledge entry `rye/core/runtimes/runtime-authoring` for detailed guidance on custom runtime creation.
