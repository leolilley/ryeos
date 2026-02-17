**Source:** Original implementation: `.ai/tools/rye/` and tool organization in kiwi-mcp

# Data Tools Overview

## Purpose

This overview describes all data tools in RYE and how they relate to each other.

**Data Tools** are functional components that make up the RYE system, organized into logical groups. Unlike MCP tools which are interfaces exposed to LLMs, data tools represent actual functional modules and their organization.

## Tool Organization

**Key Distinction:** Tools are split into **Core Infrastructure** (immutable, essential for RYE MCP) and **Feature Groups** (mutable, optional, replaceable).

**See Also:** [[../../executor/tool-resolution-and-validation.md]] for complete details on tool spaces, mutability, and shadowing behavior.

```
 .ai/tools/
 ├── rye/                              # Bundled RYE data tools
 │   │
 │   ├── core/                         # ⚠️ Immutable - System space (essential for RYE MCP)
 │   │   ├── primitives/               #   Layer 1: Base executors
│   │   ├── runtimes/                 #   Layer 2: Language runtimes
│   │   ├── parsers/                  #   Data format parsing
│   │   ├── extractors/               #   Data extraction
│   │   ├── sinks/                   #   Event output
│   │   ├── sinks/                    #   Event output
│   │   ├── protocol/                 #   Communication protocols
│   │   └── system/                   #   System info
│   │
│   ├── agent/                        # LLM agent execution within RYE
│   │   ├── threads/                  #   Thread management
│   │   │   ├── capabilities/         #     Sandboxing
│   │   │   └── capability_tokens/    #     Token management
│   │   ├── telemetry/                #   Monitoring agent runs
│   │   ├── llm/                      #   LLM provider configs
│   │   └── mcp/                      #   MCP protocol for agent comms
│   │
│   ├── registry/                     # Tool distribution (standalone)
│   └── rag/                          # Semantic search (optional)
│
└── {other}/                          # User/custom categories
    └── {tools...}
```

**See Also:** [[../../executor/tool-resolution-and-validation.md]] for complete tool spaces, precedence, and shadowing behavior.

## Core Infrastructure (IMMUTABLE - System Space)

Core tools live under `.ai/tools/rye/core/` and are essential for RYE MCP to function.

**System tools are immutable** (read-only, in site-packages). Users CAN shadow them by creating同名工具 in project or user space (for customization). See [[../../executor/tool-resolution-and-validation.md]] for details.

### 1. **Primitives** (2 execution primitives)
**Location:** `lilux/primitives/` (code) + `.ai/tools/rye/core/primitives/` (schemas)

Hardcoded execution engines - the terminal nodes of all tool execution.

**Key:** `__executor_id__ = None` (no delegation)

- subprocess - Shell command execution
- http_client - HTTP requests

**Note:** Only these 2 contain actual execution code. Everything else routes to them.

**See:** [core/primitives](core/primitives.md)

### 2. **Runtimes** (3 tools)
**Location:** `.ai/tools/rye/core/runtimes/`

Language-specific executors - Layer 2 of three-layer architecture.

**Key:** `__executor_id__` points to primitives, declares `ENV_CONFIG`

- python_runtime - Python script execution
- node_runtime - Node.js execution
- mcp_http_runtime - MCP HTTP client

**See:** [core/runtimes](core/runtimes.md)

### 3. **Parsers** (4 tools)
**Location:** `.ai/tools/rye/core/parsers/`

Data format parsing for content files.

**Key:** All use `python_runtime`

- markdown_xml - Parse Markdown + XML
- frontmatter - Parse YAML frontmatter + Markdown
- python_ast - Parse Python code
- yaml - Parse YAML configs

**See:** [core/parsers](core/parsers.md)

### 4. **Extractors** (3 subdirectories)
**Location:** `.ai/tools/rye/core/extractors/`

Data extraction from content files.

**Key:** All use `python_runtime`

**Subdirectories:**
- directive/ - Extract from XML directives
- knowledge/ - Extract from Markdown knowledge
- tool/ - Extract metadata from tools in any format (Python, YAML, JavaScript, Bash, TOML, etc.)

**See:** [core/extractors](core/extractors.md)

### 6. **Protocol** (1 tool)
**Location:** `.ai/tools/rye/core/protocol/`

Communication protocol implementations.

**Key:** Uses `python_runtime`

- jsonrpc_handler - JSON-RPC protocol

**See:** [core/protocol](core/protocol.md)

