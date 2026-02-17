# RYE-Lilux Architecture Summary

## Quick Reference

This document provides a crystal-clear summary of the architecture for implementation.

---

## The Two Layers

| Layer     | Package     | Role                                 | Intelligence                            |
| --------- | ----------- | ------------------------------------ | --------------------------------------- |
| **Lilux** | `lilux`     | Microkernel - dumb execution         | None - just executes                    |
| **RYE**   | `rye-lilux` | OS Layer - intelligent orchestration | Content understanding, caching, routing |

---

## Lilux (Microkernel)

**Purpose:** Execute exactly what it's told. No intelligence.

### Directory Structure

```
lilux/
├── primitives/
│   ├── subprocess.py      # Execute shell commands
│   ├── http_client.py     # Make HTTP requests
│   ├── integrity.py       # Pure hash/crypto functions
│   ├── lockfile.py        # Lockfile I/O (LockfileManager)
│   └── errors.py          # Error types
├── runtime/
│   ├── auth_store.py      # Keychain integration
│   └── env_resolver.py    # Environment variable resolution
└── schemas/
    └── tool_schema.py     # JSON Schema validation utilities
```

### What Lilux Does

- `SubprocessPrimitive.execute(config)` → runs shell command
- `HttpClientPrimitive.execute(config)` → makes HTTP request
- `compute_hash(content)` → returns SHA256 hash
- `sign(content, key)` → returns signature
- `verify_signature(content, sig, key)` → returns bool
- `LockfileManager.load(path)` → loads lockfile from disk
- `LockfileManager.save(lockfile, path)` → saves lockfile to disk
- `AuthStore.get_token(service)` → retrieves from keychain
- `EnvResolver.resolve(env_config)` → resolves environment variables
- `SchemaValidator.validate(instance, schema)` → validates JSON

### What Lilux Does NOT Do

- ❌ Chain resolution
- ❌ Chain validation
- ❌ Lockfile validation (comparing chains)
- ❌ Lockfile creation from chains
- ❌ Metadata extraction
- ❌ Tool discovery
- ❌ Caching with invalidation
- ❌ Content parsing (XML, frontmatter, etc.)
- ❌ Orchestration

---

## RYE (OS Layer)

**Purpose:** Intelligent orchestration on top of Lilux.

### Directory Structure

```
rye/
├── server.py              # MCP server entry point
├── handlers/
│   ├── directive/         # DirectiveHandler
│   ├── tool/              # ToolHandler
│   └── knowledge/         # KnowledgeHandler
├── executor/
│   ├── primitive_executor.py   # Chain resolution + execution
│   ├── chain_validator.py      # Validate tool chains
│   └── integrity_verifier.py   # Cached integrity verification
├── loading/
│   └── tool_loader.py     # On-demand tool loading
└── extractors/
    └── schema_extractor.py # Metadata extraction
```

### The 4 MCP Tools

LLM sees exactly 4 tools:

| Tool      | Purpose             |
| --------- | ------------------- |
| `search`  | Find items by query |
| `load`    | Load item content   |
| `execute` | Execute an item     |
| `sign`    | Validate and sign   |

### The 3 Item Types

| Type        | Location          | Format                 |
| ----------- | ----------------- | ---------------------- |
| `directive` | `.ai/directives/` | XML in Markdown        |
| `tool`      | `.ai/tools/`      | Python, YAML, etc.     |
| `knowledge` | `.ai/knowledge/`  | Markdown + frontmatter |

### On-Demand Loading

**No startup scanning.** Items are loaded when requested:

```
LLM calls: execute(item_type="tool", item_id="git")
    │
    ├─→ RYE loads .ai/tools/{category}/git.py
    ├─→ RYE extracts metadata (__tool_type__, __executor_id__, etc.)
    ├─→ RYE resolves chain (git → python_runtime → subprocess)
    ├─→ RYE calls Lilux SubprocessPrimitive.execute()
    └─→ Returns result to LLM
```

---

## How They Work Together

