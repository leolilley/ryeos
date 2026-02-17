# Content Bundle Structure

## Overview

The content bundle (`.ai/` directory) contains all tool definitions, directives, and knowledge entries. It's bundled with RYE and deployed as data, not code.

**Two-Part RYE Architecture:**

1. **Code Layer:** `rye/` package - Contains server, handlers, runtime wrappers
2. **Data Layer:** `.ai/tools/rye/` - Contains all bundled tool definitions (data files)

**Key Distinction:** Tools are split into **Core** (essential) and **App** (optional, replaceable). User pip installs system to an immutable site-packages. These ensure core rye tools are always available and are hash validated with lockfiles built into the package. Users can load tools out of the system space, modify and override with their modified version if they wish. However, we can always guarantee system level packages will not change.

**See Also:**

- **RYE Package Structure:** `[[rye/package/structure]]`
- **Registry (Optional):** `[[rye/categories/registry]]`

---

## Bundle Organization

```
.ai/
├── tools/
│    └─ rye/                        # RYE bundled tools, the system space - Immutable, pip-installed to site-packages, hash-validated with lockfiles
│       ├── core/                   # The core tooling which the primary tools use or base tool foundations
│       │   ├── primitives/         # Execution primitives
│       │   ├── runtimes/           # Language runtimes
│       │   ├── parsers/            # Content parsing
│       │   ├── extractors/         # Metadata extraction
│       │   ├── protocol/           # Communication protocols
│       │   ├── sinks/              # Event output
│       │   └── system/             # System information (paths, runtime info)
│       │
│       ├── agent/                  # LLM agent execution within RYE
│       │   ├── capabilities/       # Sandboxing (enforced in llm threads through the safety harness)
│       │   ├── telemetry/          # Agent monitoring and diagnostics through the safety harness
│       │   ├── llm/                # Base LLM provider configs. User extends to provide their own API keys, model selection, etc.
│       │   ├── mcp/                # MCP protocol for agent. (still a little unsure if we need an agent specific mcp tool here).
│       │   ├── harness/            # Agent harness that processes llm loop and tool calls
│       │   └── threads/            # Thread management.
│       │
│       ├── registry/               # Primary items distribution.
│       ├── rag/                    # Personal Semantic search which user extends with their perefences.
│       ├── telemetry/              # System monitoring and diagnostics, giving tool to access mcp logging.
│       └── mcp/                    # MCP protocol tools for rye os.
│
├── directives/                     # Workflow definitions for core rye functionality.
├── knowledge/                      # Knowledge entries for rye and lilux documentation.
└── ...
```

## Core vs App Tools

| Category | Location        | Purpose                           |
| -------- | --------------- | --------------------------------- |
| **Core** | `core/`         | Essential for RYE MCP to function |
| **App**  | everything else | Features delivered via RYE        |

### Tool Spaces Model

RYE uses a three-space model for tool resolution:

| Space       | Location                 | Precedence  |
| ----------- | ------------------------ | ----------- |
| **Project** | `{project}/.ai/`         | 1 (highest) |
| **User**    | `~/.ai/`                 | 2 (medium)  |
| **System**  | `site-packages/rye/.ai/` | 3 (lowest)  |

**Key Principles:**

- Project tools have highest precedence (shadow user/system)
- User tools can shadow system tools
- System tools are immutable (read-only, installed via pip to site-packages)
- System tools are hash-validated with built-in lockfiles - guaranteed not to change
- Users can load tools from system space and override with modified versions if desired
- Tools can depend on tools from equal or higher precedence spaces

**See Also:** [[../executor/tool-resolution-and-validation.md]] for complete details on tool resolution, chain validation across spaces, and shadowing behavior

### Core Tools (`core/`) - System Space (Immutable)

The executor chain relies on these tools:

| Directory          | Purpose                                      | Why Critical                       |
| ------------------ | -------------------------------------------- | ---------------------------------- |
| `core/primitives/` | subprocess, http_client                      | Terminal nodes of ALL execution    |
| `core/runtimes/`   | python_runtime, node_runtime                 | Language environment configuration |
| `core/parsers/`    | markdown_xml, frontmatter, yaml              | Content preprocessing              |
| `core/extractors/` | tool/, directive/, knowledge/                | Metadata extraction for discovery  |
| `core/system/`     | paths, runtime info (pre-packaged RYE tools) | System information                 |
| `core/sinks/`      | file, null, websocket                        | Event output infrastructure        |

