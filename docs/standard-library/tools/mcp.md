---
id: tools-mcp
title: "MCP Client Tools"
description: Connect to external MCP servers, discover tools, and manage server configurations
category: standard-library/tools
tags: [tools, mcp, connect, discover, manager, external]
version: "1.0.0"
---

# MCP Client Tools

**Namespace:** `rye/mcp/`
**Runtime:** `python_script_runtime`

Three tools for integrating with external MCP servers. Rye OS itself is an MCP server — these tools let it act as a **client** to other MCP servers, forming a hub-and-spoke architecture.

Supports two transports:

- **HTTP** — Streamable HTTP transport (MCP spec 2025-03-26), recommended for remote servers
- **stdio** — Local process via stdin/stdout, for CLI-based MCP servers

Requires the MCP SDK: `pip install mcp httpx`

---

## `connect`

**Item ID:** `rye/mcp/connect`

Execute a tool call on an MCP server. Can operate in two modes:

### Server Config Mode

Uses a YAML config file that defines the server connection:

```python
rye_execute(item_type="tool", item_id="rye/mcp/connect",
    parameters={
        "server_config": ".ai/tools/mcp/servers/context7.yaml",
        "tool": "query-docs",
        "params": {"libraryId": "/vercel/next.js", "query": "routing"}
    })
```

### Direct Mode

Connect without a config file by specifying transport details directly:

```python
rye_execute(item_type="tool", item_id="rye/mcp/connect",
    parameters={
        "transport": "http",
        "url": "https://mcp.example.com/mcp",
        "headers": {"Authorization": "Bearer ${API_KEY}"},
        "tool": "my-tool",
        "params": {"input": "value"}
    })
```

### Environment Variable Resolution

Server configs support `${VAR}` and `${VAR:-default}` syntax. Variables are resolved from:

1. `.env` files: `~/.ai/.env`, `.ai/.env`, `.env`, `.env.local` (in order)
2. `os.environ`

### Output

```json
{
  "success": true,
  "tool": "query-docs",
  "content": [{ "type": "text", "text": "result content..." }],
  "isError": false
}
```

---

## `discover`

**Item ID:** `rye/mcp/discover`

Discover available tools on an MCP server. Returns tool names, descriptions, and input schemas.

### Parameters

| Name        | Type   | Required  | Description                                        |
| ----------- | ------ | --------- | -------------------------------------------------- |
| `transport` | string | ✅        | `stdio`, `http`, or `sse` (deprecated, use `http`) |
| `url`       | string | for HTTP  | Server URL                                         |
| `headers`   | object | ❌        | HTTP headers for authentication                    |
| `command`   | string | for stdio | Command to run                                     |
| `args`      | array  | ❌        | Command arguments                                  |
| `env`       | object | ❌        | Environment variables                              |

### Output

```json
{
  "success": true,
  "transport": "http (streamable)",
  "tools": [
    {
      "name": "query-docs",
      "description": "Query documentation for a library",
      "inputSchema": {"type": "object", "properties": {...}}
    }
  ],
  "count": 5
}
```

### Example

```python
# Discover tools from an HTTP MCP server
rye_execute(item_type="tool", item_id="rye/mcp/discover",
    parameters={
        "transport": "http",
        "url": "https://mcp.context7.com/mcp",
        "headers": {"CONTEXT7_API_KEY": "..."}
    })

# Discover tools from a stdio MCP server
rye_execute(item_type="tool", item_id="rye/mcp/discover",
    parameters={
        "transport": "stdio",
        "command": "npx",
        "args": ["-y", "@my-org/mcp-server"]
    })
```

---

## `manager`

**Item ID:** `rye/mcp/manager`

Manage MCP server configurations. Handles registration, discovery, tool config generation, and cleanup. The manager creates two types of files:

- **Server configs** at `.ai/tools/mcp/servers/<name>.yaml` — connection details
- **Tool configs** at `.ai/tools/mcp/<name>/<tool>.yaml` — per-tool definitions with schemas

### Actions

#### `add` — Register a new server

Connects to the server, discovers all tools, writes the server config and individual tool YAML files.

```python
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={
        "action": "add",
        "name": "context7",
        "transport": "http",
        "url": "https://mcp.context7.com/mcp",
        "headers": {"CONTEXT7_API_KEY": "${CONTEXT7_API_KEY}"},
        "scope": "user"  # or "project" (default)
    })
```

#### `list` — List registered servers

```python
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "list", "include_tools": true})
```

#### `refresh` — Re-discover tools for a server

Reconnects to the server, clears old tool configs, and writes new ones.

```python
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "refresh", "name": "context7"})
```

#### `remove` — Remove a server and its tools

Deletes the server config and all discovered tool configs from both project and user spaces.

```python
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "remove", "name": "context7"})
```

### Scoping

Servers can be scoped to **project** (`.ai/tools/mcp/`) or **user** (`~/.ai/tools/mcp/`). User-scoped servers are available to all projects.
