# RYE-Lilux Documentation

## Overview

**RYE** is an AI operating system layer built on top of the **Lilux** microkernel. Together they provide a data-driven, universal tool execution platform for LLMs.

| Component | Role | Analogy |
|-----------|------|---------|
| **Lilux** | Microkernel with execution primitives | Hardware kernel |
| **RYE** | Executor + content bundle | Operating system |

## Architecture

```
LLM/User
    │
    └─→ RYE (5 MCP Tools)
        │
        │  Works with 3 item types:
        │  ├─→ directives (.ai/directives/)
        │  ├─→ tools (.ai/tools/)
        │  └─→ knowledge (.ai/knowledge/)
        │
        ├─→ search  - Find items by query
        ├─→ load    - Get content / copy between locations
        ├─→ execute - Run item (dispatches to handlers)
        ├─→ sign    - Validate and sign
        └─→ help    - Get help
            │
            │  Tool execution only:
            └─→ PrimitiveExecutor → Lilux Primitives
                ├─→ subprocess (shell commands)
                └─→ http_client (HTTP requests)
```

## Quick Links

### Core Concepts

- [RYE Principles](rye/principles.md) - Data-driven architecture principles
- [Executor Overview](rye/executor/overview.md) - Three-layer routing model

### RYE OS Layer

- [MCP Server](rye/mcp-server.md) - MCP server configuration
- [MCP Tools Overview](rye/mcp-tools/overview.md) - The 5 unified MCP tools
- [On-Demand Loading](rye/loading/overview.md) - How items are loaded
- [Executor](rye/executor/overview.md) - Tool routing and execution
- [Executor Components](rye/executor/components.md) - PrimitiveExecutor, ChainValidator, IntegrityVerifier
- [Chain Validator](rye/executor/chain-validator.md) - Tool chain validation

### MCP Tools (work with directive, tool, knowledge)

- [Search](rye/mcp-tools/search.md) - Find items by query
- [Load](rye/mcp-tools/load.md) - Load content / copy between locations
- [Execute](rye/mcp-tools/execute.md) - Run items (directives, tools, knowledge)
- [Sign](rye/mcp-tools/sign.md) - Validate and sign items


### Tool Categories

- [Categories Overview](rye/categories/overview.md) - All tool categories
- [Protection](rye/categories/protection.md) - Core vs App tools, shadowing rules

**Core Tools (Protected):**
- [Primitives](rye/categories/primitives.md) - Base executors (Layer 1)
- [Runtimes](rye/categories/runtimes.md) - Language runtimes (Layer 2)
- [Parsers](rye/categories/parsers.md) - Content preprocessors
- [Extractors](rye/categories/extractors.md) - Schema-driven metadata extraction

**App Tools (Replaceable):**
- [Threads](rye/categories/threads.md) - Async execution + capabilities
- [Registry](rye/categories/registry.md) - Tool distribution

### Bundle Structure

- [Content Bundle](rye/bundle/structure.md) - `.ai/` directory organization with core/ separation

### Lilux Microkernel

- [Lilux Principles](lilux/principles.md) - Dumb execution primitives
- [Package Structure](lilux/package/structure.md) - Lilux module organization
- [Primitives Overview](lilux/primitives/overview.md) - Subprocess, HTTP, integrity, lockfile
- [Runtime Services](lilux/runtime-services/overview.md) - AuthStore, EnvResolver
- [Schemas](lilux/schemas/overview.md) - JSON Schema validation utilities

## Installation

```bash
pip install rye-lilux  # Installs both RYE + Lilux
```

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
      }
    }
  }
}
```

## Usage Flow

1. **Search** for tools matching your needs
2. **Load** the tool schema to understand parameters
3. **Execute** the tool with appropriate parameters
4. **Sign** content when publishing to registry

## Project Status

**Architecture Complete - Implementation In Progress**