**Access:** Available in `source="system"` for search, load, execute.

**Mutability:** These tools are immutable because they're in the read-only system space (site-packages), installed via pip. They are hash-validated with built-in lockfiles to guarantee they never change. Users CAN shadow them by creating tools with the same name in project or user space, allowing customization while preserving the original immutable system tools.

### App Tools - Mutable

Optional features users can customize. Unlike Core tools, these are replaceable and can be shadowed by user-defined tools:

| Directory          | Purpose                          | Shadowable?             |
| ------------------ | -------------------------------- | ----------------------- |
| `agent/`           | LLM agent execution within RYE   | ✅ Yes                  |
| `agent/threads/`   | Thread management + capabilities | ✅ Yes                  |
| `agent/telemetry/` | Monitoring agent runs            | ✅ Yes                  |
| `agent/llm/`       | LLM provider configs             | ✅ Yes                  |
| `agent/mcp/`       | MCP protocol for agent comms     | ✅ Yes                  |
| `mcp/`             | MCP protocol rye mcp             | ✅ Yes                  |
| `registry/`        | Tool distribution                | ✅ Yes                  |
| `rag/`             | Semantic search                  | ✅ Yes (fully optional) |

**Shadowing Behavior:**

- Users can create in project or user space
- Those higher-precedence tools will be used instead of system tools
- This is INTENTIONAL - allows customization and experimentation

**Note on Capabilities:** Capabilities (`git`, `fs`, `db`, `net`, `process`) live under `agent/threads/capabilities/` because they're ONLY enforced when running LLM agent threads in the agent harness. Direct MCP calls are NOT restricted by capabilities. Capabilities are unrelated to tool spaces and shadowing.

---

**See Also:** [[../executor/tool-resolution-and-validation.md]] for complete tool resolution and validation details

## Category Definitions

### Core Tool Categories (Under `.ai/tools/rye/core/`) - System Space (Immutable, Hash-Validated)

**System space tools are pip-installed to site-packages and are hash-validated with built-in lockfiles to ensure they never change. Users can shadow these tools with their own versions, but the original system tools remain immutable and always available.**

#### 1. Primitives (`core/primitives/`)

**2 execution primitive schemas** - hardcoded execution with `__executor_id__ = None`

| File             | Purpose                  |
| ---------------- | ------------------------ |
| `subprocess.py`  | Shell command execution  |
| `http_client.py` | HTTP requests with retry |

**Key:** All have `__executor_id__ = None` (no delegation)

#### 2. Runtimes (`core/runtimes/`)

**3 language-specific executors** - add environment configuration on top of primitives

| File                  | Delegates To | Purpose                  |
| --------------------- | ------------ | ------------------------ |
| `python_runtime.py`   | subprocess   | Python script execution  |
| `node_runtime.py`     | subprocess   | Node.js script execution |
| `mcp_http_runtime.py` | http_client  | HTTP-based MCP           |

**Key:** All have `__executor_id__` pointing to primitives, declare `ENV_CONFIG`

#### 3. Parsers (`core/parsers/`)

**4 content parsers** - preprocess content for extraction

| File              | Purpose                             |
| ----------------- | ----------------------------------- |
| `markdown_xml.py` | Parse Markdown with XML             |
| `frontmatter.py`  | Parse frontmatter (YAML + Markdown) |
| `python_ast.py`   | Parse Python AST                    |
| `yaml.py`         | Parse YAML                          |

#### 4. Extractors (`core/extractors/`)

**3 subdirectories** - metadata extraction from content

| Directory    | Purpose                                                                                |
| ------------ | -------------------------------------------------------------------------------------- |
| `tool/`      | Extract metadata from tools in any format (Python, YAML, JavaScript, Bash, TOML, etc.) |
| `directive/` | Extract from XML directives                                                            |
| `knowledge/` | Extract from Markdown                                                                  |

