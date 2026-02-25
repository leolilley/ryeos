```yaml
id: packages-and-bundles
title: "Packages and Bundles"
description: How Rye OS is distributed — pip packages, bundle entry points, extras, and dependency layering
category: internals
tags: [packages, bundles, dependencies, distribution, pyproject, extras]
version: "2.0.0"
```

# Packages and Bundles

Rye OS is distributed as pip packages organized in a monorepo. Each package has a clear role, minimal dependencies, and optionally registers a **bundle** of `.ai/` items into the system space. This page explains the full package hierarchy, what each package ships, how bundles compose, and the install tiers.

## Monorepo Layout

```
lilux/
  kernel/            → pip: lilux        (microkernel + primitives)
  proc/              → pip: lilux-proc   (process lifecycle, Rust binary)
  watch/             → pip: lilux-watch  (file watcher, Rust binary)

ryeos/               → pip: ryeos        (engine + standard .ai/ bundle)
  bundles/
    core/            → pip: ryeos-core   (engine + rye/core .ai/ only)
    web/             → pip: ryeos-web    (rye/web .ai/ data bundle)
    code/            → pip: ryeos-code   (rye/code .ai/ data bundle)

ryeos-bare/          → pip: ryeos-bare   (engine, no .ai/)
ryeos-mcp/           → pip: ryeos-mcp   (MCP server transport)
```

## Install Tiers

Choose what you need:

```bash
# Standard — agent, bash, file-system, mcp, primary, core, authoring, guides (~3MB)
pip install ryeos

# Add web tools (browser automation, fetch, search)
pip install ryeos[web]    # or: pip install ryeos-web

# Add code tools (git, npm, typescript, LSP, diagnostics)
pip install ryeos[code]   # or: pip install ryeos-code

# Everything
pip install ryeos[all]

# Minimal — just rye/core (runtimes, primitives, bundler, extractors)
pip install ryeos-core

# Engine only — no data bundles at all
pip install ryeos-bare

# MCP server (pulls in ryeos automatically)
pip install ryeos-mcp
```

### What you get with each install

```
pip install ryeos-core     → system space has: rye/core/* only
pip install ryeos          → system space has: standard bundle (rye/agent/*, rye/bash/*, rye/core/*, rye/file-system/*, rye/mcp/*, rye/primary/*)
pip install ryeos[web]     → system space has: standard bundle + rye/web/*
pip install ryeos[code]    → system space has: standard bundle + rye/code/*
pip install ryeos[all]     → system space has: standard bundle + rye/web/* + rye/code/*
pip install ryeos-bare     → system space has: nothing (engine only)
pip install ryeos-mcp      → system space has: standard bundle (via ryeos dep)
pip install ryeos my-tools → system space has: standard bundle + my-tools/*
```

## Package Details

### lilux (microkernel)

```
┌─────────────────────────────────────────────────────┐
│  lilux                                              │
│  Stateless microkernel primitives                   │
│  deps: cryptography, httpx, lilux-proc              │
│  bundle: none (no .ai/ items)                       │
├─────────────────────────────────────────────────────┤
│  lilux-proc                                         │
│  Process lifecycle manager (Rust binary)             │
│  Hard dependency of lilux — all process operations  │
│  delegate to lilux-proc                             │
├─────────────────────────────────────────────────────┤
│  lilux-watch                                        │
│  Push-based file watcher (Rust binary)              │
│  Used by the Rust runtime for registry watching     │
│  Optional — installed when needed                   │
└─────────────────────────────────────────────────────┘
```

**`lilux`** — The microkernel. Provides stateless async primitives: subprocess execution, HTTP client, Ed25519 signing, integrity hashing, lockfile I/O, and environment resolution. Lilux is **type-agnostic** — it has no knowledge of tools, directives, knowledge, `.ai/` directories, or Rye itself.

