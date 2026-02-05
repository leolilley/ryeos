**Source:** Implementation: `.ai/tools/rye/core/mcp/` in rye-os

# MCP Core Tools

## Purpose

MCP (Model Context Protocol) tools enable RYE to **discover and call external MCP servers** as first-class tools. This allows agents to connect to any MCP server (HTTP or stdio) and use its tools through the standard `rye execute` flow.

**Location:** `.ai/tools/rye/core/mcp/`

## Architecture

MCP support is built entirely on existing primitives (subprocess, http_client) with data-driven configuration:

```
Execution Chain
───────────────
MCP Tool (YAML)
  └─ executor_id: rye/core/runtimes/mcp_http_runtime
       └─ executor_id: rye/core/primitives/subprocess
            └─ Runs: python connect.py --server-config ... --tool ... --params ...
                 └─ connect.py uses MCP SDK, outputs JSON to stdout
```

## File Structure

All MCP-related files are signable tools under `.ai/tools/`:

```
.ai/tools/
├── rye/core/mcp/                     # Core MCP implementation
│   ├── connect.py                    # Executable script - calls MCP tools
│   ├── discover.py                   # Executable script - discovers MCP tools
│   └── manager.py                    # Server management (add/list/refresh/remove)
│
├── rye/core/runtimes/
│   ├── mcp_http_runtime.py           # Runtime for HTTP MCP servers
│   └── mcp_stdio_runtime.py          # Runtime for stdio MCP servers
│
└── mcp/                              # User's configured MCP servers & tools
    ├── servers/                      # Server connection configs
    │   ├── context7.yaml
    │   └── rye-os.yaml
    │
    ├── context7/                     # Discovered tools from context7
    │   ├── resolve-library-id.yaml
    │   └── query-docs.yaml
    │
    └── rye-os/                       # Discovered tools from rye-os
        ├── search.yaml
        └── execute.yaml
```

## Core Scripts

### connect.py

Directly executable Python script that calls MCP tools. Uses MCP SDK internally.

```python
# Metadata (data-driven)
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/core/mcp"

# Executable via CLI
if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--server-config", required=True)
    parser.add_argument("--tool", required=True)
    parser.add_argument("--params", default="{}")
    parser.add_argument("--project-path", required=True)
    ...
    result = asyncio.run(execute(...))
    print(json.dumps(result))
```

**CLI Interface:**

```bash
python connect.py \
  --server-config /path/to/.ai/tools/mcp/servers/context7.yaml \
  --tool resolve-library-id \
  --params '{"libraryName": "react"}' \
  --project-path /path/to/project
```

**Responsibilities:**

1. Load server config YAML
2. Resolve `${ENV_VAR}` from .env files
3. Connect via MCP SDK (HTTP or stdio based on transport)
4. Call the specified tool
5. Output JSON result to stdout

### discover.py

Directly executable Python script that discovers tools from MCP servers.

```bash
python discover.py \
  --transport http \
  --url https://mcp.context7.com/mcp \
  --headers '{"Authorization": "Bearer ${CONTEXT7_API_KEY}"}' \
  --project-path /path/to/project
```

**Responsibilities:**

1. Connect to MCP server
2. List available tools
3. Output tool definitions as JSON

## Runtimes

### mcp_http_runtime.py

Configures subprocess primitive to run connect.py for HTTP MCP servers.

```python
__tool_type__ = "runtime"
__executor_id__ = "rye/core/primitives/subprocess"
__category__ = "rye/core/runtimes"

ENV_CONFIG = {
    "interpreter": {
        "type": "venv_python",
        "var": "RYE_PYTHON",
        "fallback": "python3",
    },
}

CONFIG = {
    "command": "${RYE_PYTHON}",
    "args": [
        "{connect_script_path}",
        "--server-config", "{server_config_path}",
        "--tool", "{tool_name}",
        "--params", "{params_json}",
        "--project-path", "{project_path}",
    ],
    "timeout": 60,
}
```

### mcp_stdio_runtime.py

Same pattern for stdio MCP servers. The transport type is determined from the server config.

## Server Configuration

Server configs are signable YAML tool files.

### HTTP Server (`.ai/tools/mcp/servers/context7.yaml`)

```yaml
# rye:validated:2026-02-05T00:00:00Z:abc123...
tool_type: mcp_server
executor_id: null # Not directly executable
category: mcp/servers
version: "1.0.0"
description: "Context7 documentation MCP server"

config:
  transport: http
  url: https://mcp.context7.com/mcp
  headers:
    Authorization: "Bearer ${CONTEXT7_API_KEY}"
  timeout: 30

cache:
  discovered_at: "2026-02-05T12:00:00Z"
  tool_count: 2
```

### Stdio Server (`.ai/tools/mcp/servers/rye-os.yaml`)

```yaml
# rye:validated:2026-02-05T00:00:00Z:def456...
tool_type: mcp_server
executor_id: null
category: mcp/servers
version: "1.0.0"
description: "RYE-OS MCP server (recursive self-connection)"

config:
  transport: stdio
  command: python
  args: ["-m", "rye.server"]
  env:
    RYE_DEBUG: "${RYE_DEBUG:-false}"
  timeout: 30

cache:
  discovered_at: "2026-02-05T12:00:00Z"
  tool_count: 4
```

## Discovered Tool Configuration

Discovered tools are YAML configs that route through the MCP runtime.

### Example (`.ai/tools/mcp/context7/resolve-library-id.yaml`)

