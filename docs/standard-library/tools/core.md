```yaml
id: tools-core
title: "Core Tools"
description: "Internal tools that power Rye OS — parsers, runtimes, extractors, sinks, bundler, registry, system, and telemetry"
category: standard-library/tools
tags:
  [
    tools,
    core,
    internal,
    parsers,
    runtimes,
    extractors,
    sinks,
    bundler,
    registry,
  ]
version: "1.0.0"
```

# Core Tools

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

| Action       | Description                 |
| ------------ | --------------------------- |
| `login`      | Start device auth flow      |
| `login_poll` | Poll for auth completion    |
| `logout`     | Clear local auth session    |
| `signup`     | Create account              |
| `whoami`     | Show authenticated user     |
| `search`     | Search registry items       |
| `pull`       | Download item from registry |
| `push`       | Upload item to registry     |
| `delete`     | Remove item from registry   |
| `publish`    | Make item public            |
| `unpublish`  | Make item private           |

---

## Bundler — `rye/core/bundler/bundler`

Create and verify `.ai/` bundles — packaged collections of directives, tools, and knowledge.

| Action    | Description                                  |
| --------- | -------------------------------------------- |
| `create`  | Create bundle manifest from items            |
| `verify`  | Verify manifest signature and content hashes |
| `inspect` | Parse manifest without verification          |
| `list`    | List all bundles under `.ai/bundles/`        |

`collect.yaml` defines which items to include in a bundle.

---

## Parsers — `rye/core/parsers/`

Parse different file formats into structured metadata. Used by the search and execution engines.

| Parser                 | File Types            | Extracts                                                                                   |
| ---------------------- | --------------------- | ------------------------------------------------------------------------------------------ |
| `markdown_xml`         | Directive `.md` files | XML metadata blocks (model, limits, permissions, hooks, inputs, outputs)                   |
| `markdown_frontmatter` | Knowledge `.md` files | YAML frontmatter (id, title, category, tags, version)                                      |
| `python_ast`           | Tool `.py` files      | Dunder metadata (`__version__`, `__tool_type__`, `__category__`, etc.) and `CONFIG_SCHEMA` |
| `yaml`                 | Tool `.yaml` files    | Top-level keys (tool_id, tool_type, executor_id, parameters)                               |

---

## Extractors — `rye/core/extractors/`

YAML configs defining how metadata is extracted and indexed per item type:

| Config                               | Item Type  | Defines                                               |
| ------------------------------------ | ---------- | ----------------------------------------------------- |
| `directive/directive_extractor.yaml` | Directives | Search fields, validation rules, required metadata    |
| `tool/tool_extractor.yaml`           | Tools      | Search fields, parameter extraction, executor mapping |
| `knowledge/knowledge_extractor.yaml` | Knowledge  | Search fields, frontmatter validation, tag indexing   |

---

## Runtimes — `rye/core/runtimes/`

YAML configs defining how each tool type is executed. Runtimes configure interpreter resolution, argument templates, and environment setup.

| Runtime | Language | Execution | Args Template | Resolver | Use When |
|---------|----------|-----------|---|---|---|
| **python_function_runtime** | Python | In-process (fast) | `python -c "import,load,execute(params)"` | `venv_python` (find Python in .venv, fallback to system) | Pure Python logic, compute-heavy, no subprocess needed — fastest option |
| **python_script_runtime** | Python | Subprocess with isolation | `python tool.py --params {...} --project-path /path` | `venv_python` (find Python in .venv, fallback to system) | Long-running, heavy I/O, subprocess isolation needed, can use shell commands |
| **node_runtime** | JavaScript/TypeScript | Subprocess with Node resolution | `node tool.js --params {...} --project-path /path` | `node_modules` (find node in node_modules/.bin, fallback to system) | JavaScript tools, TypeScript via tsx, Node.js ecosystem dependencies |
| **bash_runtime** | Bash/Shell | Direct `/bin/bash` execution | `/bin/bash -c "{command}"` | `env` (PATH passthrough, no resolution) | Shell scripts, system administration, jq pipes, CLI composition |
| **mcp_stdio_runtime** | MCP (stdin/stdout) | Subprocess: spawn MCP, call via stdio | `python connect.py --server-config ... --tool ... --params {...}` | `venv_python` (Python for connect.py) | Local MCP servers, stdio transport, lightweight message passing |
| **mcp_http_runtime** | MCP (HTTP/SSE) | HTTP request to MCP server | `python connect.py --server-config ... --tool ... --params {...}` | `venv_python` (Python for connect.py) | External HTTP MCP servers, long-lived connections, streaming tools |
| **state_graph_runtime** | YAML Graph | Subprocess: load graph, dispatch rye_execute per node | `python -c "load_graph,walk_nodes,rye_execute(...)"` | `venv_python` with `mode: always` anchoring | Declarative workflows, condition branches, multi-step node execution |

### Interpreter Resolution Strategies

**`python_function_runtime` & `python_script_runtime` & `state_graph_runtime`** use `venv_python` resolver:
- Searches for Python in `{project}/.venv/bin/python3`
- Falls back to system `python3` if not found
- Enables virtual environment isolation per project
- Sets `RYE_PYTHON` environment variable for template expansion

**`node_runtime`** uses `node_modules` resolver:
- Searches `node_modules/.bin` for Node/tsx executables
- Falls back to system `node` command
- Enables per-project Node version pinning via `package.json`
- Sets `RYE_NODE` environment variable

**`bash_runtime`** uses `env` resolver:
- No interpreter resolution, just passes `${PATH}` through
- Bash is found via absolute path `/bin/bash`
- Minimal setup, maximum shell power

**MCP runtimes** use `venv_python` for the connect.py script:
- Same venv resolution as Python runtimes
- Server config is read from filesystem
- Tool parameters passed as JSON to MCP call

### Anchoring & Module Resolution

Runtimes with anchoring enabled (`python_function_runtime`, `python_script_runtime`, `node_runtime`, `state_graph_runtime`) establish a project root for dependency loading:
- Anchors automatically locate `pyproject.toml` or `package.json`
- Set `PYTHONPATH` or `NODE_PATH` to include anchor root and runtime lib directories
- Enables tools to load sibling modules without package-level imports

The `lib/python/module_loader.py` handles dynamic Python module loading for thread tools — it imports modules relative to an anchor path so thread components can load each other without package-level imports.

---

## Primitives — `rye/core/primitives/`

Low-level YAML configs for system operations:

| Config             | Purpose                                  |
| ------------------ | ---------------------------------------- |
| `subprocess.yaml`  | Shell subprocess execution configuration |
| `http_client.yaml` | HTTP client configuration                |

---

## Sinks — `rye/core/sinks/`

Output sinks for streaming thread events:

| Sink                | Format | Description                                      |
| ------------------- | ------ | ------------------------------------------------ |
| `file_sink.py`      | JSONL  | Write events to a file, one JSON object per line |
| `null_sink.py`      | —      | Discard all events (no-op, for testing)          |
| `websocket_sink.py` | JSON   | Stream events to a WebSocket client in real-time |

Sinks receive events from the `EventEmitter` during thread execution. Configure which sink to use via the thread's event configuration.
