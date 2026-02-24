```yaml
id: runtimes
title: "Runtimes"
description: "How runtimes configure tool execution — interpreter resolution, arg templates, anchoring, and the 8 standard runtimes"
category: internals
tags: [runtimes, interpreter, execution, python, node, javascript, bash, mcp, subprocess, state-graph, rust]
version: "1.0.0"
```

# Runtimes

A **runtime** is a YAML configuration that describes how to invoke a tool. It bridges tools (Python scripts, JavaScript, YAML configs) and [primitives](lilux-primitives.md) (subprocess, HTTP). Every tool declares an `executor_id` pointing to a runtime, which then points to a primitive.

## Where Runtimes Live

Runtimes are YAML files in `.ai/tools/rye/core/runtimes/` (system space). You can override or extend them by placing custom runtimes in project space at `.ai/tools/rye/core/runtimes/my-runtime.yaml`.

```
.ai/tools/rye/core/runtimes/
├── bash/bash.yaml                       # Execute shell commands
├── mcp/
│   ├── http.yaml                        # Call MCP tools via HTTP
│   └── stdio.yaml                       # Spawn MCP servers over stdio
├── node/node.yaml                       # Run JavaScript/TypeScript with Node
├── python/
│   ├── function.yaml                    # Call Python execute() in-process
│   ├── script.yaml                      # Run Python scripts in subprocess
│   └── lib/                             # Shared Python runtime libraries
├── rust/
│   ├── runtime.yaml                     # Rust binary runtime
│   ├── rye-proc/                        # Process lifecycle manager
│   └── rye-watch/                       # Push-based registry watcher (src/)
└── state-graph/
    ├── runtime.yaml                     # Walk declarative graph YAML
    └── walker.py                        # Graph walking engine
```

## The 8 Standard Runtimes

| Runtime | Language | Execution | When to Use |
|---------|----------|-----------|------------|
| **python/function** | Python | In-process (fast) | Pure Python logic, fast startup, single-threaded, no shell access needed |
| **python/script** | Python | Subprocess with isolation | Heavy I/O, long-running, needs subprocess isolation, can use shell commands |
| **node/node** | JavaScript/TypeScript | Subprocess with Node resolution | JavaScript tools, TypeScript (via tsx), Node.js ecosystem dependencies |
| **bash/bash** | Bash/Shell | Direct `/bin/bash` execution | Shell scripts, system administration, `jq` pipes, CLI composition |
| **mcp/stdio** | MCP (stdin/stdout) | Subprocess, launch MCP server | Call tools from external MCP servers (e.g., brave-search, filesystem) |
| **mcp/http** | MCP (HTTP/SSE) | HTTP request to MCP server | Call tools from long-running HTTP MCP servers (e.g., Claude-native servers) |
| **rust/runtime** | Rust | Compiled binary on PATH | Cross-platform process management and file watching |
| **state-graph/runtime** | YAML Graph | Subprocess, dispatch rye_execute | Declarative workflows, condition branches, node-by-node execution |

## How Interpreter Resolution Works

Every runtime can configure **interpreter resolution** — finding the right Python, Node, bash, etc. to execute tools. There are 3 resolution types, plus a static environment option:

### Type 1: Local Binary (`local_binary`)

Finds a binary inside local project directories (virtualenvs, `node_modules`, etc.):

**Python example:**

```yaml
env_config:
  interpreter:
    type: local_binary
    binary: python                       # Binary name to find
    candidates: [python3]                # Alternative names to try
    search_paths: [".venv/bin", ".venv/Scripts"]  # Relative to project root
    var: RYE_PYTHON                      # Environment variable to set
    fallback: python3                    # If not found, use this
```

**Node example:**

```yaml
env_config:
  interpreter:
    type: local_binary
    binary: tsx                          # Binary name to find
    search_paths: ["node_modules/.bin"]  # Relative to anchor root
    search_roots: ["{anchor_path}"]      # Where to start searching
    var: RYE_NODE                        # Environment variable to set
    fallback: node                       # If not found, use this
```

**How it works:**
1. For each path in `search_paths`, look for `binary` (and each name in `candidates`)
2. `search_roots` controls where `search_paths` are resolved from (defaults to project root)
3. If found, set the `var` environment variable to that path
4. If not found, fall back to the `fallback` binary from `$PATH`

