<!-- rye:signed:2026-02-26T05:52:24Z:74bfd1ae79f31c8eb173dabc4a0f1267f5aca700d7f8c14b5d5545a76ea5afb5:3uytmqEQbVZ5-oCgq7fRNhgfgPxGmHUBu5wVe1rTVdLC9NhTWBczKckBmByMacJCKV2nx9Rk1m11Fq4nr_6QBw==:4b987fd4e40303ac -->

```yaml
name: mcp-integration
title: MCP Client Integration
entry_type: reference
category: rye/mcp
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - mcp
  - servers
  - integration
  - tools
references:
  - "docs/standard-library/tools/mcp.md"
```

# MCP Client Integration

Three tools for integrating with external MCP servers. Rye OS acts as an MCP **client** in a hub-and-spoke architecture.

## Namespace & Runtime

| Field       | Value                                       |
| ----------- | ------------------------------------------- |
| Namespace   | `rye/mcp/`                                  |
| Runtime     | `python/script`                     |
| Executor ID | `rye/core/runtimes/python/script`   |
| Dependency  | `pip install mcp httpx`                     |

## Transport Types

| Transport | Use Case         | Required Param | Protocol                       |
| --------- | ---------------- | -------------- | ------------------------------ |
| `http`    | Remote servers   | `url`          | Streamable HTTP (MCP 2025-03-26) |
| `stdio`   | Local CLI tools  | `command`      | stdin/stdout                   |
| `sse`     | Deprecated       | `url`          | Use `http` instead             |

## Server Lifecycle

```
add → discover → connect → refresh → remove
```

1. **add** (manager) — register server, discover tools, write configs
2. **discover** — list available tools and schemas from a server
3. **connect** — execute a tool call on a server
4. **refresh** (manager) — re-discover tools, update configs
5. **remove** (manager) — delete server and tool configs

---

## `connect`

**Item ID:** `rye/mcp/connect`

Execute a tool call on an MCP server. Two modes of operation.

### Server Config Mode

```python
rye_execute(item_type="tool", item_id="rye/mcp/connect",
    parameters={
        "server_config": ".ai/tools/mcp/servers/context7.yaml",
        "tool": "query-docs",
        "params": {"libraryId": "/vercel/next.js", "query": "routing"}
    })
```

### Direct Mode

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

### Parameters (Direct Mode)

| Name        | Type   | Required  | Description                     |
| ----------- | ------ | --------- | ------------------------------- |
| `transport` | string | ✅        | `http` or `stdio`              |
| `tool`      | string | ✅        | Tool name to call               |
| `params`    | object | ❌        | Tool parameters                 |
| `url`       | string | for HTTP  | Server URL                      |
| `headers`   | object | ❌        | HTTP headers                    |
| `command`   | string | for stdio | Command to run                  |
| `args`      | array  | ❌        | Command arguments               |
| `env`       | object | ❌        | Environment variables           |
| `timeout`   | int    | ❌        | Timeout in seconds (default: 30)|

### Parameters (Server Config Mode)

| Name            | Type   | Required | Description                |
| --------------- | ------ | -------- | -------------------------- |
| `server_config` | string | ✅       | Path to server YAML config |
| `tool`          | string | ✅       | Tool name to call          |
| `params`        | object | ❌       | Tool parameters            |

### Return

```json
{
  "success": true,
  "tool": "query-docs",
  "content": [{"type": "text", "text": "result content..."}],
  "isError": false
}
```

---

## `discover`

**Item ID:** `rye/mcp/discover`

Discover available tools on an MCP server.

### Parameters

| Name        | Type   | Required  | Description                      |
| ----------- | ------ | --------- | -------------------------------- |
| `transport` | string | ✅        | `stdio`, `http`, or `sse`       |
| `url`       | string | for HTTP  | Server URL                       |
| `headers`   | object | ❌        | HTTP headers for auth            |
| `command`   | string | for stdio | Command to run                   |
| `args`      | array  | ❌        | Command arguments                |
| `env`       | object | ❌        | Environment variables            |