---

### App Tool Categories (Under `.ai/tools/rye/`) - Mutable Space (Shadowable)

**App tools are optional, replaceable features. Unlike Core tools (which are immutable system tools), App tools can be shadowed by user-defined tools in project or user space.**

### App Tool Categories (Under `.ai/tools/rye/`) - NOT PROTECTED

#### 6. Threads (`threads/`)

**12+ tools** - async execution and thread management

Includes `threads/capabilities/` subdirectory for sandboxing (git, fs, db, net, process).

**Note:** Capabilities are ONLY enforced when running LLM threads.

#### 7. Telemetry (`telemetry/`)

**7 telemetry tools** - system monitoring and diagnostics

| File                     | Purpose              |
| ------------------------ | -------------------- |
| `telemetry_configure.py` | Configure telemetry  |
| `telemetry_status.py`    | Get telemetry status |
| `telemetry_clear.py`     | Clear telemetry data |
| `telemetry_export.py`    | Export telemetry     |
| `rag_configure.py`       | Configure RAG        |
| `lib_configure.py`       | Configure libraries  |
| `health_check.py`        | System health check  |

#### 8. Protocol (`protocol/`)

**Protocol implementations** - communication protocols

| File                 | Purpose                   |
| -------------------- | ------------------------- |
| `jsonrpc_handler.py` | JSON-RPC protocol handler |

#### 9. Sinks (`sinks/`)

**3 event sinks** - where events flow to

| File                | Purpose           |
| ------------------- | ----------------- |
| `file_sink.py`      | Write to files    |
| `null_sink.py`      | Discard events    |
| `websocket_sink.py` | Send to WebSocket |

|

#### 6. System (`core/system/`)

**System information** - paths, runtime info, pre-packaged RYE tools

| File              | Purpose                         |
| ----------------- | ------------------------------- |
| `bootstrap.py`    | Get started with example tools  |
| `examples/`       | Example tools                   |
| `templates/`      | Tool templates for new projects |
| `runtime_info.py` | Runtime configuration           |
| `paths.py`        | System path information         |

**Protection:** System tools are **read-only** (like core tools) - can't be overwritten in place, but can be shadowed.

**Access:** Available in `source="system"` for search, load, execute.

#### 10. MCP (`mcp/`)

**MCP tools and configurations**

**MCP tools and configurations**

| File            | Purpose           |
| --------------- | ----------------- |
| `mcp_call.py`   | Execute MCP calls |
| `mcp_server.py` | Run MCP server    |
| `mcp_client.py` | MCP client        |

#### 11. LLM (`llm/`)

**LLM provider configurations** - YAML config files only

| File                        | Purpose                         |
| --------------------------- | ------------------------------- |
| `openai_chat.yaml`          | OpenAI Chat API config          |
| `openai_completion.yaml`    | OpenAI Completion API config    |
| `anthropic_messages.yaml`   | Anthropic Messages API config   |
| `anthropic_completion.yaml` | Anthropic Completion API config |
| `pricing.yaml`              | Token pricing config            |

#### 12. Registry (`registry/`)

**Registry operations** - publish/pull from registry

| File          | Executor    | Purpose                      |
| ------------- | ----------- | ---------------------------- |
| `registry.py` | http_client | Registry publish/pull/search |

#### 13. RAG (`rag/`) - OPTIONAL

**Semantic search** - completely optional user feature, distinct from RYE core.

**System Tools** - Pre-packaged RYE tools in `core/system/` (not RAG).

**RAG Tools** - Located in `rag/` for semantic search (user feature).

| File            | Purpose                  |
| --------------- | ------------------------ |
| `rag_index.py`  | Index documents          |
| `rag_search.py` | Semantic search          |
| `rag_embed.py`  | Embed single document    |
| `rag_delete.py` | Remove from vector store |

**Note:** RYE works without RAG using keyword search.

#### 14. Utility (`utility/`)

**General utilities**

| File                 | Executor       | Purpose              |
| -------------------- | -------------- | -------------------- |
| `http_test.py`       | http_client    | HTTP request testing |
| `hello_world.py`     | python_runtime | Hello world example  |
| `test_proxy_pool.py` | python_runtime | Proxy pool testing   |