**Used by:** `python/script`, `python/function`, `state-graph/runtime`, `node/node`, `mcp/stdio`, `mcp/http`

### Type 2: System Binary (`system_binary`)

Finds a binary on the system `$PATH`:

```yaml
env_config:
  interpreter:
    type: system_binary
    binary: python3                      # Binary to find on PATH
    var: RYE_PYTHON
```

**How it works:**
1. Search `$PATH` for the named binary
2. Set `var` to the resolved path

**Used by:** fallback for any runtime when local binary is not found

### Type 3: Command (`command`)

Run a resolve command and use its stdout as the interpreter path:

```yaml
env_config:
  interpreter:
    type: command
    resolve_cmd: ["pyenv", "which", "python"]  # Command to run
    var: RYE_PYTHON
    fallback: python3
```

**How it works:**
1. Execute `resolve_cmd` and capture stdout (trimmed)
2. Set `var` to the resolved path
3. If the command fails, fall back to `fallback`

**Used by:** projects using version managers (pyenv, nvm, mise, etc.)

### Static Environment (`env`)

Directly set environment variables without any interpreter resolution:

```yaml
env_config:
  env:
    PATH: "${PATH}"                     # Pass through PATH
    NODE_ENV: development               # Set static value
```

**How it works:**
- Expand `${PATH}` from current environment
- Set all other keys as static values
- No interpreter binary resolution occurs

**Used by:** `bash/bash` (PATH already has bash), other simple tools

---

## How Anchoring Works

The **anchor system** solves module/dependency resolution by establishing a project root where tool dependencies live. It enables tools to load libraries relative to their anchor path.

### Anchor Configuration

```yaml
anchor:
  enabled: true                         # Enable anchoring for this runtime
  mode: auto                            # 'auto' or 'always'
  markers_any: [__init__.py, pyproject.toml]   # Search for these markers
  root: tool_dir                        # 'tool_dir' (default) or other value
  lib: lib                              # Relative lib path for runtime libraries
  cwd: "{anchor_path}"                  # Optional: change working directory
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]   # Prepend to PATH-like vars
```

### Anchor Resolution Process

**Mode: `auto`**
1. Start from tool directory (e.g., `.ai/tools/rye/bash/`)
2. Walk up parent directories looking for marker files (`__init__.py`, `pyproject.toml`)
3. Stop at the first match — that's the anchor root
4. If no marker found, use tool directory as anchor

**Mode: `always`**
1. Anchor is always the tool directory (no upward search)

### Anchor Path Variables

Once anchored, use these in `env_paths` and `config.args`:

| Variable | Expands To | Example |
|----------|-----------|---------|
| `{anchor_path}` | Root of anchored project | `/home/user/project/.ai/tools/rye/bash` |
| `{runtime_lib}` | Combined anchor lib + runtime lib | `/home/user/project/.ai/tools/rye/bash/lib` |

**Example:** Python runtime with anchoring:

```yaml
anchor:
  enabled: true
  markers_any: [__init__.py, pyproject.toml]
  root: tool_dir
  lib: lib
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]
```

When a tool is executed:
1. Anchor root found at `/project/.ai/tools/rye/bash`
2. `PYTHONPATH` is set to `/project/.ai/tools/rye/bash:/project/.ai/tools/rye/bash/lib`
3. Tool can `import` modules from those directories without package-level imports

> **Note:** The node runtime also uses `env_paths` to prepend `node_modules/.bin` to `PATH`, enabling local binaries to be found without full paths.

---

## How Template Variables Are Substituted Into Args

Runtimes use two-stage templating to build the final command:

### Stage 1: Environment Variable Expansion

`${VAR_NAME}` is replaced with environment variables. This happens **first**, before tool execution:

```yaml
env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"

config:
  command: "${RYE_PYTHON}"              # Resolved to /path/to/python3
  args:
    - "{tool_path}"
```

When `RYE_PYTHON` is resolved to `/project/.venv/bin/python3`, the args become:
```yaml
command: /project/.venv/bin/python3
args:
  - "{tool_path}"                       # Still waiting for stage 2
```