```
LLM
  │
  └─→ RYE (5 MCP Tools)
        │
        ├─→ ToolHandler
        │     ├─→ Loads tool from .ai/tools/
        │     ├─→ SchemaExtractor extracts metadata
        │     ├─→ ChainValidator validates chain
        │     └─→ PrimitiveExecutor resolves chain
        │
        └─→ Lilux Primitives
              ├─→ SubprocessPrimitive.execute()
              └─→ HttpClientPrimitive.execute()
```

---

## Key Design Decisions

### 1. Lilux is Dumb

Lilux never:

- Parses tool IDs to find schemas
- Builds registries at startup
- Caches with complex invalidation
- Validates chains

Lilux always:

- Executes what the orchestrator tells it
- Returns result objects (success/failure)
- Provides pure functions

### 2. RYE is Smart

RYE handles:

- Chain resolution
- Schema extraction
- Caching with hash-based invalidation
- MCP protocol
- Content type understanding

### 3. On-Demand, Not Discovery

- No startup scanning of `.ai/tools/`
- Items loaded when LLM requests them
- Same model for directives, tools, and knowledge

### 4. Clean Separation

- Lilux `integrity.py` = pure `compute_hash()` function
- RYE `IntegrityVerifier` = stateful class with caching that uses Lilux's function

---

## Implementation Checklist

### Lilux Implementation

- [ ] `primitives/subprocess.py` - SubprocessPrimitive
- [ ] `primitives/http_client.py` - HttpClientPrimitive
- [ ] `primitives/integrity.py` - compute_hash, sign, verify_signature
- [ ] `primitives/lockfile.py` - LockfileManager
- [ ] `primitives/errors.py` - Error types
- [ ] `runtime/auth_store.py` - AuthStore
- [ ] `runtime/env_resolver.py` - EnvResolver
- [ ] `schemas/tool_schema.py` - SchemaValidator

### RYE Implementation

- [ ] `server.py` - MCP server with 5 tools
- [ ] `handlers/directive/handler.py` - DirectiveHandler
- [ ] `handlers/tool/handler.py` - ToolHandler
- [ ] `handlers/knowledge/handler.py` - KnowledgeHandler
- [ ] `executor/primitive_executor.py` - PrimitiveExecutor
- [ ] `executor/chain_validator.py` - ChainValidator
- [ ] `executor/integrity_verifier.py` - IntegrityVerifier
- [ ] `loading/tool_loader.py` - On-demand tool loading
- [ ] `extractors/schema_extractor.py` - SchemaExtractor

---

## Documentation Files

### Lilux Docs (`docs/lilux/`)

| File                           | Purpose                 |
| ------------------------------ | ----------------------- |
| `principles.md`                | What Lilux is and isn't |
| `package/structure.md`         | Directory organization  |
| `primitives/overview.md`       | All primitives          |
| `primitives/subprocess.md`     | SubprocessPrimitive     |
| `primitives/http-client.md`    | HttpClientPrimitive     |
| `primitives/integrity.md`      | Pure hash functions     |
| `primitives/lockfile.md`       | LockfileManager         |
| `runtime-services/overview.md` | AuthStore, EnvResolver  |
| `schemas/overview.md`          | JSON Schema validation  |

### RYE Docs (`docs/rye/`)

| File                          | Purpose                                              |
| ----------------------------- | ---------------------------------------------------- |
| `principles.md`               | What RYE is                                          |
| `mcp-server.md`               | MCP server configuration                             |
| `mcp-tools/overview.md`       | The 5 MCP tools                                      |
| `loading/overview.md`         | On-demand loading                                    |
| `executor/overview.md`        | Chain resolution                                     |
| `executor/components.md`      | PrimitiveExecutor, ChainValidator, IntegrityVerifier |
| `executor/chain-validator.md` | Chain validation                                     |
| `cache/overview.md`           | Caching system                                       |
| `package/structure.md`        | Directory organization                               |
| `bundle/structure.md`         | .ai/ content organization                            |