---

## Feature Groups (NOT PROTECTED)

These are optional features delivered via RYE. Users can shadow/replace these.

### 7. **Threads** (12 tools + YAML + Capabilities)
**Location:** `.ai/tools/rye/threads/`

Async execution and thread management.

**Key:** All use `python_runtime`

**Tools:**
- thread_create, thread_read, thread_update, thread_delete
- message_add, message_read, message_update, message_delete
- run_create, run_read, run_update, run_step_read

**Configs:**
- anthropic_thread.yaml
- openai_thread.yaml

**Subdirectory: Capabilities** (`threads/capabilities/`)

Sandboxing for LLM-driven execution. **Only enforced when running threads**, NOT for direct MCP calls.

- git - Version control
- fs - Filesystem operations
- db - Database operations
- net - Network operations
- process - Process management

**See:** [agent/threads](agent/threads.md), [agent/capabilities](agent/capabilities.md)

### 8. **Telemetry** (6 tools)
**Location:** `.ai/tools/rye/telemetry/`

System monitoring and diagnostics.

**Key:** All use `python_runtime`

- telemetry_configure - Configure collection
- telemetry_status - Get status
- telemetry_clear - Clear data
- telemetry_export - Export data
- lib_configure - Configure libraries
- health_check - System health

**See:** [agent/telemetry](agent/telemetry.md)

### 9. **RAG** (4 tools) **[OPTIONAL]**
**Location:** `.ai/tools/rye/rag/`

Semantic search and document embedding capabilities.

**Key:** All use `http_client` for embedding APIs

- rag_index - Index documents into vector store
- rag_search - Semantic search in vector store
- rag_embed - Embed single document
- rag_delete - Remove from vector store

**Note:** Completely optional. RYE works without RAG using keyword search.

**See:** [rag](rag/rag.md)

### 10. **Sinks** (3 tools)
**Location:** `.ai/tools/rye/sinks/`

Event destination handlers.

**Key:** All use `python_runtime`

- file_sink - Write to files
- null_sink - Discard events
- websocket_sink - Send to WebSocket

**See:** [core/sinks](core/sinks.md)

### 11. **MCP** (3 tools + YAML)
**Location:** `.ai/tools/rye/mcp/`

Model Context Protocol support.

**Key:** Mixed executors

**Tools:**
- mcp_call - Execute MCP calls
- mcp_server - Run MCP server
- mcp_client - Create MCP client

**Configs:**
- mcp_stdio.yaml
- mcp_http.yaml
- mcp_ws.yaml

**See:** [agent/mcp](agent/mcp.md)

### 12. **LLM** (5 configs)
**Location:** `.ai/tools/rye/llm/`

LLM provider configurations.

**Key:** YAML configs (not executable)

**Configs:**
- openai_chat.yaml - OpenAI Chat API
- openai_completion.yaml - OpenAI Completion API
- anthropic_messages.yaml - Anthropic Messages API
- anthropic_completion.yaml - Anthropic Completion API
- pricing.yaml - Token pricing

**See:** [agent/llm](agent/llm.md)

### 13. **Registry** (1 tool)
**Location:** `.ai/tools/rye/registry/`

Tool distribution and package management.

**Key:** Uses `http_client`

- registry.py - Publish, pull, search tools

**Operations:** publish, pull, search, auth, key

**See:** [registry/registry](registry/registry.md)

### 14. **Examples** (2 tools)
**Location:** `.ai/tools/rye/examples/`

Reference implementations.

**Key:** Use `python_runtime`

- git_status - Git operations example
- health_check - System diagnostics example

**See:** [examples/examples](examples/examples.md)

### 15. **Python Library** (Shared modules)
**Location:** `.ai/tools/rye/python/lib/`

Shared Python libraries for tools.

**Key:** `__tool_type__ = "python_lib"` (not executable)

- proxy_pool.py - Shared proxy pool
- (other shared modules)

**See:** [examples/python](examples/python.md)

### Category Summary Table

#### Core Infrastructure (IMMUTABLE - System Space)

| Category | Location | Count | Executor | Purpose |
|----------|----------|-------|----------|---------|
| Primitives | `core/primitives/` | 2 | None | Execution engines |
| Runtimes | `core/runtimes/` | 3 | subprocess, http | Language runtimes |
| Parsers | `core/parsers/` | 4 | python_runtime | Format parsing |
| Extractors | `core/extractors/` | 3 dirs | python_runtime | Data extraction |
| Protocol | `core/protocol/` | 1 | python_runtime | Communication protocols |
| Sinks | `core/sinks/` | 3 | python_runtime | Event output |
| System | `core/system/` | 1 | python_runtime | System info |