### Stage 2: Runtime Parameter Expansion

`{param_name}` is replaced with values passed to the runtime. This happens **second**, after the tool is called:

| Variable | Value | Example |
|----------|-------|---------|
| `{tool_path}` | Absolute path to tool file | `/project/.ai/tools/rye/bash/bash.py` |
| `{params_json}` | Tool parameters as JSON | `{"command":"ls -la"}` |
| `{project_path}` | Project root | `/project` |
| `{system_space}` | System space root | `/usr/local/lib/python3.11/site-packages/rye/rye/.ai` |
| `{server_config_path}` | MCP server config path | `/project/.ai/tools/mcp/servers/context7.yaml` |
| `{anchor_path}` | Anchored root | `/project/.ai/tools/rye/bash` |
| `{runtime_lib}` | Runtime lib directory | `/project/.ai/tools/rye/bash/lib` |
| `{command}` | For bash/bash: command arg | `ls -la` |
| `{tool_name}` | For MCP runtimes: tool name | `query-docs` |

### Complete Example: Python Script Runtime

```yaml
config:
  command: "${RYE_PYTHON}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
```

When executing `rye/bash/bash` with params `{"command":"echo hello"}`:
1. `${RYE_PYTHON}` → `/project/.venv/bin/python3`
2. `{tool_path}` → `/project/.ai/tools/rye/bash/bash.py`
3. `{params_json}` → `{"command":"echo hello"}`
4. `{project_path}` → `/project`

Final command:
```bash
/project/.venv/bin/python3 /project/.ai/tools/rye/bash/bash.py --params '{"command":"echo hello"}' --project-path /project
```

---

## How to Create a Custom Runtime

A custom runtime is just a YAML file in `.ai/tools/rye/core/runtimes/`:

### Step 1: Create the Runtime File

```yaml
# .ai/tools/rye/core/runtimes/ruby_runtime.yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "Ruby runtime — executes Ruby scripts with bundler support"

env_config:
  interpreter:
    type: local_binary                  # Find Ruby in local directories or fallback
    binary: ruby
    search_paths: [".rbenv/shims", ".rvm/rubies/default/bin"]
    var: RYE_RUBY
    fallback: ruby
  env:
    BUNDLE_GEMFILE: "{anchor_path}/Gemfile"

anchor:
  enabled: true
  mode: auto
  markers_any: [Gemfile, Gemfile.lock]
  root: tool_dir
  lib: lib/ruby
  cwd: "{anchor_path}"
  env_paths:
    RUBYLIB:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [.rb, .yaml, .yml, .json]
  exclude_dirs: [.bundle, .git, node_modules]

config:
  command: "${RYE_RUBY}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    script:
      type: string
      description: Ruby script path
```

### Step 2: Reference It in a Tool

Create a tool that uses the runtime:

```python
# .ai/tools/my/ruby_example.py
"""A Ruby tool example."""

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/ruby/ruby"  # Point to custom runtime
__category__ = "my/ruby"
__tool_description__ = "Example Ruby tool"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "name": {"type": "string", "description": "Name to greet"}
    },
    "required": ["name"]
}

def execute(params: dict, project_path: str) -> dict:
    return {"success": True, "message": f"Hello {params['name']}"}
```

### Step 3: Sign and Execute

```bash
rye_sign my/ruby_example
rye_execute my/ruby_example --name Alice
```

---

## Complete YAML Reference: All 7 Runtimes

### Python Function Runtime (In-Process)

```yaml
# rye/core/runtimes/python/function
# Fastest option: imports Python module and calls execute() directly
# Use for: pure Python logic, compute-heavy tasks, no subprocess needed
# Executor: rye/core/primitives/subprocess (uses inline Python code)

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/python
description: "Python function runtime - calls execute(params, project_path) in-process"

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git", "config"]

# Args passed to python -c (inline code loader)
# Loads module, finds execute(), calls with params and project_path
config:
  command: "${RYE_PYTHON}"
  args:
    - "-c"
    - |
      import sys,json,importlib.util,inspect,asyncio
      spec=importlib.util.spec_from_file_location("tool",sys.argv[1])
      mod=importlib.util.module_from_spec(spec)
      spec.loader.exec_module(mod)
      fn=getattr(mod,"execute",None)
      if not fn:sys.exit("No execute() in "+sys.argv[1])
      p=json.loads(sys.argv[2])
      if inspect.iscoroutinefunction(fn):r=asyncio.run(fn(p,sys.argv[3]))
      else:r=fn(p,sys.argv[3])
      print(json.dumps(r,default=str)if r is not None else'{}')
    - "{tool_path}"
    - "{params_json}"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    script:
      type: string
      description: Python module with execute(params, project_path) function
```