#### 15. Examples (`examples/`)

**Example tools** - reference implementations

| File              | Purpose              |
| ----------------- | -------------------- |
| `git_status.py`   | Git status example   |
| `health_check.py` | Health check example |

#### 16. Python Libraries (`python/lib/`)

**Shared Python libraries** - imported by tools

| Module              | Purpose                          |
| ------------------- | -------------------------------- |
| `proxy_pool.py`     | Shared proxy pool implementation |
| (other shared libs) | Reusable Python modules          |

**Key:** Libraries have `__tool_type__ = "python_lib"` (not executable)

## Tool Category Summary

### Core Tools (PROTECTED - Immutable System Space)

**Core tools are pip-installed to immutable site-packages, hash-validated with built-in lockfiles to guarantee they never change. Users can shadow these tools with their own versions in project or user space.**

| Category   | Location           | Count  | Executor                |
| ---------- | ------------------ | ------ | ----------------------- |
| Primitives | `core/primitives/` | 2      | None                    |
| Runtimes   | `core/runtimes/`   | 3      | subprocess, http_client |
| Parsers    | `core/parsers/`    | 4      | python_runtime          |
| Extractors | `core/extractors/` | 3 dirs | python_runtime          |
| Protocol   | `core/protocol/`   | 1      | python_runtime          |
| Sinks      | `core/sinks/`      | 3      | python_runtime          |
| System     | `core/system/`     | 1      | python_runtime          |

### App Tools (NOT PROTECTED - Mutable and Shadowable)

**App tools are optional, replaceable features that can be shadowed by user-defined tools. Unlike Core tools (which are immutable system tools), App tools are fully customizable.**

| Category       | Location                           | Count     | Executor       |
| -------------- | ---------------------------------- | --------- | -------------- |
| Agent          | `agent/`                           | -         | -              |
| ├ Threads      | `agent/threads/`                   | 12 + YAML | python_runtime |
| ├ Capabilities | `agent/threads/capabilities/`      | 5         | python_runtime |
| ├ Cap Tokens   | `agent/threads/capability_tokens/` | 1         | python_runtime |
| ├ Telemetry    | `agent/telemetry/`                 | 7         | python_runtime |
| ├ LLM          | `agent/llm/`                       | 5         | N/A            |
| └ MCP          | `agent/mcp/`                       | 3 + YAML  | python_runtime |
| Registry       | `registry/`                        | 1         | http_client    |
| RAG            | `rag/`                             | 4         | http_client    |

**Total:** ~85+ tools/configs in bundled RYE package

## On-Demand Loading Process

### How Tools Are Accessed

Tools are loaded on demand when the LLM calls `search()`, `load()`, or `execute()`. Only 5 MCP tools are exposed to the LLM - not all tools in `.ai/tools/`.

```
LLM calls execute("git", {"command": "status"})
│
└─→ RYE reads .ai/tools/rye/agent/threads/capabilities/git.py
│
├─ Parse metadata
├─ Extract __tool_type__, __executor_id__, __category__
├─ Extract CONFIG_SCHEMA
├─ Extract ENV_CONFIG (if runtime)
│
└─→ Execute tool and return result
```

### Tool Lookup Example

```
LLM: execute("git", {"command": "status"})
│
└─→ Load from: .ai/tools/rye/agent/threads/capabilities/git.py
│
├─ __tool_type__ = "python"
├─ __executor_id__ = "python_runtime"
├─ __category__ = "capabilities"
│
└─→ Execute and return result to LLM
```

## Category Organization

### Core (`.ai/tools/rye/core/`)

**Immutable System Space - Essential for RYE MCP to function**

- primitives/, runtimes/, parsers/, extractors/, sinks/, protocol/, system/
- Installed via pip to immutable site-packages
- Hash-validated with built-in lockfiles - guaranteed never to change
- Users CAN shadow these tools in project or user space (intentional - allows customization while preserving immutable originals)

### App (`.ai/tools/rye/{other}/`)

**Mutable - Features delivered via RYE**

