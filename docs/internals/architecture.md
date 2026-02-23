```yaml
id: architecture
title: "Architecture"
description: Rye OS system architecture — layers, components, and data flow
category: internals
tags: [architecture, layers, components, overview]
version: "1.0.0"
```

# Architecture

Rye OS is a four-layer system that turns AI agent tool calls into OS-level operations. Every layer has a single responsibility and communicates with adjacent layers through well-defined interfaces.

## Layer 1: Lilux Microkernel

The bottom layer. Lilux provides stateless, async-first primitives for interacting with the operating system. It has **no knowledge** of Rye, `.ai/` directories, or tool metadata.

| Primitive             | Location                            | Purpose                                                                          |
| --------------------- | ----------------------------------- | -------------------------------------------------------------------------------- |
| `SubprocessPrimitive` | `lilux/primitives/subprocess.py`    | Run shell commands with two-stage templating, timeout handling, and stdin piping |
| `HttpClientPrimitive` | `lilux/primitives/http_client.py`   | Make HTTP requests with retry logic, auth headers, and SSE streaming             |
| `signing`             | `lilux/primitives/signing.py`       | Ed25519 key generation, sign, verify — pure crypto, no policy                    |
| `integrity`           | `lilux/primitives/integrity.py`     | Generic deterministic SHA256 hashing via `compute_integrity(data)`               |
| `lockfile`            | `lilux/primitives/lockfile.py`      | Lockfile I/O — load/save JSON lockfiles with explicit paths                      |
| `EnvResolver`         | `lilux/runtime/env_resolver.py`     | Resolve environment variables from `.env` files, venvs, version managers         |
| `SchemaValidator`     | `lilux/schemas/schema_validator.py` | JSON Schema validation                                                           |

Lilux primitives are pure I/O. They receive fully-resolved configuration and execute it. No path discovery, no precedence logic, no policy decisions.

## Layer 2: Rye MCP Server

The orchestration layer. Rye implements the MCP (Model Context Protocol) server that AI agents interact with. It provides four MCP tools:

- **execute** — Run a tool by item ID (resolves chain, verifies integrity, delegates to Lilux)
- **load** — Read a directive, tool, or knowledge entry (with metadata parsing)
- **search** — Find items across all spaces by keyword
- **sign** — Sign items with Ed25519 (batch signing via glob patterns)

Key components in this layer:

| Component           | Location                             | Responsibility                                                                        |
| ------------------- | ------------------------------------ | ------------------------------------------------------------------------------------- |
| `PrimitiveExecutor` | `rye/executor/primitive_executor.py` | Chain resolution, validation, caching, and execution routing                          |
| `ChainValidator`    | `rye/executor/chain_validator.py`    | Space compatibility, I/O matching, version constraint checks                          |
| `LockfileResolver`  | `rye/executor/lockfile_resolver.py`  | Three-tier lockfile resolution and management                                         |
| `MetadataManager`   | `rye/utils/metadata_manager.py`      | Signature format handling, content hashing, signing                                   |
| Resolvers           | `rye/utils/resolvers.py`             | `DirectiveResolver`, `ToolResolver`, `KnowledgeResolver` — three-tier path resolution |
| `path_utils`        | `rye/utils/path_utils.py`            | Space paths, bundle discovery, category extraction                                    |

This layer implements all policy: which spaces to search, how to validate chains, when to reject unsigned items. Lilux never makes these decisions.

## Layer 3: `.ai/` Data Bundle

The "standard library" that ships inside the `rye` package at `rye/rye/.ai/`. This is what agents actually interact with — it contains the tools, runtimes, directives, and knowledge entries that define system capabilities.

### Runtimes (YAML configs)

Runtimes are YAML files in `.ai/tools/rye/core/runtimes/` that configure how to invoke a primitive:

| Runtime                   | `executor_id`                     | Purpose                                                          |
| ------------------------- | --------------------------------- | ---------------------------------------------------------------- |
| `python/script`           | `rye/core/primitives/subprocess`  | Run Python scripts with venv resolution and PYTHONPATH anchoring |
| `python/function`         | `rye/core/primitives/subprocess`  | Run Python functions via module loader                           |
| `bash/bash`               | `rye/core/primitives/subprocess`  | Execute shell commands via `/bin/bash -c`                        |
| `node/node`               | `rye/core/primitives/subprocess`  | Run Node.js scripts with `node_modules` resolution               |
| `mcp/stdio`               | `rye/core/primitives/subprocess`  | Spawn MCP servers over stdio                                     |
| `mcp/http`                | `rye/core/primitives/http_client` | Connect to MCP servers over HTTP/SSE                             |
| `state-graph/runtime`     | `rye/core/primitives/subprocess`  | Walk declarative graph YAML tools, dispatching `rye_execute` for each node  |

### Tools

Python scripts and other executables in `.ai/tools/` that point to a runtime via `__executor_id__`. Examples: bash tool, file-system operations, web search, registry client, parsers, thread system.

### Directives

Markdown files in `.ai/directives/` containing XML workflow instructions. Examples: `create_directive`, `create_tool`, `bootstrap`.

### Knowledge

Reference entries in `.ai/knowledge/` — metadata patterns, best practices, domain information.

## Layer 4: Registry API

A separate FastAPI service (`services/registry-api/`) for sharing items across projects and users. It provides:

- **POST /v1/push** — Validate, sign with registry provenance, and store items in Supabase
- **GET /v1/pull/{item_type}/{item_id}** — Download items with integrity verification
- **GET /v1/search** — Full-text search across published items
- **POST /v1/bundle/push** and **GET /v1/bundle/pull** — Bundle-level push/pull
- **GET /v1/public-key** — Expose the registry's Ed25519 public key for TOFU pinning

The registry API runs independently (deployed on Railway) and uses Supabase as its database. The bundled registry tool (`.ai/tools/rye/core/registry/registry.py`) is the client-side interface.

## Data Flow

A complete execution path from agent request to OS operation:

```
MCP Client (AI Agent)
  │
  ▼  JSON-RPC "execute" call
Rye MCP Server
  │
  ▼  PrimitiveExecutor.execute(item_id, params)
Chain Resolution
  │  _build_chain() follows __executor_id__ recursively
  │  e.g., "rye/bash/bash" → python/script → subprocess primitive
  │
  ▼  verify_item() on every chain element
Integrity Verification
  │  Check signature, content hash, Ed25519 sig, trust store
  │
  ▼  ChainValidator.validate_chain()
Chain Validation
  │  Space compatibility, I/O matching, version constraints
  │
  ▼  LockfileResolver.get_lockfile() / create_lockfile()
Lockfile Check
  │  Verify pinned hashes match current files
  │
  ▼  EnvResolver.resolve() through chain
Environment Resolution
  │  .env files, venv detection, interpreter paths, static vars
  │
  ▼  _execute_chain() → SubprocessPrimitive.execute() or HttpClientPrimitive.execute()
Lilux Primitive
  │  Two-stage templating: ${ENV_VAR} then {param}
  │
  ▼  asyncio.create_subprocess_exec() or httpx request
Operating System
```

## Design Philosophy

**Everything is data.** Tools are files with metadata headers (`__executor_id__`, `__version__`, `ENV_CONFIG`). Runtimes are YAML configs that describe how to invoke a primitive. Extractors are YAML files that define how to parse different file formats. The system reads data to decide how to execute, rather than hardcoding behavior.

This means:

- Adding a new runtime = creating a YAML file (no code changes to Rye)
- Adding a new tool = creating a Python/JS/shell script with metadata headers
- Overriding system behavior = placing a file in project space (shadows system space)
- No hardcoded executor IDs in `PrimitiveExecutor` — only the two Lilux primitive mappings (`subprocess` and `http_client`) are registered in `PRIMITIVE_MAP`

The only hardcoded knowledge in the system is the mapping from primitive IDs to Lilux classes. Everything above that is resolved from the filesystem at runtime.

## Package and Bundle Distribution

The system is distributed as pip packages (`lilux`, `ryeos-core`, `ryeos`, `ryeos-mcp`), each with a clear role. `ryeos` and `ryeos-core` share the same Python code but register different bundles — `ryeos` exposes all `rye/*` items, while `ryeos-core` exposes only `rye/core/*`. See [Packages and Bundles](packages-and-bundles.md) for the full breakdown.