### Timeouts

| Transport | Discovery Timeout |
| --------- | ----------------- |
| stdio     | 10 seconds        |
| http      | 30 seconds        |

### Return

```json
{
  "success": true,
  "transport": "http (streamable)",
  "tools": [
    {
      "name": "query-docs",
      "description": "Query documentation",
      "inputSchema": {"type": "object", "properties": {...}}
    }
  ],
  "count": 5
}
```

---

## `manager`

**Item ID:** `rye/mcp/manager`

Manage MCP server configurations.

### Actions

| Action    | Required Params        | Description                              |
| --------- | ---------------------- | ---------------------------------------- |
| `add`     | `name`, `transport`    | Register server, discover tools          |
| `list`    | —                      | List registered servers                  |
| `refresh` | `name`                 | Re-discover tools for existing server    |
| `remove`  | `name`                 | Delete server and tool configs           |

### `add` Parameters

| Name        | Type   | Required  | Default     | Description               |
| ----------- | ------ | --------- | ----------- | ------------------------- |
| `name`      | string | ✅        | —           | Server name               |
| `transport` | string | ✅        | —           | `http` or `stdio`        |
| `url`       | string | for HTTP  | —           | Server URL                |
| `headers`   | object | ❌        | —           | HTTP headers              |
| `command`   | string | for stdio | —           | Command to run            |
| `args`      | array  | ❌        | —           | Command arguments         |
| `env`       | object | ❌        | —           | Environment variables     |
| `scope`     | string | ❌        | `"project"` | `"project"` or `"user"`  |
| `timeout`   | int    | ❌        | `30`        | Timeout in seconds        |

### `list` Parameters

| Name            | Type | Required | Default | Description          |
| --------------- | ---- | -------- | ------- | -------------------- |
| `include_tools` | bool | ❌       | `false` | Include tool names   |

### File Structure

```
.ai/tools/mcp/
├── servers/
│   └── context7.yaml          # Server config
└── context7/
    ├── query-docs.yaml        # Tool config
    └── resolve-library-id.yaml
```

- **Server configs**: connection details, transport, auth
- **Tool configs**: per-tool YAML with schemas, server reference, runtime mapping

### Scoping

| Scope     | Path                           | Visibility       |
| --------- | ------------------------------ | ----------------- |
| `project` | `.ai/tools/mcp/`              | Current project   |
| `user`    | `~/.ai/tools/mcp/`            | All projects      |

### Environment Variable Resolution

Server configs support `${VAR}` and `${VAR:-default}` syntax. Resolution order:

1. `.env` files: `~/.ai/.env` → `.ai/.env` → `.env` → `.env.local`
2. `os.environ`

### Invocation Examples

```python
# Add a server
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={
        "action": "add",
        "name": "context7",
        "transport": "http",
        "url": "https://mcp.context7.com/mcp",
        "headers": {"CONTEXT7_API_KEY": "${CONTEXT7_API_KEY}"}
    })

# List servers with tools
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "list", "include_tools": true})

# Refresh
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "refresh", "name": "context7"})

# Remove
rye_execute(item_type="tool", item_id="rye/mcp/manager",
    parameters={"action": "remove", "name": "context7"})
```

## Error Conditions

| Error                              | Tool     | Cause                                    |
| ---------------------------------- | -------- | ---------------------------------------- |
| MCP SDK not available              | all      | `mcp` / `httpx` not installed            |
| Timeout                            | all      | Server unresponsive                      |
| Server already exists              | manager  | `add` with duplicate name                |
| Server not found                   | manager  | `refresh`/`remove` with unknown name     |
| No URL / No command                | connect  | Missing required transport param         |
| Server config not found            | connect  | YAML file path doesn't exist             |