- threads/, mcp/, registry/, rag/, telemetry/, sinks/, etc.
- Can be replaced by user tools (shadowable)

### User (`.ai/tools/{user}/`)

**User-defined tools** - Can create any category

```
.ai/tools/
├── rye/          # Bundled with RYE
│   ├── core/     # Immutable system space
│   └── ...       # App tools
├── python/       # Example: Python tools
├── user/         # Example: User tools
└── myproject/    # Example: Project-specific tools
```

## Directory Metadata

### File-Level Metadata

```python
# .ai/tools/rye/agent/threads/capabilities/git.py

__version__ = "1.0.0"          # Tool version
__tool_type__ = "python"       # Type: primitive, runtime, python, python_lib
__executor_id__ = "python_runtime"  # Execution delegate
__category__ = "capabilities"  # Category
```

### Directory-Level Organization

- **Core**: Immutable tools (primitives, runtimes, parsers, extractors, protocol, sinks, **system**) - pip-installed to site-packages, hash-validated with built-in lockfiles
- **App**: Mutable features (threads, mcp, registry, etc.) - replaceable and shadowable
- **User**: Custom tools (any category outside rye/) - always takes precedence over system tools

**System Tools**

The `core/system/` directory contains **pre-packaged RYE tools** that expose system information:

| Tool        | Purpose                        | Data-Driven? |
| ----------- | ------------------------------ | ------------ |
| Bootstrap   | Get started with example tools | ✅ Yes       |
| System Info | Paths, runtime configuration   | ✅ Yes       |

**Protection:** System tools are **read-only** (like all core tools) - pip-installed to immutable site-packages, hash-validated with built-in lockfiles to guarantee they never change. They can't be overwritten in place, but can be shadowed by user tools in project or user space.

**Access:** Available in `source="system"` for search, load, execute.

## Tool Resolution and Shadowing

### Tool Resolution Logic

RYE resolves tools by **priority** across three locations. System space tools (Core) are immutable and hash-validated with built-in lockfiles to guarantee they never change:

```
Priority: 1 (highest) → Priority: 2 → Priority: 3 (lowest)
  └─────────────┐       └─────────────┐       └─────────────┐
  Project          │              User           │              System
  (project_path/.ai/)│              (~/.ai/)        │              ({install_location}/.ai/)
```

**Resolution Steps:**

1. **Project space** - Search `{project_path}/.ai/` first
2. **User space** - If not in project, search `~/.ai/` (or `USER_SPACE`)
3. **System space** - If not in project or user, search `{install_location}/.ai/`

**Result:** Tool from highest-priority location is used.

### Shadowing Rules

**Shadowing** = Creating a tool with same `item_id` in a higher-priority space.

| Scenario                             | Behavior                                   | Example                                                            |
| ------------------------------------ | ------------------------------------------ | ------------------------------------------------------------------ |
| Project exists, User creates same ID | Project version is used (highest priority) | User creates `git.py`, project has `git.py` → Project wins         |
| User exists, System has same ID      | User version is used (higher priority)     | System has `bootstrap.py`, user creates `bootstrap.py` → User wins |
| All three have same ID               | Project version is used                    | `git.py` exists in all three → Project wins                        |

**Note:** System and user tools can be shadowed by project tools. Project tools **cannot** be shadowed (only user can shadow system).

---

## Key Distinction Summary

**Tools are split into Core (essential) and App (optional, replaceable).**

- **Core tools** are pip-installed to immutable site-packages
- Core tools are always available and hash-validated with lockfiles built into the package
- Users can load tools from system space and override with their modified version if they wish
- System level packages are guaranteed not to change
- **App tools** are mutable, optional features that can be shadowed by user-defined tools

This architecture ensures RYE's core functionality remains stable and reliable while allowing maximum flexibility for customization.

## Related Documentation

- [[../executor/overview]] - Tool discovery and routing
- [[overview]] - All categories detailed
- [[../executor/tool-resolution-and-validation]] - Tool spaces, precedence, chain validation, and shadowing behavior
- [[../categories/extractors]] - Schema-driven extraction
- [[../categories/parsers]] - Content preprocessors