#### Feature Groups (MUTABLE - User/Project Spaces)

| Category | Location | Count | Executor | Purpose |
|----------|----------|-------|----------|---------|
| Agent | `agent/` | - | - | LLM agent execution |
| ├ Threads | `agent/threads/` | 12 + YAML | python_runtime | Thread management |
| ├ Capabilities | `agent/threads/capabilities/` | 5 | python_runtime | Sandboxing |
| ├ Cap Tokens | `agent/threads/capability_tokens/` | 1 | python_runtime | Token management |
| ├ Telemetry | `agent/telemetry/` | 6 | python_runtime | Monitoring agent runs |
| ├ LLM | `agent/llm/` | 5 | N/A | LLM provider configs |
| └ MCP | `agent/mcp/` | 3 + YAML | Mixed | MCP protocol |
| Registry | `registry/` | 1 | http_client | Tool distribution |
| RAG | `rag/` | 4 | http_client | Semantic search (optional) |
| Examples | `examples/` | 2 | python_runtime | Reference implementations |

**Total:** ~85+ tools + configs in bundled RYE

## Category Dependencies

```
CORE (Immutable System Space - Essential for RYE MCP)
├── core/primitives/ (Layer 1)
│   ├─ subprocess, http_client
│   └─ __executor_id__ = None (terminal nodes)
│
├── core/runtimes/ (Layer 2)
│   ├─ python_runtime, node_runtime, mcp_http_runtime
│   ├─ Declare ENV_CONFIG
│   └─ Delegate to Primitives
│
├── core/parsers/
│   └─ Content preprocessing (used by handlers)
│
├── core/extractors/
│   └─ Metadata extraction (used by discovery)
│
├── core/protocol/
│   └─ Communication protocols
│
├── core/sinks/
│   └─ Event output (file, null, websocket)
│
└── core/system/
    └─ System info (paths, runtime)

FEATURE GROUPS (Mutable - Features delivered via RYE)
├── agent/ (LLM agent execution)
│   ├─ threads/ (Thread management + capabilities)
│   ├─ telemetry/ (Monitoring agent runs)
│   ├─ llm/ (LLM provider configs)
│   └─ mcp/ (MCP protocol for agent comms)
├── registry/ (Tool distribution)
└── rag/ (Semantic search - optional)
```

## On-Demand Loading

All categories are accessible via the 5 MCP tools (search, load, execute, sign, help):

```
LLM calls search/load/execute
     │
     ├─→ Read from .ai/tools/rye/core/**/*.py    # Core tools (immutable system space)
     ├─→ Read from .ai/tools/rye/**/*.py         # App tools (mutable)
     ├─→ Read from .ai/tools/rye/**/*.yaml
     ├─→ Read from .ai/tools/{other}/**/*.py     # User tools (mutable)
     │
     └─→ Return tool definition or execution result
         └─ Only 5 MCP tools exposed to LLM
 ```

## Tool Spaces and Mutability

- **Core Infrastructure** (`.ai/tools/rye/core/`): Immutable system space, essential for RYE MCP
- **Feature Groups** (`.ai/tools/rye/{other}/`): Mutable, optional features delivered via RYE
- **User** (`.ai/tools/{user}/`): Mutable, user-created custom tools

**See Also:** [[../../executor/tool-resolution-and-validation.md]] for complete details on tool spaces, precedence, and validation.

## Best Practices

1. **System tools are immutable** - Core tools cannot be modified in-place (site-packages)
2. **Extend via app categories** - Create custom categories for organization
3. **Use appropriate executor** - Match tool to executor
4. **Follow metadata pattern** - Consistent `__version__`, `__tool_type__`, etc.
5. **Document CONFIG_SCHEMA** - Clear parameter documentation
6. **Handle errors gracefully** - Return error status in response

## Related Documentation

- [../bundle/structure](../bundle/structure.md) - Bundle directory organization with spaces model
- [../tool-resolution-and-validation](../executor/tool-resolution-and-validation.md) - Tool resolution, chain validation, and shadowing behavior
- [../executor/routing](../executor/routing.md) - How tools are routed
- [../executor/overview](../executor/overview.md) - Executor architecture
- [../package/structure](../package/structure.md) - Package organization
