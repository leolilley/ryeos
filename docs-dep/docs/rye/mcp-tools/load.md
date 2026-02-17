# Load Tool (`mcp__rye__load`)

## Purpose

Load item content for inspection or copy items between locations (project ↔ user).

## Behavior

- **destination omitted or equals source:** Read-only mode - returns content without copying
- **destination differs from source:** Copies item to destination location
- **system source:** Immutable (read-only) - system tools are in site-packages and cannot be modified in-place

## Source Locations

| Source    | Path                                     | Description                  |
| --------- | ---------------------------------------- | --------------------------- |
| `project` | `{project_path}/.ai/`                      | Project-local items          |
| `user`    | `~/.ai/` (or `USER_SPACE` env var)        | User-global items           |
| `system`   | `{install_location}/.ai/`                 | Pre-packaged RYE tools       |

## Request Schema

```json
{
  "item_type": "directive" | "tool" | "knowledge",  // Required
  "item_id": "string",                               // Required
  "source": "project" | "user",                      // Required
  "destination": "project" | "user",                 // Optional
  "project_path": "/path/to/project"                 // Required
}
```

## Response Schema

```json
{
  "content": "string",           // Item content
  "metadata": {
    "name": "string",
    "version": "string",
    "...": "..."
  },
  "path": "string",              // File path
  "source": "project" | "user"
}
```

## Examples

### Inspect a Directive (Read-Only)

**Request:**

```json
{
  "item_type": "directive",
  "item_id": "create_tool",
  "source": "project",
  "project_path": "/home/user/myproject"
}
```

**Response:**

```json
{
  "content": "<directive name=\"create_tool\" version=\"1.0.0\">...",
  "metadata": {
    "name": "create_tool",
    "version": "1.0.0",
    "description": "Create a new tool"
  },
  "path": "/home/user/myproject/.ai/directives/create_tool.md",
  "source": "project"
}
```

### Copy Tool from User to Project

**Request:**

```json
{
  "item_type": "tool",
  "item_id": "scraper",
  "source": "user",
  "destination": "project",
  "project_path": "/home/user/myproject"
}
```

**Response:**

```json
{
  "content": "...",
  "metadata": {...},
  "path": "/home/user/myproject/.ai/tools/scraper.py",
  "source": "user",
  "copied_to": "project"
}
```

### Load Knowledge Entry

**Request:**

```json
{
  "item_type": "knowledge",
  "item_id": "api_patterns",
  "source": "project",
  "project_path": "/home/user/myproject"
}
```

## Use Cases

| Use Case                  | Source  | Destination | Effect          | For Item Type |
| ------------------------- | ------- | ----------- | --------------- | ------------- |
| Inspect before executing  | project | (omit)      | Returns content | All types     |
| Copy from user to project | user    | project     | Copies file     | All types     |
| Copy from project to user | project | user        | Copies file     | All types     |

## Knowledge: Load vs Execute

**Use `load` when:** You want to read raw content (e.g., to inspect file, copy to another location)

**Use `execute` when:** You want knowledge returned as structured data for agent consumption

| Tool      | Purpose                | Returns                 | For Knowledge                                                 |
| --------- | ---------------------- | ----------------------- | ------------------------------------------------------------- |
| `load`    | Read content or copy   | Raw content string      | Returns markdown + frontmatter for inspection                 |
| `execute` | Return structured data | Parsed knowledge object | Returns structured data: `{id, title, content}` for agent use |

## Related Documentation

- `[[categories/registry]]` - Registry tool for package distribution

## Handler Dispatch

```
mcp__rye__load(item_type="tool", item_id="scraper", source="user", destination="project")
    │
    └─→ ToolHandler.load(item_id, source, destination)
        │
        ├─→ Read from source
        └─→ Copy to destination (if specified)
```

## Related Documentation

- [[../mcp-server]] - MCP server architecture
- [[search]] - Search for items
- [[execute]] - Execute items
