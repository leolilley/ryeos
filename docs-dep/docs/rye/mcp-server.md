**Source:** Original implementation: `kiwi_mcp/server.py` adapted for RYE

# MCP Server Architecture

## Overview

RYE's MCP server exposes a **universal tool interface** to LLMs via the Model Context Protocol.

## Lilux vs RYE MCP Relationship

| Aspect             | Lilux (Microkernel)   | RYE (OS)                            |
| ------------------ | --------------------- | ----------------------------------- |
| **Entry Point**    | Not used (dependency) | `python -m rye.server`              |
| **LLM Calls**      | `mcp__lilux__*`       | `mcp__rye__*`                       |
| **User Install**   | `pip install lilux`   | `pip install rye-lilux` (gets both) |
| **Tool Discovery** | Fixed tool list       | Fixed 5 tools, items loaded on demand |
| **Primary Use**    | Dependency only       | Main interface for LLMs             |

## MCP Configuration

### Claude Desktop

```json
{
  "mcpServers": {
    "rye": {
      "command": "/path/to/venv/bin/python",
      "args": ["-m", "rye.server"],
      "environment": {
        "USER_SPACE": "/home/user/.ai"
      },
      "enabled": true
    }
  }
}
```

### Cursor IDE

```json
{
  "mcpServers": {
    "rye": {
      "command": "/path/to/venv/bin/python",
      "args": ["-m", "rye.server"],
      "environment": {
        "USER_SPACE": "/home/user/.ai"
      },
      "alwaysAllow": [
        "mcp__rye__search",
        "mcp__rye__load",
        "mcp__rye__execute",
        "mcp__rye__sign"
      ]
    }
  }
}
```

## The 5 Universal MCP Tools

RYE exposes exactly **5 MCP tools** that work with **3 item types**:

| Item Type | Description | Location |
|-----------|-------------|----------|
| `directive` | Workflow definitions | `.ai/directives/` |
| `tool` | Executable tools | `.ai/tools/` |
| `knowledge` | Knowledge entries | `.ai/knowledge/` |
| `system` | Pre-packaged RYE tools | `{install_location}/.ai/tools/rye/core/system/` |
| `all` | All item types | Searches all above |

### 1. Search (`mcp__rye__search`)

**Purpose:** Search for items across directives, tools, or knowledge

```
Request:
{
  "item_type": "directive" | "tool" | "knowledge",
  "query": "lead generation",
  "source": "project" | "user" | "system",
  "limit": 10,
  "project_path": "/path/to/project"
}
```
Request:
{
  "item_type": "directive" | "tool" | "knowledge",
  "query": "lead generation",
  "source": "project" | "user" | "system",
  "limit": 10,
  "project_path": "/path/to/project"
}

 Response:
{
  "results": [...],
  "total": 5,
  "query": "lead generation",
  "search_type": "keyword"
}
```

### 2. Load (`mcp__rye__load`)

**Purpose:** Load item content or copy between locations

```
Request:
{
  "item_type": "directive" | "tool" | "knowledge",
  "item_id": "create_tool",
  "source": "project" | "user" | "system",
  "destination": "project" | "user",  # Optional - omit for read-only
  "project_path": "/path/to/project"
}

Response:
{
  "content": "...",
  "metadata": {...},
  "path": "..."
}
```

### 3. Execute (`mcp__rye__execute`)

**Purpose:** Execute an item

```
Request:
{
  "item_type": "directive" | "tool" | "knowledge",
  "item_id": "scraper",
  "parameters": {"url": "https://..."},
  "project_path": "/path/to/project"
}

Response:
{
  "status": "success",
  "data": {...},
  "metadata": {"duration_ms": 123}
}
```

**Execution behavior by item type:**
- `directive`: Returns parsed XML for agent to follow
- `tool`: Executes tool via executor chain → primitives
- `knowledge`: Returns knowledge content

### 4. Sign (`mcp__rye__sign`)

**Purpose:** Validate and sign an item file

```
Request:
{
  "item_type": "directive" | "tool" | "knowledge",
  "item_id": "my_directive",
  "project_path": "/path/to/project",
  "parameters": {"location": "project"}
}

