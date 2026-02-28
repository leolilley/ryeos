```yaml
id: bundled-tools
title: "Bundled Tools"
description: All tools that ship with Rye OS — from file operations to the orchestration engine
category: standard-library
tags: [tools, bundled, standard-library, catalog]
version: "1.0.0"
```

# Bundled Tools

Rye OS ships a standard library of tools inside the `ryeos` package at `ryeos/rye/.ai/tools/rye/`. These live in the **system space** and are always available — no installation required. Tools are organized into two tiers: **agent-facing tools** that users interact with directly, and **infrastructure tools** that power the system internally.

Tools are distributed across multiple bundles:
- **Standard bundle** (`ryeos`) — file-system, bash, MCP, primary tools, agent system, infrastructure
- **Web bundle** (`ryeos-web`, or `pip install ryeos[web]`) — browser automation, fetch, search
- **Code bundle** (`ryeos-code`, or `pip install ryeos[code]`) — npm, diagnostics, typescript, LSP

All tools are invoked via `rye_execute(item_type="tool", item_id="<item_id>", parameters={...})`.

---

## Agent-Facing Tools

These are the tools you'll use most often — file I/O, shell access, web, MCP, and the orchestration engine.

### File System (`rye/file-system/`)

Python scripts for file operations, all executed via `python/script`.

| Tool       | Item ID                      | Description                                      |
| ---------- | ---------------------------- | ------------------------------------------------ |
| read       | `rye/file-system/read`       | Read file contents                               |
| write      | `rye/file-system/write`      | Write content to a file (supports `create_dirs`) |
| edit_lines | `rye/file-system/edit_lines` | Edit specific lines in a file                    |
| glob       | `rye/file-system/glob`       | Find files matching glob patterns                |
| grep       | `rye/file-system/grep`       | Search file contents with regex                  |
| ls         | `rye/file-system/ls`         | List directory contents                          |

**Examples:**

```python
# Write a file, creating parent directories
rye_execute(item_type="tool", item_id="rye/file-system/write",
    parameters={"path": "config/settings.json", "content": '{"debug": true}', "create_dirs": True})

# Search for a pattern across files
rye_execute(item_type="tool", item_id="rye/file-system/grep",
    parameters={"pattern": "TODO:", "path": "src/", "glob": "**/*.py"})

# Find all markdown files
rye_execute(item_type="tool", item_id="rye/file-system/glob",
    parameters={"pattern": "**/*.md", "path": "docs/"})
```

### Bash (`rye/bash/`)

| Tool | Item ID         | Description            |
| ---- | --------------- | ---------------------- |
| bash | `rye/bash/bash` | Execute shell commands |

```python
rye_execute(item_type="tool", item_id="rye/bash/bash",
    parameters={"command": "git status --short"})
```

### Web (`rye/web/`) — requires `ryeos-web` or `ryeos[web]`

| Tool    | Item ID                      | Description                              |
| ------- | ---------------------------- | ---------------------------------------- |
| fetch   | `rye/web/fetch/fetch`        | Fetch and extract content from web pages |
| search  | `rye/web/search/search`      | Search the web                           |
| browser | `rye/web/browser/browser`    | Browser automation via playwright-cli    |

**Browser commands:** `open`, `goto`, `screenshot`, `snapshot`, `click`, `fill`, `type`, `select`, `hover`, `resize`, `console`, `network`, `eval`, `press`, `tab-list`, `tab-new`, `tab-select`, `tab-close`, `close`, `close-all`

```python
# Open a page
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "open", "args": ["http://localhost:3000"]})

# Take a screenshot
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "screenshot"})

# Click an element by ref
rye_execute(item_type="tool", item_id="rye/web/browser/browser",
    parameters={"command": "click", "args": ["e15"]})

# Fetch a web page
rye_execute(item_type="tool", item_id="rye/web/fetch/fetch",
    parameters={"url": "https://docs.example.com/api"})
```

### MCP Client (`rye/mcp/`)

Tools for connecting to external MCP servers.

| Tool     | Item ID            | Description                               |
| -------- | ------------------ | ----------------------------------------- |
| connect  | `rye/mcp/connect`  | Connect to an external MCP server         |
| discover | `rye/mcp/discover` | Discover available tools on an MCP server |
| manager  | `rye/mcp/manager`  | Manage MCP server connections             |

### Registry (`rye/core/registry/`)

| Tool     | Item ID                      | Description                                  |
| -------- | ---------------------------- | -------------------------------------------- |
| registry | `rye/core/registry/registry` | Push, pull, and search items in the registry |

```python
rye_execute(item_type="tool", item_id="rye/core/registry/registry",
    parameters={"action": "search", "query": "deployment"})
```

### Code Tools (`rye/code/`) — requires `ryeos-code` or `ryeos[code]`

Development tools for package management, type checking, diagnostics, and LSP code intelligence. Node.js dependencies (`node_modules`) are NOT shipped — they are installed on first use via the node runtime's anchor system.