```yaml
# rye:validated:2026-02-05T00:00:00Z:789abc...
tool_type: mcp
executor_id: rye/core/runtimes/mcp_http_runtime
category: mcp/context7
version: "1.0.0"
description: "Resolves a package/product name to a Context7-compatible library ID"

config:
  server: mcp/servers/context7 # Relative path to server config
  tool_name: resolve-library-id

input_schema:
  type: object
  properties:
    libraryName:
      type: string
      description: "Library name to search for"
    query:
      type: string
      description: "The user's original question or task"
  required: [query, libraryName]
```

## Execution Flow

### 1. User Invokes MCP Tool

```
rye execute tool mcp/context7/resolve-library-id --parameters '{"libraryName": "react", "query": "how to use hooks"}'
```

### 2. PrimitiveExecutor Builds Chain

```
Tool: mcp/context7/resolve-library-id.yaml
  └─ executor_id: rye/core/runtimes/mcp_http_runtime
       └─ executor_id: rye/core/primitives/subprocess
            └─ executor_id: null (terminal)
```

### 3. Chain Config Resolution

From the tool config:

- `server: mcp/servers/context7` → resolves to server config path
- `tool_name: resolve-library-id`

From the runtime config:

- `command: ${RYE_PYTHON}` → resolved via ENV_CONFIG
- `args: [...]` → templated with tool config values

### 4. Subprocess Execution

```bash
/path/to/.venv/bin/python \
  /path/to/.ai/tools/rye/core/mcp/connect.py \
  --server-config /path/to/.ai/tools/mcp/servers/context7.yaml \
  --tool resolve-library-id \
  --params '{"libraryName": "react", "query": "how to use hooks"}' \
  --project-path /path/to/project
```

### 5. connect.py Execution

1. Loads `context7.yaml` server config
2. Loads .env files, resolves `${CONTEXT7_API_KEY}`
3. Connects via MCP SDK (streamable HTTP)
4. Calls `resolve-library-id` with params
5. Outputs JSON to stdout:

```json
{
  "success": true,
  "tool": "resolve-library-id",
  "content": [{ "type": "text", "text": "..." }]
}
```

### 6. Result Returned

Subprocess primitive captures stdout, parses JSON, returns to executor.

## MCP Manager

The manager provides CRUD operations for MCP servers via `rye execute`.

### Add Server

```
rye execute tool rye/core/mcp/manager --parameters '{
  "action": "add",
  "name": "context7",
  "transport": "http",
  "url": "https://mcp.context7.com/mcp",
  "headers": {"Authorization": "Bearer ${CONTEXT7_API_KEY}"},
  "scope": "project"
}'
```

**Flow:**

1. Creates `.ai/tools/mcp/servers/context7.yaml`
2. Runs discover.py to find available tools
3. Creates `.ai/tools/mcp/context7/{tool}.yaml` for each discovered tool
4. Signs all created files

### List Servers

```
rye execute tool rye/core/mcp/manager --parameters '{
  "action": "list",
  "include_tools": true
}'
```

### Refresh Server

```
rye execute tool rye/core/mcp/manager --parameters '{
  "action": "refresh",
  "name": "context7"
}'
```

### Remove Server

```
rye execute tool rye/core/mcp/manager --parameters '{
  "action": "remove",
  "name": "context7"
}'
```

## Environment Variables

API keys are stored in `.env` files and resolved by connect.py at runtime.

### .env Locations (Precedence)

1. `~/.ai/.env` - User-level secrets
2. `{project}/.ai/.env` - Project-level secrets
3. `{project}/.env` - Root .env
4. `{project}/.env.local` - Local overrides

### Example .env

```bash
CONTEXT7_API_KEY=your-api-key-here
RYE_DEBUG=false
```

### Template Syntax

```yaml
headers:
  Authorization: "Bearer ${CONTEXT7_API_KEY}"
  X-Custom: "${CUSTOM_HEADER:-default-value}"
```

## Signing

All MCP files are signable tools using `# rye:validated:...` YAML comment format:

```yaml
# rye:validated:2026-02-05T12:00:00Z:abc123def456...
tool_type: mcp_server
...
```

**Benefits:**

- Integrity verification
- Registry sharing (publish server configs)
- Version control with meaningful hashes
- Cache invalidation

## Recursive Self-Connection

RYE can call itself through MCP:

```bash
# Add rye-os as stdio MCP server
rye execute tool rye/core/mcp/manager --parameters '{
  "action": "add",
  "name": "rye-os",
  "transport": "stdio",
  "command": "python",
  "args": ["-m", "rye.server"]
}'

# Call rye search through MCP
rye execute tool mcp/rye-os/search --parameters '{
  "item_type": "tool",
  "query": "mcp",
  "project_path": "/path/to/project"
}'
```

This spawns `python -m rye.server`, connects via stdio MCP, calls `mcp__rye__search`.

## Summary

| Component         | Type              | Location           | Purpose                     |
| ----------------- | ----------------- | ------------------ | --------------------------- |
| connect.py        | Executable script | rye/core/mcp/      | Call MCP tools              |
| discover.py       | Executable script | rye/core/mcp/      | Discover MCP tools          |
| manager.py        | Executable script | rye/core/mcp/      | Server CRUD                 |
| mcp_http_runtime  | Runtime           | rye/core/runtimes/ | Configure HTTP transport    |
| mcp_stdio_runtime | Runtime           | rye/core/runtimes/ | Configure stdio transport   |
| servers/\*.yaml   | Config            | mcp/servers/       | Connection details          |
| {server}/\*.yaml  | Config            | mcp/{server}/      | Discovered tool definitions |

## Related Documentation

- [runtimes](runtimes.md) - Runtime architecture (Layer 2)
- [primitives](primitives.md) - Subprocess primitive (Layer 1)
- [../overview](../overview.md) - All data tool categories