Response:
{
  "status": "signed",
  "path": "...",
  "signature": "..."
}
```

**Batch signing:** Use glob patterns like `demos/meta/*`

## Server Startup

```bash
python -m rye.server
```

**Process:**
1. Initialize RYE server
2. Register 4 MCP tools
3. Start listening for LLM requests

That's it. No scanning. No registry building.

## Tool Discovery Flow

```
Server Startup
    │
    └─→ Register 4 MCP tools
        ├─→ mcp__rye__search
        ├─→ mcp__rye__load
        ├─→ mcp__rye__execute
        └─→ mcp__rye__sign
        
That's it. No scanning. No registry building.
Items are loaded on demand when LLM calls these tools.
```

## Request/Response Flow

```
LLM
    │
    └─→ JSON-RPC Request (MCP)
        │
        ├─→ mcp__rye__search
        │   └─→ Search .ai/ directories on demand
        │
        ├─→ mcp__rye__load
        │   └─→ Load item content from disk
        │
        ├─→ mcp__rye__execute
        │   ├─→ Executor routes
        │   ├─→ ENV_CONFIG resolution
        │   ├─→ Lilux primitive execution
        │   └─→ Result returned
        │
        └─→ mcp__rye__sign
            └─→ Sign with credentials
                │
                └─→ JSON-RPC Response (MCP)
                    │
                    └─→ Back to LLM
```

## Configuration

### Environment Variables

| Variable | Purpose | Default |
|----------|---------|---------|
| `USER_SPACE` | User tool directory | `~/.ai` |
| `RYE_DEBUG` | Enable debug logging | `false` |
| `RYE_PORT` | Server port | `8000` |

### Server Settings

```python
# rye/server.py (pseudocode)

class RYEServer:
    def __init__(self):
        self.user_space = os.getenv("USER_SPACE", "~/.ai")
        self.debug = os.getenv("RYE_DEBUG", "false").lower() == "true"
    
    def start(self):
        # Register 5 MCP tools
        self._register_mcp_tools()
        
        # Start listening
        self._start_server()
    
    def _register_mcp_tools(self):
        """Register 4 universal MCP tools. That's all."""
        self.register_tool("mcp__rye__search", self.search)
        self.register_tool("mcp__rye__load", self.load)
        self.register_tool("mcp__rye__execute", self.execute)
        self.register_tool("mcp__rye__sign", self.sign)
```

## Tool Names from LLM Perspective

The LLM sees **exactly 4 tools** in the MCP tool listing:

- `mcp__rye__search` - Search for items (directives, tools, knowledge)
- `mcp__rye__load` - Load item content
- `mcp__rye__execute` - Execute an item
- `mcp__rye__sign` - Validate and sign an item

Everything else (actual tools, directives, knowledge) is accessed **through** these 5 tools.

## Source Locations

| Source    | Path                                      | Description         |
| --------- | ----------------------------------------- | ------------------- |
| `project` | `{project_path}/.ai/`                     | Project-local items |
| `user`    | `~/.ai/` (or `USER_SPACE` env var)       | User-global items   |
| `system`   | `{install_location}/.ai/`                | Pre-packaged RYE tools |

## LLM Workflow

### Example: Execute Git Command

```
LLM: "Check git status"
    │
    ├─→ Call mcp__rye__search("git")
    │   └─→ Returns: [{name: "git", category: "capabilities", ...}]
    │
    ├─→ Call mcp__rye__load("git")
    │   └─→ Returns: {schema: {...}, description: "..."}
    │
    └─→ Call mcp__rye__execute({
            name: "git",
            parameters: {command: "status"}
        })
        └─→ Returns: {status: "success", result: {...}}
             │
             └─→ LLM presents result to user
```

**Note:** Execute tool resolves `item_id` to a file path - works with all sources (project, user, system) automatically based on item location. Search uses `source` parameter to filter where to look.

## Key Points

- **RYE is the main MCP entry point** - not Lilux
- **LLM talks to RYE** → RYE executor routes to Lilux
- **User installs RYE** → gets OS + microkernel
- **LLM sees exactly 5 MCP tools** → search, load, execute, sign, help
- **Items loaded on demand** via those 5 tools
- **No startup scanning** → no "dynamic registry" concept
- **Everything accessed through the 5 tools** → directives, tools, knowledge

## Related Documentation

- [[executor/routing]] - How tools are executed
- [[executor/overview]] - Executor architecture
- [[categories/mcp]] - MCP tools in RYE
- [[bundle/structure]] - Bundle organization