### Python Script Runtime (Subprocess)

```yaml
# rye/core/runtimes/python/script
# Runs Python tool with __main__ entry point
# Use for: subprocess isolation, long-running tasks, I/O, subprocess commands
# Executor: rye/core/primitives/subprocess

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/python
description: "Python script runtime - runs Python scripts with __main__ entry point"

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git", "config"]

# Args: tool.py gets --params and --project-path
# Tool must have: if __name__ == "__main__": argparse --params --project-path
config:
  command: "${RYE_PYTHON}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    script:
      type: string
      description: Python script path or inline code
    args:
      type: array
      items:
        type: string
      description: Script arguments
    module:
      type: string
      description: "Module to run with -m flag"
```

### Node Runtime (JavaScript/TypeScript)

```yaml
# rye/core/runtimes/node/node
# Runs JavaScript/TypeScript with Node.js
# Use for: JavaScript tools, TypeScript (via tsx), Node ecosystem
# Executor: rye/core/primitives/subprocess

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/node
description: "Node.js runtime executor - runs JavaScript/TypeScript with Node interpreter resolution"

env_config:
  interpreter:
    type: local_binary
    binary: tsx
    search_paths: ["node_modules/.bin"]
    search_roots: ["{anchor_path}"]
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
    PATH:
      prepend: ["{anchor_path}/node_modules/.bin"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".js", ".ts", ".mjs", ".cjs", ".json", ".yaml", ".yml"]
  exclude_dirs: ["node_modules", "__pycache__", ".git", "dist", "build"]

# Args: tool.js gets --params and --project-path as CLI args
config:
  command: "${RYE_NODE}"
  args:
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    script:
      type: string
      description: JavaScript file path
    args:
      type: array
      items:
        type: string
      description: Script arguments
```

### Bash Runtime (Shell)

```yaml
# rye/core/runtimes/bash/bash
# Executes shell commands via /bin/bash
# Use for: shell scripts, system administration, CLI composition, jq pipes
# Executor: rye/core/primitives/subprocess

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/bash
description: "Bash runtime - executes shell commands directly"

env_config:
  env:
    PATH: "${PATH}"

config:
  command: "/bin/bash"
  args:
    - "-c"
    - "{command}"
    - "{tool_path}"
    - "{params_json}"
    - "{project_path}"
  timeout: 300

config_schema:
  type: object
  properties:
    command:
      type: string
      description: Shell command to execute
```

### MCP HTTP Runtime

```yaml
# rye/core/runtimes/mcp/http
# Calls MCP tools via HTTP/SSE transport
# Use for: external MCP servers, long-lived HTTP connections, streaming
# Executor: rye/core/primitives/subprocess (launches connect.py)

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/mcp
description: "MCP HTTP runtime - executes MCP tools via HTTP transport"

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"

config:
  command: "${RYE_PYTHON}"
  args:
    - "{system_space}/tools/rye/core/mcp/connect.py"
    - "--server-config"
    - "{server_config_path}"
    - "--tool"
    - "{tool_name}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 60

config_schema:
  type: object
  properties:
    server:
      type: string
      description: "Relative tool ID to server config (e.g., mcp/servers/context7)"
    tool_name:
      type: string
      description: MCP tool name to call
    timeout:
      type: number
      description: Execution timeout in seconds
      default: 60
  required: [server, tool_name]
```

### MCP Stdio Runtime

