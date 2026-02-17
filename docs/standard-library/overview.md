---
id: standard-library-overview
title: "Standard Library Overview"
description: Everything that ships with Rye OS out of the box
category: standard-library
tags: [standard-library, bundled, system-space, catalog]
version: "1.0.0"
---

# Standard Library Overview

Rye OS ships a **standard library** of directives, tools, and knowledge entries inside the Python package at `rye/rye/.ai/`. These items live in the **system space** — the lowest-priority tier — and are available to every project automatically, without any setup or installation.

When you install Rye OS, every project immediately has access to file-system tools, shell execution, item creation directives, thread orchestration, and more. You never need to copy these files into your project.

## Override Mechanism

System space items can be overridden by placing a file with the same `item_id` in a higher-priority space:

| Space       | Location                        | Priority |
| ----------- | ------------------------------- | -------- |
| **Project** | `.ai/` (project root)           | Highest  |
| **User**    | `~/.ai/` (home directory)       | Middle   |
| **System**  | `rye/rye/.ai/` (Python package) | Lowest   |

Resolution order: **project → user → system**. The first match wins.

For example, to customize the `rye/file-system/read` tool for your project, create `.ai/tools/rye/file-system/read.py` in your project root. Your version will be used instead of the built-in one. The system version remains untouched and continues to serve other projects.

---

## Catalog

### Directives

Five directives ship in `.ai/directives/rye/`:

| Item ID                              | Version | Description                                                                        |
| ------------------------------------ | ------- | ---------------------------------------------------------------------------------- |
| `rye/core/create_directive`          | 3.0.0   | Create a new directive with metadata, validate, and sign                           |
| `rye/core/create_tool`               | 3.0.0   | Create a new tool file with metadata headers and parameter schema, then sign       |
| `rye/core/create_knowledge`          | 3.0.0   | Create a new knowledge entry with YAML frontmatter and sign                        |
| `rye/core/create_threaded_directive` | 2.0.0   | Create a directive with full thread execution support (model, limits, permissions) |
| `rye/agent/threads/thread_summary`   | 1.0.0   | Summarize a thread conversation for context carryover during resumption            |

The first four are **user-facing** creation directives — you invoke them to scaffold new items. `thread_summary` is **infrastructure** — called internally by the thread system during handoff.

See [Bundled Directives](bundled-directives.md) for detailed documentation of each.

### Tools

Tools are organized by namespace under `.ai/tools/rye/`:

#### File System — `rye/file-system/`

| Tool         | Description                     |
| ------------ | ------------------------------- |
| `read`       | Read file contents              |
| `write`      | Write file contents             |
| `edit_lines` | Edit specific lines in a file   |
| `glob`       | Find files by glob pattern      |
| `grep`       | Search file contents with regex |
| `ls`         | List directory contents         |

#### Shell — `rye/bash/`

| Tool   | Description            |
| ------ | ---------------------- |
| `bash` | Execute shell commands |

#### Web — `rye/web/`

| Tool        | Description                      |
| ----------- | -------------------------------- |
| `webfetch`  | Fetch and parse web page content |
| `websearch` | Search the web                   |

#### MCP Client — `rye/mcp/`

| Tool       | Description                               |
| ---------- | ----------------------------------------- |
| `connect`  | Connect to an external MCP server         |
| `discover` | Discover available tools on an MCP server |
| `manager`  | Manage MCP server connections             |

#### Registry — `rye/registry/`

| Tool       | Description                              |
| ---------- | ---------------------------------------- |
| `registry` | Push, pull, and search the item registry |

#### LSP — `rye/lsp/`

| Tool  | Description                          |
| ----- | ------------------------------------ |
| `lsp` | Language Server Protocol integration |

#### System — `rye/core/system/`

| Tool     | Description                                              |
| -------- | -------------------------------------------------------- |
| `system` | System info (environment variables, paths, runtime info) |

#### Telemetry — `rye/core/telemetry/`

| Tool       | Description                  |
| ---------- | ---------------------------- |
| `mcp_logs` | MCP request/response logging |

#### Primary Tool Wrappers — `rye/primary/`

| Tool          | Description                         |
| ------------- | ----------------------------------- |
| `rye_execute` | Execute items (used inside threads) |
| `rye_load`    | Load item content                   |
| `rye_search`  | Search for items                    |
| `rye_sign`    | Validate and sign items             |

These are the tools that threads use to interact with Rye OS itself.

#### Thread Orchestration Engine — `rye/agent/threads/`

The thread system is the largest tool namespace. It provides autonomous, budget-controlled directive execution.