| Tool        | Item ID                              | Description                                         |
| ----------- | ------------------------------------ | --------------------------------------------------- |
| npm         | `rye/code/npm/npm`                   | NPM/NPX operations — install, run, build, exec      |
| diagnostics | `rye/code/diagnostics/diagnostics`   | Run linters and type checkers (ruff, mypy, eslint…)  |
| typescript  | `rye/code/typescript/typescript`     | TypeScript type checker — tsc --noEmit               |
| lsp         | `rye/code/lsp/lsp`                   | LSP client — go to definition, references, hover…    |

**Actions:** `install`, `run`, `build`, `test`, `init`, `exec`

```python
# Install packages
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "install", "args": ["react", "react-dom"], "working_dir": "frontend"})

# Run a script
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "run", "args": ["build"], "working_dir": "frontend"})

# Execute via npx
rye_execute(item_type="tool", item_id="rye/code/npm/npm",
    parameters={"action": "exec", "args": ["vite", "build"], "working_dir": "frontend"})
```

### System (`rye/core/system/`)

| Tool   | Item ID                  | Description                                   |
| ------ | ------------------------ | --------------------------------------------- |
| system | `rye/core/system/system` | System info: env vars, paths, runtime details |

### Dev Tools (`rye/dev/`)

Development and testing utilities.

| Tool        | Item ID                | Description                          |
| ----------- | ---------------------- | ------------------------------------ |
| test_runner | `rye/dev/test-runner`  | Run `.test.yaml` specs against tools |

The test runner discovers test specs from `.ai/tests/**/*.test.yaml`, executes tools via `ExecuteTool`, and evaluates assertions. Supports tag-based filtering and validate-only mode.

**Examples:**

```python
# Run all tests for a specific tool
rye_execute(item_type="tool", item_id="rye/dev/test-runner",
    parameters={"tool": "my-project/scrapers/chart-discovery"})

# Skip integration tests
rye_execute(item_type="tool", item_id="rye/dev/test-runner",
    parameters={"tool": "my-project/scrapers/chart-discovery", "exclude_tags": "integration"})
```

Or from the CLI:

```bash
rye test my-project/scrapers/chart-discovery
rye test my-project/scrapers/chart-discovery --exclude-tags integration
```

See the [test runner knowledge](../orchestration/state-graphs.md) for test spec format and assertion DSL.

---

## Orchestration Engine (`rye/agent/threads/`)

The thread system is the largest subsystem. It enables AI agents to run directives in managed threads with full LLM loops, budgets, safety controls, and event streaming.

### Entry Points

| Tool             | Item ID                              | Description                                                 |
| ---------------- | ------------------------------------ | ----------------------------------------------------------- |
| thread_directive | `rye/agent/threads/thread_directive` | **Internal** — used by `execute directive` to spawn managed threads. Not called directly by agents. |
| orchestrator     | `rye/agent/threads/orchestrator`     | Thread coordination: wait, cancel, status, chain resolution |

Agents spawn threads via `execute directive`, which internally delegates to `thread_directive`:

```python
# Spawn a directive in a managed thread
rye_execute(item_type="directive", item_id="my-workflow",
    parameters={"target": "staging"})

# Check thread status
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
    parameters={"action": "status", "thread_id": "t-abc123"})
```

### Core Components (not called directly)

| Tool           | Item ID                            | Description                                             |
| -------------- | ---------------------------------- | ------------------------------------------------------- |
| runner         | `rye/agent/threads/runner`         | Core LLM loop for thread execution                      |
| safety_harness | `rye/agent/threads/safety_harness` | Thread safety: limits, hooks, cancellation, permissions |

### Adapters (`rye/agent/threads/adapters/`)

Bridge between the thread engine and LLM providers:

- **provider_adapter** — Base provider adapter interface
- **http_provider** — HTTP-based LLM provider (Anthropic, OpenAI)
- **provider_resolver** — Resolves model name/tier to provider config
- **tool_dispatcher** — Dispatches tool calls from LLM responses to `rye_execute`

### Events (`rye/agent/threads/events/`)

- **event_emitter** — Emit thread lifecycle events (`cognition_in`, `cognition_out`, `tool_call_result`, `thread_completed`, etc.)
- **streaming_tool_parser** — Parse streaming tool call responses from LLM providers

### Internal (`rye/agent/threads/internal/`)

Low-level components that power the LLM loop:

- **budget_ops** — Budget arithmetic operations
- **cancel_checker** — Check cancellation flag
- **classifier** — Classify thread output
- **control** — Control flow actions from hooks
- **cost_tracker** — Track token/spend costs
- **emitter** — Internal event emission
- **limit_checker** — Check resource limits
- **state_persister** — Persist thread state
- **text_tool_parser** — Parse tool calls from plain text (for models without native `tool_use`)
- **thread_chain_search** — Search across continuation chain transcripts
- **tool_result_guard** — Bound large tool results, dedupe, store artifacts

### Loaders (`rye/agent/threads/loaders/`)

Data-driven config loaders — read YAML configs and return structured data:

- **config_loader**, **coordination_loader**, **error_loader**, **events_loader**, **hooks_loader**, **resilience_loader**
- **condition_evaluator** — Evaluate hook conditions
- **interpolation** — Interpolate variables in hook actions

### Persistence (`rye/agent/threads/persistence/`)

- **thread_registry** — Register, track, and query threads
- **transcript** — Record and reconstruct thread conversations
- **state_store** — Persist thread state between turns
- **artifact_store** — Store large artifacts outside conversation context
- **budgets** — Hierarchical budget ledger

### Security (`rye/agent/threads/security/`)

- **security** — Thread-level security enforcement

### Config (`rye/agent/threads/config/`)

YAML configuration files that control thread behavior:

| Config File                 | Purpose                                  |
| --------------------------- | ---------------------------------------- |
| `events.yaml`               | Event definitions and criticality levels |
| `error_classification.yaml` | Error types and retry policies           |
| `hook_conditions.yaml`      | Built-in hook condition definitions      |
| `coordination.yaml`         | Wait timeouts, continuation config       |
| `resilience.yaml`           | Default limits, retry policies           |
| `budget_ledger_schema.yaml` | Budget ledger JSON schema                |

---

## LLM Providers (`rye/agent/providers/`)

YAML configs for LLM provider integration:

- **anthropic.yaml** — Anthropic Claude API config (model tiers, endpoints, `tool_use` format)
- **openai.yaml** — OpenAI API config

---

## Capability System (`rye/agent/permissions/`)

Controls what directives and threads are allowed to do:

- **capability_tokens.py** — Capability token creation and validation
- **Capability YAML files** in `capabilities/`:
  - `primary.yaml` — Primary capability definitions
  - `tools/rye/agent.yaml` — Agent tool capabilities
  - `tools/rye/fs.yaml` — File system capabilities
  - `tools/rye/db.yaml` — Database capabilities
  - `tools/rye/git.yaml` — Git capabilities
  - `tools/rye/mcp.yaml` — MCP capabilities
  - `tools/rye/net.yaml` — Network capabilities
  - `tools/rye/process.yaml` — Process capabilities
  - `tools/rye/registry.yaml` — Registry capabilities

---

## Infrastructure Tools

These tools power the system internally. You won't call them directly, but they're good to know about.

### Parsers (`rye/core/parsers/`)

Parse different file formats into structured metadata:

- **markdown/xml** — Parse directive files (markdown + XML metadata)
- **markdown/frontmatter** — Parse knowledge files (markdown + YAML frontmatter)
- **python/ast** — Parse Python tool metadata via AST introspection
- **yaml/yaml** — Parse YAML tool configs
- **javascript/javascript** — Parse JS/TS tool metadata via regex extraction

### Extractors (`rye/core/extractors/`)

YAML configs defining search fields, extraction rules, and validation schemas per item type:

- `directive/directive_extractor.yaml`
- `tool/tool_extractor.yaml`
- `knowledge/knowledge_extractor.yaml`

### Runtimes (`rye/core/runtimes/`)

YAML configs defining how each language/protocol is executed:

| Runtime                        | Description                       |
| ------------------------------ | --------------------------------- |
| `python/script.yaml`           | Run Python scripts via subprocess |
| `python/function.yaml`         | Run Python functions in-process   |
| `node/node.yaml`               | Run Node.js scripts               |
| `bash/bash.yaml`               | Run bash scripts                  |
| `mcp/stdio.yaml`               | Connect to MCP servers via stdio  |
| `mcp/http.yaml`                | Connect to MCP servers via HTTP   |

The `lib/python/module_loader.py` handles dynamic Python module loading for thread tools.

### Primitives (`rye/core/primitives/`)

YAML configs for low-level operations:

- **subprocess.yaml** — Shell subprocess config
- **http_client.yaml** — HTTP client config

### Sinks (`rye/core/sinks/`)

Output sinks for streaming events:

- **file_sink.py** — Write events to file (JSONL format)
- **null_sink.py** — Discard events
- **websocket_sink.py** — Stream events via WebSocket

### Telemetry (`rye/core/telemetry/`)

- **mcp_logs.py** — MCP request/response logging

### Bundler (`rye/core/bundler/`)

- **bundler.py** — Create and verify `.ai/` bundles
- **collect.yaml** — Collection config

### Primary Tools (`rye/primary/`)

Wrappers around the 4 MCP tools, used inside threads:

| Tool        | Item ID                   | Description                             |
| ----------- | ------------------------- | --------------------------------------- |
| rye_execute | `rye/primary/rye_execute` | Execute tools, directives, or knowledge |
| rye_load    | `rye/primary/rye_load`    | Load item content for inspection        |
| rye_search  | `rye/primary/rye_search`  | Search for items by scope and query     |
| rye_sign    | `rye/primary/rye_sign`    | Validate and sign item files            |

These are the tools the LLM sees when running inside a thread. They call the same underlying MCP tool implementations but are exposed as individual tool definitions for the LLM's function-calling interface.
