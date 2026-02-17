**Source:** Original implementation: `.ai/tools/rye/protocol/` in kiwi-mcp

# Protocol Category

## Purpose

Protocol tools implement **communication protocols** for RYE and tools.

**Location:** `.ai/tools/rye/core/protocol/`
**Count:** 1 tool
**Executor:** `python_runtime`
**Protected:** ✅ Yes (core tool - cannot be shadowed)

## Core Protocol Tools

### JSON-RPC Handler (`jsonrpc_handler.py`)

**Purpose:** Handle JSON-RPC protocol communications

```python
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "protocol"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "operation": {
            "type": "string",
            "enum": ["parse", "validate", "execute", "respond"]
        },
        "request": {"type": "object", "description": "JSON-RPC request"},
        "validate_schema": {"type": "boolean", "default": True},
    },
    "required": ["operation", "request"]
}

# Returns: {
#   "jsonrpc": "2.0",
#   "result": {...} or "error": {...},
#   "id": "...",
#   "valid": true
# }
```

**Operations:**
- `parse` - Parse JSON-RPC request
- `validate` - Validate against JSON-RPC spec
- `execute` - Execute the RPC method
- `respond` - Format JSON-RPC response

## JSON-RPC Details

### Request Format

```json
{
  "jsonrpc": "2.0",
  "method": "tool_name",
  "params": {
    "param1": "value1",
    "param2": "value2"
  },
  "id": "request-id-123"
}
```

### Response Format

```json
{
  "jsonrpc": "2.0",
  "result": {
    "output": "..."
  },
  "id": "request-id-123"
}
```

### Error Response

```json
{
  "jsonrpc": "2.0",
  "error": {
    "code": -32603,
    "message": "Internal error",
    "data": "Error details"
  },
  "id": "request-id-123"
}
```

## Error Codes

| Code | Meaning |
|------|---------|
| -32700 | Parse error |
| -32600 | Invalid Request |
| -32601 | Method not found |
| -32602 | Invalid params |
| -32603 | Internal error |

## Usage Examples

### Parse JSON-RPC Request

```bash
Call jsonrpc_handler with:
  operation: "parse"
  request: {
    "jsonrpc": "2.0",
    "method": "git",
    "params": {"command": "status"},
    "id": "req-123"
  }
```

### Validate JSON-RPC

```bash
Call jsonrpc_handler with:
  operation: "validate"
  request: {
    "jsonrpc": "2.0",
    "method": "my_tool",
    "params": {...},
    "id": "req-456"
  }
  validate_schema: true
```

### Execute JSON-RPC Method

```bash
Call jsonrpc_handler with:
  operation: "execute"
  request: {
    "jsonrpc": "2.0",
    "method": "git",
    "params": {"command": "status", "repo": "/path/to/repo"},
    "id": "req-789"
  }
```

## Metadata Pattern

All protocol tools follow this pattern:

```python
# .ai/tools/rye/protocol/{name}.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "python_runtime"
__category__ = "protocol"

CONFIG_SCHEMA = { ... }

def main(**kwargs) -> dict:
    """Handle protocol operation."""
    pass
```

## RYE Communication Model

```
LLM/User
    │
    └─→ JSON-RPC Request (via MCP)
        │
        ├─→ jsonrpc_handler (validate)
        ├─→ Check method exists
        ├─→ Validate parameters
        │
        └─→ Executor routes tool
            │
            ├─→ Load tool
            ├─→ Check __executor_id__
            ├─→ Route to primitive/runtime
            │
            └─→ Execute and get result
                │
                └─→ jsonrpc_handler (respond)
                    │
                    └─→ JSON-RPC Response
                        │
                        └─→ Back to LLM/User
```

## Key Characteristics

| Aspect | Detail |
|--------|--------|
| **Count** | 1 tool |
| **Location** | `.ai/tools/rye/protocol/` |
| **Executor** | `python_runtime` |
| **Purpose** | JSON-RPC protocol handling |
| **Use Cases** | Request validation, response formatting, error handling |

## Related Documentation

- [overview](../overview.md) - All categories
- [../mcp-server](../mcp-server.md) - MCP server architecture
- [../executor/routing](../executor/routing.md) - Tool execution routing