Lilux depends on `lilux-proc` as a hard dependency — `SubprocessPrimitive.__init__()` resolves the `lilux-proc` binary via `shutil.which()` and raises `ConfigurationError` if not found. All process operations (exec, spawn, kill, status) delegate to `lilux-proc`.

**`lilux-proc`** — Cross-platform process lifecycle manager compiled as a Rust binary. Subcommands: `exec` (run-and-wait with stdout/stderr capture, timeout, stdin piping, cwd, and env support), `spawn` (detached/daemonized), `kill` (graceful SIGTERM → SIGKILL / TerminateProcess), `status` (is-alive check). Installed as a pip package that places the binary on `$PATH`.

**`lilux-watch`** — Push-based file watcher compiled as a Rust binary. Watches `registry.db` for thread status changes using OS-native file watchers (inotify on Linux, FSEvents/kqueue on macOS, ReadDirectoryChangesW on Windows). Used by the Rust runtime's `lilux-watch` tool as a push-based alternative to polling. Not a hard dependency — only needed when using the Rust runtime for thread watching.

Lilux does **not** contribute a bundle because it has no `.ai/` directory. It's pure library code.

### ryeos (standard bundle)

**Package name:** `ryeos`
**Source:** `ryeos/`
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Extras:** `[web]` → `ryeos-web`, `[code]` → `ryeos-code`, `[all]` → both
**Bundle:** `ryeos` → standard items under `rye/` (agent, bash, core, file-system, mcp, primary, authoring, guides)

The standard installation. Contains the resolver, executor, signing, metadata manager, and registers the `ryeos` bundle which includes the standard library: bash tool, file-system operations, MCP tools, agent thread system, primary tool wrappers, core runtimes, and creation directives. Approximately 3MB.

This is the package to install when you want a fully-featured Rye OS without web or code tools. Web and code tools are available as optional extras (`ryeos[web]`, `ryeos[code]`) or as standalone packages.

```python
# Direct execution without MCP:
from rye.tools.execute import ExecuteTool
executor = ExecuteTool()
result = await executor.run(item_type="tool", item_id="rye/bash/bash", parameters={"command": "ls"})
```

### ryeos-core (minimal bundle)

**Package name:** `ryeos-core`
**Source:** `ryeos/bundles/core/`
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** `ryeos-core` → items under `rye/core/` only

The minimal installation. Contains the same Python code as `ryeos` but only registers the `ryeos-core` bundle — core runtimes, primitives, parsers, extractors, and bundler. No agent tools, bash tool, file-system tools, MCP tools, registry client, or web/code tools.

Use `ryeos-core` when you want the execution engine but don't need the full standard library.

> **Note:** `ryeos-core` and `ryeos` both install the `rye` Python module and are **mutually exclusive** — install one or the other, not both.

### ryeos-web (web data bundle)

**Package name:** `ryeos-web`
**Source:** `ryeos/bundles/web/`
**Dependencies:** `ryeos`
**Bundle:** `ryeos-web` → items under `rye/web/`

Adds browser automation, web page fetching, and web search tools. Installs as a standalone package or via `pip install ryeos[web]`.

Tools provided: `rye/web/browser/browser` (Playwright-based browser automation), `rye/web/fetch/fetch` (web page fetching with format conversion), `rye/web/search/search` (web search via DuckDuckGo, Exa).

### ryeos-code (code data bundle)

**Package name:** `ryeos-code`
**Source:** `ryeos/bundles/code/`
**Dependencies:** `ryeos`
**Bundle:** `ryeos-code` → items under `rye/code/`

Adds development tools for package management, type checking, diagnostics, and LSP code intelligence. Installs as a standalone package or via `pip install ryeos[code]`.

Tools provided: `rye/code/npm/npm` (NPM/NPX operations), `rye/code/diagnostics/diagnostics` (linter/type checker runner), `rye/code/typescript/typescript` (TypeScript type checking), `rye/code/lsp/lsp` (LSP client — go to definition, references, hover).

> **Note:** `node_modules` are NOT shipped with the package. They are installed on first use via the node runtime's anchor system, which resolves and installs dependencies automatically.

