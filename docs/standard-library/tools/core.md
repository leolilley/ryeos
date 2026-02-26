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

## Keys — `rye/core/keys/keys`

Manage Ed25519 signing keys — generate, inspect, trust, list, and remove.

| Action     | Description                                                             |
| ---------- | ----------------------------------------------------------------------- |
| `generate` | Generate a new Ed25519 keypair in `~/.ai/config/keys/signing/`          |
| `info`     | Show fingerprint and details for the current keypair                    |
| `trust`    | Add the public key to the trust store (default: user space)             |
| `list`     | List all trusted keys across all spaces                                 |
| `remove`   | Remove a trusted key by fingerprint                                     |
| `import`   | Import a keypair from environment variables (for serverless environments)|

Use `space: project` with `trust` to provision a signing key into a bundle's `.ai/config/keys/trusted/` for distribution.

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
| `publish`        | Make item public                |
| `unpublish`      | Make item private               |
| `create_api_key` | Create an API key (`rye_sk_` prefix) |
| `list_api_keys`  | List all API keys for the user  |
| `revoke_api_key` | Revoke an API key               |

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

| Parser                   | File Types                       | Extracts                                                                                   |
| ------------------------ | -------------------------------- | ------------------------------------------------------------------------------------------ |
| `markdown/xml`           | Directive `.md` files            | XML metadata blocks (model, limits, permissions, hooks, inputs, outputs)                   |
| `markdown/frontmatter`   | Knowledge `.md` files            | YAML frontmatter (name, title, category, tags, version)                                    |
| `python/ast`             | Tool `.py` files                 | Dunder metadata (`__version__`, `__tool_type__`, `__category__`, etc.) and `CONFIG_SCHEMA` |
| `yaml/yaml`              | Tool `.yaml` files               | Top-level keys (tool_id, tool_type, executor_id, parameters)                               |
| `javascript/javascript`  | Tool `.js`/`.ts`/`.mjs`/`.cjs`  | `export const` metadata (`__version__`, `__tool_type__`, etc.) and `CONFIG_SCHEMA`         |

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
| **python/function** | Python | In-process (fast) | `python -c "import,load,execute(params)"` | `local_binary` (binary: python3, candidates/search_paths in .venv, fallback to system) | Pure Python logic, compute-heavy, no subprocess needed — fastest option |
| **python/script** | Python | Subprocess with isolation | `python tool.py --params {...} --project-path /path` | `local_binary` (binary: python3, candidates/search_paths in .venv, fallback to system) | Long-running, heavy I/O, subprocess isolation needed, can use shell commands |
| **node/node** | JavaScript/TypeScript | Subprocess with Node resolution | `node tool.js --params {...} --project-path /path` | `local_binary` (binary: node, search_paths/search_roots in node_modules/.bin, fallback to system) | JavaScript tools, TypeScript via tsx, Node.js ecosystem dependencies |
| **bash/bash** | Bash/Shell | Direct `/bin/bash` execution | `/bin/bash -c "{command}"` | `env` (PATH passthrough, no resolution) | Shell scripts, system administration, jq pipes, CLI composition |
| **mcp/stdio** | MCP (stdin/stdout) | Subprocess: spawn MCP, call via stdio | `python connect.py --server-config ... --tool ... --params {...}` | `local_binary` (binary: python3, for connect.py) | Local MCP servers, stdio transport, lightweight message passing |
| **mcp/http** | MCP (HTTP/SSE) | HTTP request to MCP server | `python connect.py --server-config ... --tool ... --params {...}` | `local_binary` (binary: python3, for connect.py) | External HTTP MCP servers, long-lived connections, streaming tools |
| **state-graph/runtime** | YAML Graph | Subprocess: load graph, dispatch rye_execute per node | `python -c "load_graph,walk_nodes,rye_execute(...)"` | `local_binary` (binary: python3) with `mode: always` anchoring | Declarative workflows, condition branches, multi-step node execution |

### Interpreter Resolution Strategies

**`python/function`**, **`python/script`**, & **`state-graph/runtime`** use `local_binary` resolver (binary: `python3`):
- Searches candidates/search_paths for Python in `{project}/.venv/bin/python3`
- Falls back to system `python3` if not found
- Enables virtual environment isolation per project
- Sets `RYE_PYTHON` environment variable for template expansion

**`node/node`** uses `local_binary` resolver (binary: `node`):
- Searches search_paths/search_roots including `node_modules/.bin` for Node/tsx executables
- Falls back to system `node` command
- Enables per-project Node version pinning via `package.json`
- Sets `RYE_NODE` environment variable

**`bash/bash`** uses `env` resolver:
- No interpreter resolution, just passes `${PATH}` through
- Bash is found via absolute path `/bin/bash`
- Minimal setup, maximum shell power

**MCP runtimes** (`mcp/stdio`, `mcp/http`) use `local_binary` (binary: `python3`) for the connect.py script:
- Same `local_binary` resolution as Python runtimes
- Server config is read from filesystem
- Tool parameters passed as JSON to MCP call

### Anchoring & Module Resolution

Runtimes with anchoring enabled (`python/function`, `python/script`, `node/node`, `state-graph/runtime`) establish a project root for dependency loading:
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
