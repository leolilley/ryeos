```yaml
id: packages-and-bundles
title: "Packages and Bundles"
description: How Rye OS is distributed — pip packages, bundle entry points, and dependency layering
category: internals
tags: [packages, bundles, dependencies, distribution, pyproject]
version: "1.0.0"
```

# Packages and Bundles

Rye OS is distributed as pip packages. Each package has a clear role, minimal dependencies, and registers a **bundle** of `.ai/` items into the system space. This page explains what ships where and why.

## Packages

```
┌─────────────────────────────────────────────────────┐
│  ryeos-mcp                                          │
│  MCP transport (stdio/SSE)                          │
│  deps: ryeos, mcp                                   │
│  bundle: none (inherits ryeos bundle from ryeos)    │
├─────────────────────────────────────────────────────┤
│  ryeos                                              │
│  Resolver, executor, signing, metadata              │
│  deps: lilux, pyyaml, cryptography, packaging       │
│  bundle: ryeos (all rye/* items)                    │
├─────────────────────────────────────────────────────┤
│  ryeos-bare                                         │
│  Same code as ryeos, no bundle                      │
│  deps: lilux, pyyaml, cryptography, packaging       │
│  bundle: none (no .ai/ items registered)            │
├─────────────────────────────────────────────────────┤
│  ryeos-core                                         │
│  Same code as ryeos, minimal bundle                 │
│  deps: lilux, pyyaml, cryptography, packaging       │
│  bundle: ryeos-core (only rye/core/* items)         │
├─────────────────────────────────────────────────────┤
│  lilux                                              │
│  Stateless microkernel primitives                   │
│  deps: cryptography, httpx                          │
│  bundle: none (no .ai/ items)                       │
├─────────────────────────────────────────────────────┤
│  services/registry-api                              │
│  FastAPI registry (deployed separately)             │
│  deps: fastapi, supabase, httpx, python-jose, etc.  │
│  bundle: none                                       │
└─────────────────────────────────────────────────────┘
```

### lilux

**Package name:** `lilux`
**Source:** `lilux/`
**Dependencies:** `cryptography`, `httpx`

The microkernel. Provides stateless async primitives: subprocess execution, HTTP client, Ed25519 signing, integrity hashing, lockfile I/O, and environment resolution. Lilux is **type-agnostic** — it has no knowledge of tools, directives, knowledge, `.ai/` directories, or Rye itself.

Lilux declares `cryptography` (for Ed25519 signing in `primitives/signing.py` and encrypted auth storage in `runtime/auth.py`) and `httpx` (for the HTTP client primitive in `primitives/http_client.py` and OAuth2 token refresh in `runtime/auth.py`) as its only two third-party dependencies.

Lilux does **not** contribute a bundle because it has no `.ai/` directory. It's pure library code.

### ryeos-core

**Package name:** `ryeos-core`
**Source:** `rye-core/` (builds from shared `rye/rye/` source)
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** `ryeos-core` → items under `rye/core/` only

The minimal installation. Contains the same Python code as `ryeos` (resolver, executor, metadata manager, etc.) but only registers the `ryeos-core` bundle — core runtimes, primitives, parsers, extractors, and bundler. No MCP tools, registry client, agent threads, or web search.

Use `ryeos-core` when you want the execution engine but don't need the full standard library.

> **Note:** `ryeos-core` and `ryeos` both install the `rye` Python module and are **mutually exclusive** — install one or the other, not both.

### ryeos

**Package name:** `ryeos`
**Source:** `rye/`
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** `ryeos` → all items under `rye/`

The full standard library. Same Python code as `ryeos-core`, but registers the `ryeos` bundle which includes **everything**: bash tool, MCP tools, registry client, agent thread system, web search, and all other bundled items.

This is the package to install when you want to call the executor directly from Python — no MCP transport needed. Useful for thread scripting, CI pipelines, or wrapping in a future `rye-cli`.

```python
# Direct execution without MCP:
from rye.tools.execute import ExecuteTool
executor = ExecuteTool()
result = await executor.run(item_type="tool", item_id="rye/bash/bash", parameters={"command": "ls"})
```

### ryeos-bare

**Package name:** `ryeos-bare`
**Source:** `rye/` (same source as `ryeos`)
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** none

Bare installation with no data-driven tools. Same Python code as `ryeos` but registers no bundle. Used by services like `registry-api` that need the engine but not any `.ai/` items.

> **Note:** `ryeos-bare`, `ryeos`, and `ryeos-core` all install the `rye` Python module and are **mutually exclusive** — install one only.

### ryeos-mcp

**Package name:** `ryeos-mcp`
**Source:** `rye-mcp/`
**Dependencies:** `ryeos`, `mcp`

The MCP server transport. Exposes the four Rye MCP tools over stdio or SSE so any MCP-compatible AI agent can use them. Does **not** register its own bundle — it inherits the `ryeos` bundle from its `ryeos` dependency.

### services/registry-api

**Package name:** N/A (deployed as a service)
**Source:** `services/registry-api/`
**Dependencies:** `fastapi`, `supabase`, `httpx`, `python-jose`, `pydantic`, `pydantic-settings`

A standalone FastAPI service for the item registry. Deployed separately (e.g., on Railway) and accessed via the bundled registry tool. Not installed as a pip package locally.

## Bundles vs Packages