```yaml
# rye/core/runtimes/mcp/stdio
# Spawns MCP servers and calls tools via stdin/stdout
# Use for: local MCP servers, lightweight stdio transports
# Executor: rye/core/primitives/subprocess (launches connect.py)

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/mcp
description: "MCP stdio runtime - executes MCP tools via stdio transport"

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"

config:
  command: "${RYE_PYTHON}"
  args:
    - "{system_space}/tools/rye/core/mcp/connect.py"
    - "--server-config"
    - "{server_config_path}"
    - "--tool"
    - "{tool_name}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 60

config_schema:
  type: object
  properties:
    server:
      type: string
      description: "Relative tool ID to server config (e.g., mcp/servers/rye-os)"
    tool_name:
      type: string
      description: MCP tool name to call
    timeout:
      type: number
      description: Execution timeout in seconds
      default: 60
  required: [server, tool_name]
```

### Rust Runtime

The Rust runtime executes compiled Rust binaries found on `$PATH`. It ships two binaries:

- **`rye-watch`**: Watches `registry.db` for thread status changes using OS-native file watchers (inotify on Linux, FSEvents/kqueue on macOS, ReadDirectoryChangesW on Windows). Used by the orchestrator's `_poll_registry` as a push-based alternative to polling.
- **`rye-proc`**: Cross-platform process lifecycle manager with subcommands `exec` (run-and-wait with stdout/stderr capture, timeout, stdin piping, cwd, and env support), `spawn` (detached/daemonized), `kill` (graceful SIGTERM → SIGKILL / TerminateProcess), and `status` (is-alive check). All process operations in `SubprocessPrimitive` delegate to rye-proc — it is a hard dependency (no POSIX fallbacks).

```yaml
# rye/core/runtimes/rust/runtime
# Executes compiled Rust binaries found on PATH
# Use for: cross-platform process management, file watching, performance-critical operations
# Executor: rye/core/primitives/subprocess

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/rust
description: "Rust runtime — executes compiled Rust binaries found on PATH"

env_config:
  interpreter:
    type: system_binary
    binary: rye-watch
    var: RYE_RUST_WATCH
  env:
    RUST_BACKTRACE: "0"

config:
  command: "${RYE_RUST_WATCH}"
  args:
    - "--db"
    - "{db_path}"
    - "--thread-id"
    - "{thread_id}"
    - "--timeout"
    - "{timeout}"
  timeout: 600
```

### State Graph Runtime

```yaml
# rye/core/runtimes/state-graph/runtime
# Walks declarative graph YAML, dispatching rye_execute for each node
# Use for: declarative workflows, condition branches, node-by-node execution
# Executor: rye/core/primitives/subprocess

version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes/state-graph
description: "State graph runtime — walks graph YAML tools, dispatching rye_execute for each node"

env_config:
  interpreter:
    type: local_binary
    binary: python
    candidates: [python3]
    search_paths: [".venv/bin", ".venv/Scripts"]
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

anchor:
  enabled: true
  mode: always
  root: tool_dir
  lib: ../python/lib
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git", "config"]

# Args: walker.py loads graph YAML and executes node by node
config:
  command: "${RYE_PYTHON}"
  args:
    - "-c"
    - |
      import sys,os,json,importlib.util
      ap=sys.argv[4]
      walker=os.path.join(ap,"walker.py")
      if not os.path.isfile(walker):
          sys.exit("walker.py not found at: "+repr(walker))
      spec=importlib.util.spec_from_file_location("walker",walker)
      mod=importlib.util.module_from_spec(spec)
      spec.loader.exec_module(mod)
      graph=mod._load_graph_yaml(sys.argv[1])
      r=mod.run_sync(graph,json.loads(sys.argv[2]),sys.argv[3])
      print(json.dumps(r,default=str))
    - "{tool_path}"
    - "{params_json}"
    - "{project_path}"
    - "{anchor_path}"
  timeout: 600

config_schema:
  type: object
  properties:
    graph:
      type: object
      description: Graph YAML structure
```

---

## See Also

- [Lilux Primitives](lilux-primitives.md) — The subprocess and HTTP primitives runtimes delegate to
- [Executor Chain](executor-chain.md) — How tools resolve to runtimes to primitives
- [Authoring Tools](../authoring/tools.md) — Write tools that use runtimes
- [Custom Runtimes](../authoring/custom-runtimes.md) — Create runtimes for new languages
