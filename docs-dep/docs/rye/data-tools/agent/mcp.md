**Source:** Original implementation: `.ai/tools/rye/mcp/` in kiwi-mcp

# MCP Category

## Purpose

MCP tools implement **Model Context Protocol** functionality for RYE.

**Location:** `.ai/tools/rye/mcp/`  
**Count:** 3 tools + YAML configs  
**Executor:** Python tools use `python_runtime`

## Core MCP Tools

### 1. MCP Call (`mcp_call.py`)

**Purpose:** Execute MCP method calls

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "mcp"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "resource": {"type": "string", "description": "MCP resource"},
        "method": {"type": "string", "description": "Method to call"},
        "params": {"type": "object", "description": "Method parameters"},
        "timeout": {"type": "integer", "default": 30},
    },
    "required": ["resource", "method"]
}
```

### 2. MCP Server (`mcp_server.py`)

**Purpose:** Run MCP server instance

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "mcp"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "config_file": {"type": "string", "description": "MCP config file path"},
        "port": {"type": "integer", "default": 8000},
        "workers": {"type": "integer", "default": 4},
    },
    "required": ["config_file"]
}
```

### 3. MCP Client (`mcp_client.py`)

**Purpose:** Create MCP client connections

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "mcp"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "endpoint": {"type": "string", "description": "MCP endpoint"},
        "protocol": {"type": "string", "enum": ["stdio", "http", "ws"], "default": "http"},
        "auth": {"type": "object", "description": "Authentication config"},
    },
    "required": ["endpoint"]
}
```

## MCP Configurations

### MCP Stdio (`mcp_stdio.yaml`)

```yaml
name: mcp_stdio
version: "1.0.0"
type: runtime
executor_id: subprocess
category: mcp

config:
  command: "${RYE_MCP_RUNNER}"
  args: ["--type", "stdio"]

env_config:
  mcp_runner:
    type: binary
    search: ["project", "system"]
    var: "RYE_MCP_RUNNER"
    fallback: "mcp"
```

### MCP HTTP (`mcp_http.yaml`)

```yaml
name: mcp_http
version: "1.0.0"
type: runtime
executor_id: http_client
category: mcp

config:
  method: POST
  url: "${MCP_ENDPOINT}"
  headers:
    Content-Type: application/json
    Authorization: "Bearer ${MCP_TOKEN}"
  timeout: 30

env_config:
  mcp_endpoint:
    type: url
    var: "MCP_ENDPOINT"
    fallback: "http://localhost:8000"
  mcp_token:
    type: credential
    key: "mcp_auth_token"
    var: "MCP_TOKEN"
```

### MCP WebSocket (`mcp_ws.yaml`)

```yaml
name: mcp_ws
version: "1.0.0"
type: runtime
executor_id: subprocess
category: mcp

config:
  command: "${RYE_MCP_WS_CLIENT}"
  args:
    - "connect"
    - "${MCP_WS_URL}"

env_config:
  mcp_ws_url:
    type: url
    var: "MCP_WS_URL"
    fallback: "ws://localhost:8000"
```

## MCP Protocol Overview

### Resource Access Pattern

```
Tool
    │
    └─→ mcp_call(resource="tools", method="list")
        │
        └─→ MCP Protocol
            ├─ Route to MCP server
            ├─ Execute method
            └─→ Return results
```

### Standard MCP Methods

| Method | Purpose |
|--------|---------|
| `resources.list` | List available resources |
| `resources.read` | Read resource content |
| `tools.list` | List available tools |
| `tools.call` | Call a tool |
| `prompts.list` | List prompts |
| `prompts.get` | Get prompt |

## Metadata Pattern

All MCP tools follow this pattern:

```python
# .ai/tools/rye/mcp/{name}.py or {name}.yaml

__version__ = "1.0.0"
__tool_type__ = "python" or "runtime"
__executor_id__ = "python_runtime" or "http_client" or "subprocess"
__category__ = "mcp"

CONFIG_SCHEMA = { ... }  # For Python tools
# or YAML structure for config files
```

## Usage Examples

### Call MCP Resource

```bash
Call mcp_call with:
  resource: "tools"
  method: "list"
  timeout: 30
```

### Create MCP Client

```bash
Call mcp_client with:
  endpoint: "http://mcp-server:8000"
  protocol: "http"
  auth:
    type: "bearer"
    token: "..."
```

### Start MCP Server

```bash
Call mcp_server with:
  config_file: "/etc/rye/mcp.yaml"
  port: 8000
  workers: 4
```

## MCP Integration

```
LLM/User
    │
    └─→ RYE MCP Server
        │
        ├─→ Exposes 5 tools (search, load, execute, sign, help)
        │
        └─→ Tools call internal resources
            ├─ Discover tools (.ai/tools/)
            ├─ Load tool metadata
            ├─ Execute tools
            └─ Sign content
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 3 tools + 3 configs |
| **Location** | `.ai/tools/rye/mcp/` |
| **Executor** | python_runtime, http_client, subprocess |
| **Purpose** | MCP protocol support |
| **Protocols** | Stdio, HTTP, WebSocket |

## Related Documentation

- [overview](../overview.md) - All categories
- [../mcp-server](../mcp-server.md) - RYE MCP server architecture
- [../bundle/structure](../bundle/structure.md) - Bundle organization