A **package** is a pip-installable Python distribution. A **bundle** is a named set of `.ai/` items that a package contributes to the system space via the `rye.bundles` entry point group.

The distinction matters because:

1. **The `.ai/` data lives inside the `rye` Python module**, but different packages control how much of it is visible to the resolver.
2. **Bundle entry points filter by category.** `ryeos-core` only exposes `rye/core/*` items. `ryeos` exposes all `rye/*` items. `ryeos-bare` exposes nothing.
3. **Multiple bundles compose.** The resolver iterates over all discovered bundles, so a third-party package can register its own bundle entry point to contribute items to the system space.

### Author Key Trust

Every bundle ships the author's Ed25519 public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`. All items in the bundle are signed with this key. The Rye system bundle is signed by Leo Lilley — the same key used for registry publishing.

There are no exceptions to signature verification: system items go through the same verification as project and user items. The trust store uses standard 3-tier resolution (project → user → system), so the author's key in the system bundle is discovered automatically — no special bootstrap logic required.

### What you get with each install

```
pip install ryeos-core     → system space has: rye/core/* only
pip install ryeos          → system space has: rye/* (everything)
pip install ryeos-bare     → system space has: nothing (engine only)
pip install ryeos-mcp      → system space has: rye/* (via ryeos dep)
pip install ryeos my-tools → system space has: rye/* + my-tools/*
```

### Entry point registration

Each package registers its bundle in `pyproject.toml`:

```toml
# ryeos registers the full bundle:
[project.entry-points."rye.bundles"]
ryeos = "rye.bundle_entrypoints:get_ryeos_bundle"

# ryeos-core registers only core:
[project.entry-points."rye.bundles"]
ryeos-core = "rye.bundle_entrypoints:get_ryeos_core_bundle"

# ryeos-bare registers NO entry points (no bundle)
```

Both functions live in the same `rye/bundle_entrypoints.py`:

```python
def get_ryeos_bundle() -> dict:
    return {
        "bundle_id": "ryeos",
        "root_path": Path(__file__).parent,
        "categories": ["rye"],              # all rye/* items
    }

def get_ryeos_core_bundle() -> dict:
    return {
        "bundle_id": "ryeos-core",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],         # only rye/core/* items
    }
```

The author's signing key is shipped as a TOML identity document at `rye/.ai/trusted_keys/{fingerprint}.toml` within the bundle root, discovered via standard 3-tier resolution.

## Dependency Layering

Dependencies flow upward. Each package declares only what it directly imports:

```
ryeos-mcp
  ├── ryeos (or ryeos-core or ryeos-bare)
  │     ├── lilux
  │     │     ├── cryptography   (signing, auth encryption)
  │     │     └── httpx          (HTTP client primitive, OAuth2 refresh)
  │     ├── pyyaml              (YAML parsing for runtimes, configs)
  │     ├── cryptography        (also direct — metadata signing in rye)
  │     └── packaging           (semver parsing in chain validator)
  └── mcp                      (MCP protocol transport)
```

### What about bundled tools?

Bundled tools (Python scripts in `.ai/tools/`) are **not** Python package dependencies. They are data files that ship inside the wheel and execute at runtime via the executor chain. Their imports are resolved differently:

| Import location                                | Resolution                                                                                                                    |
| ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Core package code (`rye/rye/*.py`)             | Standard pip dependency — must be in `pyproject.toml`                                                                         |
| Bundled tools (`rye/rye/.ai/tools/**/*.py`)    | Resolved at runtime by the executor. The tool's runtime config handles interpreter selection, PYTHONPATH, and venv resolution |
| Tools with `DEPENDENCIES = [...]`              | Installed on-demand by `EnvManager` into the tool's venv at execution time                                                    |
| Lazy imports inside functions (`import httpx`) | Available if a transitive dependency provides it, but not guaranteed — prefer `DEPENDENCIES` for explicit declaration         |

Example: `websocket_sink.py` declares `DEPENDENCIES = ["websockets"]`. The executor's `EnvManager` ensures `websockets` is installed in the tool's environment before execution. This keeps the core package dependency list minimal.

## Package → Bundle Summary

| Package                  | pip name     | Dependencies                                   | Bundle ID    | Bundle scope | Mutual exclusion                              |
| ------------------------ | ------------ | ---------------------------------------------- | ------------ | ------------ | --------------------------------------------- |
| `lilux/`                 | `lilux`      | `cryptography`, `httpx`                        | —            | —            | —                                             |
| `rye/`                   | `ryeos`      | `lilux`, `pyyaml`, `cryptography`, `packaging` | `ryeos`      | `rye/*`      | ⚠️ conflicts with `ryeos-core`, `ryeos-bare`  |
| `rye-core/`              | `ryeos-core` | `lilux`, `pyyaml`, `cryptography`, `packaging` | `ryeos-core` | `rye/core/*` | ⚠️ conflicts with `ryeos`, `ryeos-bare`       |
| `rye/` (bare)            | `ryeos-bare` | `lilux`, `pyyaml`, `cryptography`, `packaging` | —            | —            | ⚠️ conflicts with `ryeos`, `ryeos-core`       |
| `rye-mcp/`               | `ryeos-mcp`  | `ryeos`, `mcp`                                 | —            | —            | —                                             |
| `services/registry-api/` | —            | `fastapi`, `supabase`, `httpx`, etc.           | —            | —            | —                                             |
