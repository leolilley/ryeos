**Source:** RYE implementation analysis - current codebase structure

# RYE Package Structure

## Overview

RYE provides **the code implementation** that runs as an AI operating system layer on Lilux microkernel.

## Part 1: RYE Package Implementation (`rye/`)

**Location:** `/home/leo/projects/kiwi-mcp/rye-lilux/rye/`

**Purpose:** The core RYE OS kernel - Python implementation that:

- Provides MCP server interface to LLM clients (Claude, Cursor, etc.)
- Exposes **5 MCP tools** for interacting with `.ai/` items
- Implements type handlers for routing operations
- Wraps Lilux runtime services (AuthStore, EnvResolver)
- Orchestrates tool loading and execution
- Uses Lilux primitives for actual execution

### Directory Structure

```
rye/
├── __init__.py              # Package initialization
├── server.py                # MCP server (entry point: python -m rye.server)
├── handlers/                # Type-specific handlers
│   ├── directive/handler.py   # Directive routing and operations
│   ├── knowledge/handler.py   # Knowledge routing and operations
│   └── tool/handler.py       # Tool routing and operations
├── tools/                   # ← MCP tool implementations (NOT .ai/tools/)
│   ├── search.py            # mcp__rye__search implementation
│   ├── load.py              # mcp__rye__load implementation
│   ├── execute.py           # mcp__rye__execute implementation
│   ├── sign.py              # mcp__rye__sign implementation

└── utils/                   # Utility functions
```

**Important:** `rye/tools/` contains the **5 MCP tool implementations** - these are NOT the same as `.ai/tools/` which contains data-driven tool definitions.

### Key Components

#### 1. MCP Server (`server.py`)

**Purpose:** Entry point for RYE as an MCP server

```python
#!/usr/bin/env python3
"""
RYE MCP Server - AI operating system running on Lilux microkernel.

Exposes 5 MCP tools that load items from .ai/ on demand
and delegate to Lilux microkernel primitives for execution.
"""

class RYE:
    """RYE OS - Intelligence layer running on Lilux microkernel."""

    async def start(self):
        """Start RYE MCP server."""
        # Uses MCP stdio server
        # Registers 5 hardcoded MCP tools
        # Items in .ai/ loaded on demand when requested
```

**Key Features:**
- MCP stdio interface for Cursor/Claude connection
- Exposes 5 MCP tools: search, load, execute, sign, help
- Items in `.ai/` loaded on demand (no startup scanning)
- Dispatch to type handlers
- Uses Lilux primitives for actual execution

#### 2. The 5 MCP Tools (`rye/tools/`)

**Purpose:** Python implementations of MCP interface

| MCP Tool | File | Purpose |
|----------|------|---------|
| `mcp__rye__search` | `search.py` | Find items by keywords |
| `mcp__rye__load` | `load.py` | Load item content for inspection |
| `mcp__rye__execute` | `execute.py` | Run directives/tools, load knowledge |
| `mcp__rye__sign` | `sign.py` | Validate and sign items |


**These are hardcoded Python modules** - LLM calls these, and they load items from `.ai/` on demand.

#### 3. Type Handlers (`handlers/`)

**Purpose:** Route operations to appropriate handlers based on item type

```
Handler Registry
    ├─→ DirectiveHandler  (XML directives)
    ├─→ ToolHandler        (Multi-format tools: Python, YAML, JavaScript, Bash, etc.)
    └─→ KnowledgeHandler  (Markdown knowledge)
```

Each handler:
- Loads items from filesystem on demand
- Validates signatures and integrity
- Extracts metadata using SchemaExtractor
- Delegates to PrimitiveExecutor (via Lilux)
- Handles item-specific operations (sign, load, execute)

---

## Package Dependencies

### Lilux (Microkernel)

```toml
# lilux/pyproject.toml
[project]
name = "lilux"
version = "0.1.0"
description = "Lilux Microkernel - Generic execution primitives for AI agents"

dependencies = [
    "mcp>=1.0.0",
    "httpx>=0.27.0",
    "python-dotenv>=1.0.0",
    "keyring>=23.0.0",
]
```

**Role:** Provides dumb execution primitives (subprocess, HTTP, auth, environment, etc.)

### RYE (OS Layer)

```toml
# rye/pyproject.toml
[project]
name = "rye-lilux"
version = "0.1.0"
description = "RYE - AI operating system with universal tool executor running on Lilux microkernel"

dependencies = [
    "lilux>=0.1.0",
]

[project.scripts]
rye = "rye.server:main"

[build-system]
requires = ["setuptools>=61.0"]
build-backend = "setuptools.build_meta"

[tool.setuptools]
packages = ["rye"]
package_data = { "rye": [".ai/**/*"]}
```

**Role:** Provides intelligent executor + content understanding + on-demand loading + bundled tools

## Installation and Usage

### User Installation

```bash
# Users install RYE (gets both packages)
pip install rye-lilux

# RYE server starts automatically with LLM clients
# Claude Desktop / Cursor config:
{
  "mcpServers": {
    "rye": {
      "command": "/path/to/venv/bin/python",
      "args": ["-m", "rye.server"],
      "environment": {"USER_SPACE": "/home/user/.ai"},
      "enabled": true
    }
  }
}
```

### Developer Installation

```bash
# Install both packages for development
pip install -e lilux/
pip install -e rye/
```

## Related Documentation

- **Bundle Structure:** `[[bundle/structure]]` - Data-driven items in .ai/
- **Principles:** `[[principles]]` - RYE vs Lilux separation, on-demand loading model
- **MCP Tools:** `[[mcp-tools/overview]]` - 5 MCP tools architecture
- **Executor:** `[[executor/overview]]` - Tool discovery and routing
