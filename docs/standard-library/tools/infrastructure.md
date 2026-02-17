```yaml
id: tools-infrastructure
title: "Infrastructure Tools"
description: "Internal tools that power Rye OS — parsers, runtimes, extractors, sinks, bundler, registry, system, and telemetry"
category: standard-library/tools
tags: [tools, infrastructure, internal, parsers, runtimes, extractors, sinks, bundler, registry]
version: "1.0.0"
```

# Infrastructure Tools

These tools power the Rye OS internals. You won't call most of them directly, but understanding them helps when debugging or extending the system.

---

## System & Telemetry

### `system` — `rye/core/system/system`

Query runtime information: environment variables, paths, and system details.

### `telemetry` — `rye/core/telemetry/telemetry`

MCP request/response logging. Read server logs, stats, and errors.

---

## Registry — `rye/core/registry/registry`

Push, pull, search, and manage items in the Rye OS registry. Supports these actions:

| Action | Description |
| --- | --- |
| `login` | Start device auth flow |
| `login_poll` | Poll for auth completion |
| `logout` | Clear local auth session |
| `signup` | Create account |
| `whoami` | Show authenticated user |
| `search` | Search registry items |
| `pull` | Download item from registry |
| `push` | Upload item to registry |
| `delete` | Remove item from registry |
| `publish` | Make item public |
| `unpublish` | Make item private |

---

## Bundler — `rye/core/bundler/bundler`

Create and verify `.ai/` bundles — packaged collections of directives, tools, and knowledge.

| Action | Description |
| --- | --- |
| `create` | Create bundle manifest from items |
| `verify` | Verify manifest signature and content hashes |
| `inspect` | Parse manifest without verification |
| `list` | List all bundles under `.ai/bundles/` |

`collect.yaml` defines which items to include in a bundle.

---

## Parsers — `rye/core/parsers/`

Parse different file formats into structured metadata. Used by the search and execution engines.

| Parser | File Types | Extracts |
| --- | --- | --- |
| `markdown_xml` | Directive `.md` files | XML metadata blocks (model, limits, permissions, hooks, inputs, outputs) |
| `markdown_frontmatter` | Knowledge `.md` files | YAML frontmatter (id, title, category, tags, version) |
| `python_ast` | Tool `.py` files | Dunder metadata (`__version__`, `__tool_type__`, `__category__`, etc.) and `CONFIG_SCHEMA` |
| `yaml` | Tool `.yaml` files | Top-level keys (tool_id, tool_type, executor_id, parameters) |

---

## Extractors — `rye/core/extractors/`

YAML configs defining how metadata is extracted and indexed per item type:

| Config | Item Type | Defines |
| --- | --- | --- |
| `directive/directive_extractor.yaml` | Directives | Search fields, validation rules, required metadata |
| `tool/tool_extractor.yaml` | Tools | Search fields, parameter extraction, executor mapping |
| `knowledge/knowledge_extractor.yaml` | Knowledge | Search fields, frontmatter validation, tag indexing |

---

## Runtimes — `rye/core/runtimes/`

YAML configs defining how each tool type is executed:

| Runtime | Language/Protocol | How It Runs |
| --- | --- | --- |
| `python_script_runtime` | Python | Subprocess: `python tool.py --params '{}' --project-path /path` |
| `python_function_runtime` | Python | In-process: import module, call `execute(params, project_path)` |
| `node_runtime` | JavaScript | Subprocess: `node tool.js` |
| `bash_runtime` | Bash | Subprocess: `bash tool.sh` |
| `mcp_stdio_runtime` | MCP (stdio) | Subprocess: launch MCP server, call tool via stdio |
| `mcp_http_runtime` | MCP (HTTP) | HTTP: connect to MCP server, call tool via Streamable HTTP |

The `lib/python/module_loader.py` handles dynamic Python module loading for thread tools — it imports modules relative to an anchor path so thread components can load each other without package-level imports.

---

## Primitives — `rye/core/primitives/`

Low-level YAML configs for system operations:

| Config | Purpose |
| --- | --- |
| `subprocess.yaml` | Shell subprocess execution configuration |
| `http_client.yaml` | HTTP client configuration |

---

## Sinks — `rye/core/sinks/`

Output sinks for streaming thread events:

| Sink | Format | Description |
| --- | --- | --- |
| `file_sink.py` | JSONL | Write events to a file, one JSON object per line |
| `null_sink.py` | — | Discard all events (no-op, for testing) |
| `websocket_sink.py` | JSON | Stream events to a WebSocket client in real-time |

Sinks receive events from the `EventEmitter` during thread execution. Configure which sink to use via the thread's event configuration.