### ryeos-bare (engine only)

**Package name:** `ryeos-bare`
**Source:** `ryeos-bare/`
**Dependencies:** `lilux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** none

Bare installation with no data-driven tools. Same Python code as `ryeos` but registers no bundle. Used by services like `registry-api` that need the engine but not any `.ai/` items.

> **Note:** `ryeos-bare`, `ryeos`, and `ryeos-core` all install the `rye` Python module and are **mutually exclusive** — install one only.

### ryeos-mcp (MCP transport)

**Package name:** `ryeos-mcp`
**Source:** `ryeos-mcp/`
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
2. **Bundle entry points filter by category.** `ryeos-core` only exposes `rye/core/*` items. `ryeos` exposes the standard set. `ryeos-web` exposes `rye/web/*`. `ryeos-code` exposes `rye/code/*`. `ryeos-bare` exposes nothing.
3. **Multiple bundles compose.** The resolver iterates over all discovered bundles via `get_system_spaces()`, so installing `ryeos` + `ryeos-web` + `ryeos-code` results in all three bundles being available in the system space. Third-party packages can register their own bundles the same way.

### How Bundles Compose

When multiple bundle packages are installed, `get_system_spaces()` discovers all `rye.bundles` entry points and returns them. The resolver iterates over all bundles when searching system space:

```
pip install ryeos ryeos-web ryeos-code
  → get_system_spaces() returns: [ryeos bundle, ryeos-web bundle, ryeos-code bundle]
  → system space search checks all three roots
  → rye/bash/bash found in ryeos, rye/web/fetch/fetch found in ryeos-web, rye/code/npm/npm found in ryeos-code
```

### Author Key Trust

Every bundle ships the author's Ed25519 public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`. All items in the bundle are signed with this key. The Rye system bundle is signed by Leo Lilley — the same key used for registry publishing.

There are no exceptions to signature verification: system items go through the same verification as project and user items. The trust store uses standard 3-tier resolution (project → user → system), so the author's key in the system bundle is discovered automatically — no special bootstrap logic required.

### Entry point registration

Each package registers its bundle in `pyproject.toml`:

```toml
# ryeos registers the standard bundle:
[project.entry-points."rye.bundles"]
ryeos = "rye.bundle_entrypoints:get_ryeos_bundle"

# ryeos-core registers only core:
[project.entry-points."rye.bundles"]
ryeos-core = "rye.bundle_entrypoints:get_ryeos_core_bundle"

# ryeos-web registers web tools:
[project.entry-points."rye.bundles"]
ryeos-web = "rye.bundle_entrypoints:get_ryeos_web_bundle"

# ryeos-code registers code tools:
[project.entry-points."rye.bundles"]
ryeos-code = "rye.bundle_entrypoints:get_ryeos_code_bundle"

# ryeos-bare registers NO entry points (no bundle)
```

Bundle entrypoint functions return a dict with `bundle_id`, `root_path`, `version`, and `categories`:

```python
def get_ryeos_bundle() -> dict:
    return {
        "bundle_id": "ryeos",
        "root_path": Path(__file__).parent,
        "categories": ["rye/agent", "rye/bash", "rye/core", "rye/file-system", "rye/mcp", "rye/primary"],
    }

def get_ryeos_web_bundle() -> dict:
    return {
        "bundle_id": "ryeos-web",
        "root_path": Path(__file__).parent,
        "categories": ["rye/web"],
    }

def get_ryeos_code_bundle() -> dict:
    return {
        "bundle_id": "ryeos-code",
        "root_path": Path(__file__).parent,
        "categories": ["rye/code"],
    }
```

The author's signing key is shipped as a TOML identity document at `rye/.ai/trusted_keys/{fingerprint}.toml` within the bundle root, discovered via standard 3-tier resolution.

## Dependency Layering

Dependencies flow upward. Each package declares only what it directly imports:

```
ryeos-mcp
  ├── ryeos (or ryeos-core or ryeos-bare)
  │     ├── lilux
  │     │     ├── lilux-proc      (hard dep — process lifecycle manager)
  │     │     ├── cryptography    (signing, auth encryption)
  │     │     └── httpx           (HTTP client primitive, OAuth2 refresh)
  │     ├── pyyaml               (YAML parsing for runtimes, configs)
  │     ├── cryptography         (also direct — metadata signing in rye)
  │     └── packaging            (semver parsing in chain validator)
  └── mcp                       (MCP protocol transport)

ryeos-web
  └── ryeos                     (inherits full engine)

ryeos-code
  └── ryeos                     (inherits full engine)
```

### What about bundled tools?

Bundled tools (Python scripts in `.ai/tools/`) are **not** Python package dependencies. They are data files that ship inside the wheel and execute at runtime via the executor chain. Their imports are resolved differently:

| Import location                                | Resolution                                                                                                                    |
| ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Core package code (`ryeos/rye/*.py`)           | Standard pip dependency — must be in `pyproject.toml`                                                                         |
| Bundled tools (`ryeos/rye/.ai/tools/**/*.py`)  | Resolved at runtime by the executor. The tool's runtime config handles interpreter selection, PYTHONPATH, and venv resolution |
| Tools with `DEPENDENCIES = [...]`              | Installed on-demand by `EnvManager` into the tool's venv at execution time                                                    |
| Lazy imports inside functions (`import httpx`) | Available if a transitive dependency provides it, but not guaranteed — prefer `DEPENDENCIES` for explicit declaration         |

Example: `websocket_sink.py` declares `DEPENDENCIES = ["websockets"]`. The executor's `EnvManager` ensures `websockets` is installed in the tool's environment before execution. This keeps the core package dependency list minimal.

Node.js tools (in `ryeos-code`) do not ship `node_modules`. Dependencies are installed on first use via the node runtime's anchor system, which detects a `package.json` at the anchor root and runs `npm install` automatically.

## Package → Bundle Summary

| Package                  | pip name      | Dependencies                                              | Bundle ID    | Bundle scope                    | Mutual exclusion                              |
| ------------------------ | ------------- | --------------------------------------------------------- | ------------ | ------------------------------- | --------------------------------------------- |
| `lilux/kernel/`          | `lilux`       | `lilux-proc`, `cryptography`, `httpx`                     | —            | —                               | —                                             |
| `lilux/proc/`            | `lilux-proc`  | (Rust binary)                                             | —            | —                               | —                                             |
| `lilux/watch/`           | `lilux-watch` | (Rust binary)                                             | —            | —                               | —                                             |
| `ryeos/`                 | `ryeos`       | `lilux`, `pyyaml`, `cryptography`, `packaging`            | `ryeos`      | standard `rye/*`                | ⚠️ conflicts with `ryeos-core`, `ryeos-bare`  |
| `ryeos/bundles/core/`    | `ryeos-core`  | `lilux`, `pyyaml`, `cryptography`, `packaging`            | `ryeos-core` | `rye/core/*`                    | ⚠️ conflicts with `ryeos`, `ryeos-bare`       |
| `ryeos/bundles/web/`     | `ryeos-web`   | `ryeos`                                                   | `ryeos-web`  | `rye/web/*`                     | —                                             |
| `ryeos/bundles/code/`    | `ryeos-code`  | `ryeos`                                                   | `ryeos-code` | `rye/code/*`                    | —                                             |
| `ryeos-bare/`            | `ryeos-bare`  | `lilux`, `pyyaml`, `cryptography`, `packaging`            | —            | —                               | ⚠️ conflicts with `ryeos`, `ryeos-core`       |
| `ryeos-mcp/`             | `ryeos-mcp`   | `ryeos`, `mcp`                                            | —            | —                               | —                                             |
| `services/registry-api/` | —             | `fastapi`, `supabase`, `httpx`, etc.                      | —            | —                               | —                                             |
