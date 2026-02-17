# Execute Tool (`mcp__rye__execute`)

## Purpose

Execute a directive, tool, or knowledge item. Behavior varies by item type.

## Request Schema

```json
{
  "item_type": "directive" | "tool" | "knowledge" | "all",  // Required
  "item_id": "string",                               // Required (omit for "all")
  "project_path": "/path/to/project",                // Required
  "parameters": {                                    // Optional
    "...": "..."
  }
}
```

**Note:** Execute tool resolves `item_id` to `.ai/tools/` locations - supports all sources (project, user, system) automatically based on item location.

## Response Schema

```json
{
  "status": "success" | "error",
  "data": {...},                    // Item-type specific result
  "metadata": {
    "duration_ms": 123,
    "tool_type": "string",
    "primitive_type": "subprocess" | "http_client"
  }
}
```

## Execution by Item Type

### Directive Execution

Returns parsed XML for the agent to follow.

**Request:**
```json
{
  "item_type": "directive",
  "item_id": "create_tool",
  "project_path": "/home/user/myproject",
  "parameters": {"tool_name": "my_scraper"}
}
```

**Response:**
```json
{
  "status": "success",
  "data": {
    "name": "create_tool",
    "version": "1.0.0",
    "inputs": [...],
    "process": {...},
    "outputs": {...}
  }
}
```

### Tool Execution

Executes the tool through the executor chain to primitives.

**Request:**
```json
{
  "item_type": "tool",
  "item_id": "scraper",
  "project_path": "/home/user/myproject",
  "parameters": {
    "url": "https://example.com",
    "dry_run": false
  }
}
```

**Response:**
```json
{
  "status": "success",
  "data": {
    "stdout": "Scraped 100 items...",
    "return_code": 0
  },
  "metadata": {
    "duration_ms": 1234,
    "tool_type": "python",
    "primitive_type": "subprocess"
  }
}
```

### Knowledge Execution

Returns knowledge content as **structured data** to inform agent decisions. Unlike `load`, which returns raw content string, `execute` parses and returns knowledge as an object.

**Request:**
```json
{
  "item_type": "knowledge",
  "item_id": "api_patterns",
  "project_path": "/home/user/myproject"
}
```

**Response:**
```json
{
  "status": "success",
  "data": {
    "id": "api_patterns",
    "title": "API Design Patterns",
    "content": "..."
  }
}
```

## Tool Execution Flow

For `item_type="tool"`, execution flows through the chain resolver to primitives:

```
mcp__rye__execute(item_type="tool", item_id="scraper", parameters={...})
    │
    └─→ ToolHandler.execute(item_id, parameters)
        │
        ├─→ Resolve tool file
        ├─→ Validate signature & integrity
        ├─→ Validate parameters against CONFIG_SCHEMA
        │
        └─→ PrimitiveExecutor.execute()
            │
            ├─→ Resolve executor chain (tool → runtime → primitive)
            ├─→ Verify integrity at each step
            │
            └─→ Execute via primitive
                ├─→ subprocess (shell commands)
                └─→ http_client (HTTP requests)
```

## Dry Run Mode

For tools, `dry_run: true` validates without executing:

**Request:**
```json
{
  "item_type": "tool",
  "item_id": "scraper",
  "project_path": "/home/user/myproject",
  "parameters": {"dry_run": true}
}
```

**Response:**
```json
{
  "status": "validation_passed",
  "message": "Tool is ready to execute",
  "metadata": {...}
}
```

## Error Response

```json
{
  "status": "error",
  "error": "Tool validation failed",
  "details": ["Missing required version metadata"],
  "path": "/path/to/tool.py",
  "solution": "Add version metadata and retry"
}
```

## Knowledge: Execute vs Load

**Use `execute` when:** You want knowledge as structured data for agent consumption

**Use `load` when:** You want raw content (e.g., to inspect file, copy to another location)

| Tool | Purpose | Returns for Knowledge | When to Use |
|------|---------|---------------------|------------|
| `execute` | Return structured data | Parsed object: `{id, title, content}` for agent reasoning/decision-making |
| `load` | Read content or copy | Raw markdown + frontmatter string | For inspection, viewing, or copying files |

**Example:** Execute returns structured data the agent can use directly:
```json
{
  "status": "success",
  "data": {
    "id": "api_patterns",
    "title": "API Design Patterns",
    "content": "REST APIs should use HTTP verbs..."
  }
}
```

**Example:** Load returns raw content the agent must parse:
```json
{
  "content": "<!-- frontmatter -->\n# API Design Patterns\n\nREST APIs should use...",
  "metadata": {...}
}
```

## Related Documentation

- [[../mcp-server]] - MCP server architecture
- [[../executor/overview]] - Executor routing
- [[search]] - Search for items
- [[load]] - Load item content