| Component         | Tools                                                                                                                                                                                  | Description                                                           |
| ----------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------------------------------------------- |
| **Core**          | `thread_directive`, `orchestrator`, `runner`, `safety_harness`                                                                                                                         | Thread lifecycle — start, orchestrate, run, and guard execution       |
| **Adapters**      | `http_provider`, `provider_adapter`, `provider_resolver`, `tool_dispatcher`                                                                                                            | Connect threads to LLM providers and dispatch tool calls              |
| **Events**        | `event_emitter`, `streaming_tool_parser`                                                                                                                                               | Emit structured events and parse streaming tool-use responses         |
| **Internal**      | `budget_ops`, `cancel_checker`, `classifier`, `control`, `cost_tracker`, `emitter`, `limit_checker`, `state_persister`, `text_tool_parser`, `thread_chain_search`, `tool_result_guard` | Budget tracking, cancellation, error classification, state management |
| **Loaders**       | `config_loader`, `coordination_loader`, `error_loader`, `events_loader`, `hooks_loader`, `resilience_loader`, `condition_evaluator`, `interpolation`                                   | Load YAML configs for coordination, resilience, hooks, and events     |
| **Persistence**   | `thread_registry`, `transcript`, `state_store`, `artifact_store`, `budgets`                                                                                                            | Store thread state, transcripts, artifacts, and budget ledgers        |
| **Security**      | `security`                                                                                                                                                                             | Capability token enforcement within threads                           |
| **Config (YAML)** | `events`, `error_classification`, `hook_conditions`, `coordination`, `resilience`, `budget_ledger_schema`                                                                              | Declarative configuration for thread behavior                         |

#### LLM Providers — `rye/agent/providers/`

| Config           | Description                               |
| ---------------- | ----------------------------------------- |
| `anthropic.yaml` | Anthropic (Claude) provider configuration |
| `openai.yaml`    | OpenAI provider configuration             |

#### Permissions — `rye/agent/permissions/`

| Component                                                                                               | Description                              |
| ------------------------------------------------------------------------------------------------------- | ---------------------------------------- |
| `capability_tokens.py`                                                                                  | Capability token creation and validation |
| `primary.yaml`                                                                                          | Primary capability definitions           |
| `agent.yaml`, `db.yaml`, `fs.yaml`, `git.yaml`, `mcp.yaml`, `net.yaml`, `process.yaml`, `registry.yaml` | Per-domain capability definitions        |

#### Bundler — `rye/core/bundler/`

| Tool           | Description                     |
| -------------- | ------------------------------- |
| `bundler`      | Create and verify item bundles  |
| `collect.yaml` | Bundle collection configuration |

#### Parsers — `rye/core/parsers/`

| Tool                   | Description                                |
| ---------------------- | ------------------------------------------ |
| `markdown_frontmatter` | Parse YAML frontmatter from Markdown files |
| `markdown_xml`         | Parse XML blocks from Markdown files       |
| `python_ast`           | Extract metadata from Python tool files    |
| `yaml`                 | Parse YAML files                           |

#### Extractors — `rye/core/extractors/`

| Config                     | Description                                     |
| -------------------------- | ----------------------------------------------- |
| `directive_extractor.yaml` | Metadata extraction rules for directives        |
| `tool_extractor.yaml`      | Metadata extraction rules for tools             |
| `knowledge_extractor.yaml` | Metadata extraction rules for knowledge entries |

#### Runtimes — `rye/core/runtimes/`

| Config                    | Description                             |
| ------------------------- | --------------------------------------- |
| `python_script_runtime`   | Execute Python tool scripts             |
| `python_function_runtime` | Execute Python functions directly       |
| `node_runtime`            | Execute JavaScript/Node.js tools        |
| `bash_runtime`            | Execute Bash scripts                    |
| `mcp_stdio_runtime`       | Execute MCP servers via stdio transport |
| `mcp_http_runtime`        | Execute MCP servers via HTTP transport  |

#### Primitives — `rye/core/primitives/`

| Config             | Description                        |
| ------------------ | ---------------------------------- |
| `subprocess.yaml`  | Subprocess execution configuration |
| `http_client.yaml` | HTTP client configuration          |

#### Sinks — `rye/core/sinks/`

| Tool             | Description                      |
| ---------------- | -------------------------------- |
| `file_sink`      | Write streaming events to files  |
| `null_sink`      | Discard streaming events (no-op) |
| `websocket_sink` | Stream events over WebSocket     |

### Knowledge

Three reference entries ship in `.ai/knowledge/rye/`:

| Item ID                                 | Description                                         |
| --------------------------------------- | --------------------------------------------------- |
| `rye/core/directive-metadata-reference` | Complete specification of directive metadata fields |
| `rye/core/tool-metadata-reference`      | Complete specification of tool metadata fields      |
| `rye/core/knowledge-metadata-reference` | Complete specification of knowledge metadata fields |

These are the authoritative references for the metadata schema of each item type. The creation directives consult them when generating new items.

### Other Bundled Files

| Path                             | Description                                          |
| -------------------------------- | ---------------------------------------------------- |
| `bundles/rye-core/manifest.yaml` | Bundle manifest for the core standard library bundle |
| `lockfiles/`                     | Integrity pinning files for signed items             |

---

## What's NOT in the Standard Library

The standard library provides the **infrastructure** — the tools and directives that make Rye OS work. It does not include:

- **Project-specific items** — directives, tools, and knowledge for your particular application (these go in `.ai/`)
- **User customizations** — personal overrides or additions (these go in `~/.ai/`)
- **Registry-downloaded items** — community or team items pulled from the registry via `rye_execute(item_type="tool", item_id="rye/registry/registry", ...)`
- **Demo or example content** — the standard library is production infrastructure, not a tutorial

To add items for your project, create files under `.ai/directives/`, `.ai/tools/`, or `.ai/knowledge/` in your project root — or use the bundled creation directives to scaffold them.
