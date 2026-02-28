```yaml
id: packages-and-bundles
title: "Packages and Bundles"
description: How Rye OS is distributed — pip packages, bundle entry points, extras, and dependency layering
category: internals
tags: [packages, bundles, dependencies, distribution, pyproject, extras]
version: "2.0.0"
```

# Packages and Bundles

Rye OS is distributed as 10 pip packages organized in a monorepo. Each package has a clear role, minimal dependencies, its own Python namespace, and optionally registers a **bundle** of `.ai/` items into the system space. This page explains the full package hierarchy, what each package ships, how bundles compose, and the install tiers.

## Monorepo Layout

```
lillux/
  kernel/            → pip: lillux        (microkernel + primitives)
  proc/              → pip: lillux-proc   (process lifecycle, Rust binary)
  watch/             → pip: lillux-watch  (file watcher, Rust binary)

ryeos/               → pip: ryeos-engine  (ships rye/ module, no .ai/ data)
  bundles/
    core/            → pip: ryeos-core    (ryeos_core/.ai/rye/core/*)
    standard/        → pip: ryeos         (ryeos_std/.ai/rye/{agent,bash,...}/*)
    web/             → pip: ryeos-web     (ryeos_web/.ai/rye/web/*)
    code/            → pip: ryeos-code    (ryeos_code/.ai/rye/code/*)

ryeos-mcp/           → pip: ryeos-mcp    (rye_mcp/ module)
ryeos-cli/           → pip: ryeos-cli   (rye_cli/ module)
```

## Install Tiers

Choose what you need:

```bash
# Engine only — no .ai/ data (for services/embedding)
pip install ryeos-engine

# Minimal — just rye/core (runtimes, primitives, bundler, extractors)
pip install ryeos-core

# Standard — engine + core + agent, bash, file-system, mcp, primary, authoring, guides
pip install ryeos

# Add web tools (browser automation, fetch, search)
pip install ryeos[web]

# Add code tools (git, npm, typescript, LSP, diagnostics)
pip install ryeos[code]

# Everything
pip install ryeos[all]

# MCP server (pulls in ryeos automatically)
pip install ryeos-mcp

# Terminal CLI (pulls in ryeos automatically)
pip install ryeos-cli

# MCP server + code tools
pip install ryeos-mcp[code]
```

### What you get with each install

```
pip install ryeos-engine   → engine only, no .ai/ data
pip install ryeos-core     → engine + rye/core/* only
pip install ryeos          → engine + core + standard items (rye/agent/*, rye/bash/*, rye/file-system/*, rye/mcp/*, rye/primary/*, rye/authoring/*, rye/guides/*)
pip install ryeos[web]     → + rye/web/*
pip install ryeos[code]    → + rye/code/*
pip install ryeos[all]     → everything
pip install ryeos-mcp      → ryeos + MCP transport
pip install ryeos-cli     → ryeos + terminal CLI (rye command)
pip install ryeos-mcp[code] → ryeos + MCP + rye/code/*
pip install ryeos my-tools → standard + my-tools/*
```

## Package Details

### lillux (microkernel)

```
┌─────────────────────────────────────────────────────┐
│  lillux                                              │
│  Stateless microkernel primitives                   │
│  deps: cryptography, httpx, lillux-proc              │
│  bundle: none (no .ai/ items)                       │
├─────────────────────────────────────────────────────┤
│  lillux-proc                                         │
│  Process lifecycle manager (Rust binary)             │
│  Hard dependency of lillux — all process operations  │
│  delegate to lillux-proc                             │
├─────────────────────────────────────────────────────┤
│  lillux-watch                                        │
│  Push-based file watcher (Rust binary)              │
│  Used by the Rust runtime for registry watching     │
│  Optional — installed when needed                   │
└─────────────────────────────────────────────────────┘
```

**`lillux`** — The microkernel. Provides stateless async primitives: subprocess execution, HTTP client, Ed25519 signing, integrity hashing, lockfile I/O, and environment resolution. Lillux is **type-agnostic** — it has no knowledge of tools, directives, knowledge, `.ai/` directories, or Rye itself.

Lillux depends on `lillux-proc` as a hard dependency — `SubprocessPrimitive.__init__()` resolves the `lillux-proc` binary via `shutil.which()` and raises `ConfigurationError` if not found. All process operations (exec, spawn, kill, status) delegate to `lillux-proc`.

**`lillux-proc`** — Cross-platform process lifecycle manager compiled as a Rust binary. Subcommands: `exec` (run-and-wait with stdout/stderr capture, timeout, stdin piping, cwd, and env support), `spawn` (detached/daemonized), `kill` (graceful SIGTERM → SIGKILL / TerminateProcess), `status` (is-alive check). Installed as a pip package that places the binary on `$PATH`.

**`lillux-watch`** — Push-based file watcher compiled as a Rust binary. Watches `registry.db` for thread status changes using OS-native file watchers (inotify on Linux, FSEvents/kqueue on macOS, ReadDirectoryChangesW on Windows). Used by the Rust runtime's `lillux-watch` tool as a push-based alternative to polling. Not a hard dependency — only needed when using the Rust runtime for thread watching.

Lillux does **not** contribute a bundle because it has no `.ai/` directory. It's pure library code.

### ryeos-engine (execution engine)

**Package name:** `ryeos-engine`
**Source:** `ryeos/`
**Python module:** `rye/`
**Dependencies:** `lillux`, `pyyaml`, `cryptography`, `packaging`
**Bundle:** none — no `.ai/` data, no entry point

The execution engine. Contains the resolver, executor, signing, metadata manager, and all core Python code in the `rye/` module. Ships **no** `.ai/` items — bundles are delivered by separate packages that depend on `ryeos-engine`.

Use `ryeos-engine` when you need the engine but no `.ai/` data at all (e.g., services that embed the engine, or custom deployments that provide their own bundles).

```python
# Direct execution without MCP:
from rye.tools.execute import ExecuteTool
executor = ExecuteTool()
result = await executor.run(item_type="tool", item_id="rye/bash/bash", parameters={"command": "ls"})
```

### ryeos-core (core data bundle)

**Package name:** `ryeos-core`
**Source:** `ryeos/bundles/core/`
**Python module:** `ryeos_core/`
**Dependencies:** `ryeos-engine`
**Bundle:** `ryeos-core` → items under `rye/core/` only
**Entry point:** `ryeos-core = "ryeos_core.bundle:get_bundle"`
**Categories:** `["rye/core"]`

Data-only bundle that ships core runtimes, primitives, parsers, extractors, bundler, and trusted author keys. Depends on `ryeos-engine` for the execution engine — it's additive, not a variant.

Use `ryeos-core` when you want the engine plus core items but don't need the full standard library.

### ryeos (standard data bundle)

**Package name:** `ryeos`
**Source:** `ryeos/bundles/standard/`
**Python module:** `ryeos_std/`
**Dependencies:** `ryeos-core`
**Extras:** `[web]` → `ryeos-web`, `[code]` → `ryeos-code`, `[all]` → both
**Bundle:** `ryeos` → items under `rye/` (agent, bash, file-system, mcp, primary, authoring, guides)
**Entry point:** `ryeos = "ryeos_std.bundle:get_bundle"`
**Categories:** `["rye"]`

The standard installation. A data-only bundle that ships the standard library: bash tool, file-system operations, MCP tools, agent thread system, primary tool wrappers, and creation directives. Since it depends on `ryeos-core`, installing `ryeos` gives you the engine + core + standard items.

Web and code tools are available as optional extras (`ryeos[web]`, `ryeos[code]`) or as standalone packages.

### ryeos-web (web data bundle)

**Package name:** `ryeos-web`
**Source:** `ryeos/bundles/web/`
**Python module:** `ryeos_web/`
**Dependencies:** `ryeos`
**Bundle:** `ryeos-web` → items under `rye/web/`
**Entry point:** `ryeos-web = "ryeos_web.bundle:get_bundle"`
**Categories:** `["rye/web"]`

Adds browser automation, web page fetching, and web search tools. Installs as a standalone package or via `pip install ryeos[web]`.

Tools provided: `rye/web/browser/browser` (Playwright-based browser automation), `rye/web/fetch/fetch` (web page fetching with format conversion), `rye/web/search/search` (web search via DuckDuckGo, Exa).

### ryeos-code (code data bundle)

**Package name:** `ryeos-code`
**Source:** `ryeos/bundles/code/`
**Python module:** `ryeos_code/`
**Dependencies:** `ryeos`
**Bundle:** `ryeos-code` → items under `rye/code/`
**Entry point:** `ryeos-code = "ryeos_code.bundle:get_bundle"`
**Categories:** `["rye/code"]`

Adds development tools for package management, type checking, diagnostics, and LSP code intelligence. Installs as a standalone package or via `pip install ryeos[code]`.

Tools provided: `rye/code/npm/npm` (NPM/NPX operations), `rye/code/diagnostics/diagnostics` (linter/type checker runner), `rye/code/typescript/typescript` (TypeScript type checking), `rye/code/lsp/lsp` (LSP client — go to definition, references, hover).

> **Note:** `node_modules` are NOT shipped with the package. They are installed on first use via the node runtime's anchor system, which resolves and installs dependencies automatically.

### ryeos-mcp (MCP transport)

**Package name:** `ryeos-mcp`
**Source:** `ryeos-mcp/`
**Python module:** `rye_mcp/`
**Dependencies:** `ryeos`, `mcp`
**Extras:** `[web]` → `ryeos-web`, `[code]` → `ryeos-code`, `[all]` → both
**Bundle:** none — inherits bundles from its `ryeos` dependency

The MCP server transport. Exposes the four Rye MCP tools over stdio or SSE so any MCP-compatible AI agent can use them. Does **not** register its own bundle — it inherits bundles from its `ryeos` dependency chain.

### ryeos-cli (terminal CLI)

**Package name:** `ryeos-cli`
**Source:** `ryeos-cli/`
**Python module:** `rye_cli/`
**Dependencies:** `ryeos`, `pyyaml`
**Bundle:** none — inherits bundles from its `ryeos` dependency

The terminal-native CLI. Maps shell verbs (`search`, `load`, `execute`, `sign`, `thread`, `graph`, `test`) directly to the four RYE primitives — no MCP transport. Imports `ryeos` directly as a Python library for zero-overhead invocation.

Use `ryeos-cli` when you want to invoke RYE from the terminal without an MCP client — CI scripts, graph operations, test running, or interactive development.

```bash
rye search directive "lead generation"
rye execute tool rye/bash/bash --params '{"command": "ls"}'
rye graph run my-project/graphs/pipeline --params '{"min_ccu": 50000}'
rye graph validate my-project/graphs/pipeline
rye test my-project/tools/scraper --exclude-tags integration
```

### services/registry-api

**Package name:** N/A (deployed as a service)
**Source:** `services/registry-api/`
**Dependencies:** `fastapi`, `supabase`, `httpx`, `python-jose`, `pydantic`, `pydantic-settings`

A standalone FastAPI service for the item registry. Deployed separately (e.g., on Railway) and accessed via the bundled registry tool. Not installed as a pip package locally.

## Bundles vs Packages

A **package** is a pip-installable Python distribution. A **bundle** is a named set of `.ai/` items that a package contributes to the system space via the `rye.bundles` entry point group.

The distinction matters because:

1. **Each bundle has its own Python namespace.** `ryeos-engine` ships the `rye/` module. `ryeos-core` ships `ryeos_core/`. `ryeos` (standard) ships `ryeos_std/`. `ryeos-web` ships `ryeos_web/`. `ryeos-code` ships `ryeos_code/`. There is no mutual exclusion — packages are additive.
2. **Bundle entry points filter by category.** `ryeos-core` only exposes `rye/core/*` items. `ryeos` exposes the standard set. `ryeos-web` exposes `rye/web/*`. `ryeos-code` exposes `rye/code/*`. `ryeos-engine` exposes nothing.
3. **Multiple bundles compose.** The resolver iterates over all discovered bundles via `get_system_spaces()`, so installing `ryeos` + `ryeos-web` + `ryeos-code` results in all three bundles being available in the system space. Third-party packages can register their own bundles the same way.

### How Bundles Compose

When multiple bundle packages are installed, `get_system_spaces()` discovers all `rye.bundles` entry points and returns them. The resolver iterates over all bundles when searching system space:

```
pip install ryeos ryeos-web ryeos-code
  → get_system_spaces() returns: [ryeos-core bundle, ryeos bundle, ryeos-web bundle, ryeos-code bundle]
  → system space search checks all four roots
  → rye/bash/bash found in ryeos, rye/web/fetch/fetch found in ryeos-web, rye/code/npm/npm found in ryeos-code
```

### Author Key Trust

Every bundle ships the author's Ed25519 public key as a TOML identity document at `.ai/config/keys/trusted/{fingerprint}.toml`. All items in the bundle are signed with this key. The Rye system bundle is signed by Leo Lilley — the same key used for registry publishing. The trusted key file itself is signed with an inline `# rye:signed:...` signature (self-signed by the author's key), so its integrity can be verified independently of the bundle manifest.

The bundler collects `.ai/config/keys/trusted/` files into bundle manifests alongside directives, tools, and knowledge. This means `bundle verify` covers key file integrity via SHA256 hashes — the same verification path used for every other item in the bundle.

There are no exceptions to signature verification: system items go through the same verification as project and user items. The trust store uses standard 3-tier resolution (project → user → system), so the author's key in the system bundle is discovered automatically — no special bootstrap logic required.

### Entry point registration

Each bundle package registers its own entry point in `pyproject.toml`, pointing to a `bundle.py` in its own module:

```toml
# ryeos-core registers core items:
[project.entry-points."rye.bundles"]
ryeos-core = "ryeos_core.bundle:get_bundle"

# ryeos (standard) registers standard items:
[project.entry-points."rye.bundles"]
ryeos = "ryeos_std.bundle:get_bundle"

# ryeos-web registers web tools:
[project.entry-points."rye.bundles"]
ryeos-web = "ryeos_web.bundle:get_bundle"

# ryeos-code registers code tools:
[project.entry-points."rye.bundles"]
ryeos-code = "ryeos_code.bundle:get_bundle"

# ryeos-engine registers NO entry points (no bundle)
```

Bundle entrypoint functions return a dict with `bundle_id`, `root_path`, `version`, and `categories`:

```python
# ryeos_core/bundle.py
def get_bundle() -> dict:
    return {
        "bundle_id": "ryeos-core",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }

# ryeos_std/bundle.py
def get_bundle() -> dict:
    return {
        "bundle_id": "ryeos",
        "root_path": Path(__file__).parent,
        "categories": ["rye"],
    }

# ryeos_web/bundle.py
def get_bundle() -> dict:
    return {
        "bundle_id": "ryeos-web",
        "root_path": Path(__file__).parent,
        "categories": ["rye/web"],
    }

# ryeos_code/bundle.py
def get_bundle() -> dict:
    return {
        "bundle_id": "ryeos-code",
        "root_path": Path(__file__).parent,
        "categories": ["rye/code"],
    }
```

The author's signing key is provisioned into each bundle's `.ai/config/keys/trusted/` via the keys tool (`rye execute tool rye/core/keys/keys --action trust --space project` from each bundle's root). The key is stored as a self-signed TOML identity document at `.ai/config/keys/trusted/{fingerprint}.toml`, discovered via standard 3-tier resolution.

## Dependency Chain

Dependencies are strictly additive. Each package depends on the layer below it:

```
ryeos-engine ← ryeos-core ← ryeos ← ryeos-mcp
                                   ← ryeos-cli
                                   ← ryeos-web
                                   ← ryeos-code
```

Full dependency tree:

```
ryeos-mcp
  ├── ryeos
  │     ├── ryeos-core
  │     │     └── ryeos-engine
  │     │           ├── lillux
  │     │           │     ├── lillux-proc      (hard dep — process lifecycle manager)
  │     │           │     ├── cryptography    (signing, auth encryption)
  │     │           │     └── httpx           (HTTP client primitive, OAuth2 refresh)
  │     │           ├── pyyaml               (YAML parsing for runtimes, configs)
  │     │           ├── cryptography         (also direct — metadata signing in rye)
  │     │           └── packaging            (semver parsing in chain validator)
  │     └── (standard .ai/ items via ryeos_std)
  └── mcp                       (MCP protocol transport)

ryeos-cli
  ├── ryeos                     (inherits full engine + core + standard)
  └── pyyaml                    (YAML parsing)

ryeos-web
  └── ryeos                     (inherits full engine + core + standard)

ryeos-code
  └── ryeos                     (inherits full engine + core + standard)
```

### What about bundled tools?

Bundled tools (Python scripts in `.ai/tools/`) are **not** Python package dependencies. They are data files that ship inside the wheel and execute at runtime via the executor chain. Their imports are resolved differently:

| Import location                                | Resolution                                                                                                                    |
| ---------------------------------------------- | ----------------------------------------------------------------------------------------------------------------------------- |
| Core package code (`ryeos/rye/*.py`)           | Standard pip dependency — must be in `pyproject.toml`                                                                         |
| Bundled tools (`.ai/tools/**/*.py`)            | Resolved at runtime by the executor. The tool's runtime config handles interpreter selection, PYTHONPATH, and venv resolution |
| Tools with `DEPENDENCIES = [...]`              | Installed on-demand by `EnvManager` into the tool's venv at execution time                                                    |
| Lazy imports inside functions (`import httpx`) | Available if a transitive dependency provides it, but not guaranteed — prefer `DEPENDENCIES` for explicit declaration         |

Example: `websocket_sink.py` declares `DEPENDENCIES = ["websockets"]`. The executor's `EnvManager` ensures `websockets` is installed in the tool's environment before execution. This keeps the core package dependency list minimal.

Node.js tools (in `ryeos-code`) do not ship `node_modules`. Dependencies are installed on first use via the node runtime's anchor system, which detects a `package.json` at the anchor root and runs `npm install` automatically.

## Package → Bundle Summary

| Package | pip name | Dependencies | Bundle ID | Bundle scope |
| --- | --- | --- | --- | --- |
| `lillux/kernel/` | `lillux` | `lillux-proc`, `cryptography`, `httpx` | — | — |
| `lillux/proc/` | `lillux-proc` | (Rust binary) | — | — |
| `lillux/watch/` | `lillux-watch` | (Rust binary) | — | — |
| `ryeos/` | `ryeos-engine` | `lillux`, `pyyaml`, `cryptography`, `packaging` | — | — |
| `ryeos/bundles/core/` | `ryeos-core` | `ryeos-engine` | `ryeos-core` | `rye/core/*` |
| `ryeos/bundles/standard/` | `ryeos` | `ryeos-core` | `ryeos` | standard `rye/*` |
| `ryeos/bundles/web/` | `ryeos-web` | `ryeos` | `ryeos-web` | `rye/web/*` |
| `ryeos/bundles/code/` | `ryeos-code` | `ryeos` | `ryeos-code` | `rye/code/*` |
| `ryeos-mcp/` | `ryeos-mcp` | `ryeos`, `mcp` | — | — |
| `ryeos-cli/` | `ryeos-cli` | `ryeos`, `pyyaml` | — | — |
| `services/registry-api/` | — | `fastapi`, `supabase`, `httpx`, etc. | — | — |

## Publishing Order

Packages must be published to PyPI in dependency order. The two Rust packages (`lillux-proc`, `lillux-watch`) have no Python dependencies and can be published first. Then each layer unlocks the next:

```
 ┌─────────────────────────────────────────────────────────────────┐
 │  LAYER 1 — Standalone (no Python deps)                         │
 │                                                                 │
 │   lillux-proc   (Rust binary, maturin)                          │
 │   lillux-watch  (Rust binary, maturin)                          │
 └──────────────────────┬──────────────────────────────────────────┘
                        │
 ┌──────────────────────▼──────────────────────────────────────────┐
 │  LAYER 2 — Microkernel                                         │
 │                                                                 │
 │   lillux        (Python, depends on lillux-proc)                 │
 └──────────────────────┬──────────────────────────────────────────┘
                        │
 ┌──────────────────────▼──────────────────────────────────────────┐
 │  LAYER 3 — Engine                                               │
 │                                                                 │
 │   ryeos-engine  (ships rye/ module, no .ai/ data)               │
 └──────────────────────┬──────────────────────────────────────────┘
                        │
 ┌──────────────────────▼──────────────────────────────────────────┐
 │  LAYER 4 — Core bundle                                         │
 │                                                                 │
 │   ryeos-core    (data bundle — rye/core/*)                      │
 └──────────────────────┬──────────────────────────────────────────┘
                        │
 ┌──────────────────────▼──────────────────────────────────────────┐
 │  LAYER 5 — Standard bundle                                     │
 │                                                                 │
 │   ryeos         (data bundle — standard rye/*)                  │
 └──────────────────────┬──────────────────────────────────────────┘
                        │
 ┌──────────────────────▼──────────────────────────────────────────┐
 │  LAYER 6 — Extensions (depend on ryeos)                        │
 │                                                                 │
 │   ryeos-web     (data bundle — rye/web/*)                       │
 │   ryeos-code    (data bundle — rye/code/*)                      │
 │   ryeos-mcp     (MCP server transport)                          │
 │   ryeos-cli     (terminal CLI)                                  │
 └─────────────────────────────────────────────────────────────────┘
```

### Code Packages vs Data Bundles

Code packages contain Python or Rust source code that implements functionality:

| Package        | Type                  | What it ships                                      |
| -------------- | --------------------- | -------------------------------------------------- |
| `lillux-proc`  | Rust binary           | Process lifecycle manager                          |
| `lillux-watch` | Rust binary           | File watcher                                       |
| `lillux`       | Python library        | Microkernel primitives (subprocess, signing, HTTP) |
| `ryeos-engine` | Python library        | Execution engine (`rye/` module), no `.ai/` data   |
| `ryeos-mcp`    | Python library        | MCP server transport (`rye_mcp/` module)           |
| `ryeos-cli`    | Python library        | Terminal CLI (`rye_cli/` module)                    |

Data bundles are primarily `.ai/` item collections with minimal Python glue:

| Package      | What it ships                                                  |
| ------------ | -------------------------------------------------------------- |
| `ryeos-core` | `ryeos_core/.ai/` items (rye/core/*) + `bundle.py` entrypoint |
| `ryeos`      | `ryeos_std/.ai/` items (standard rye/*) + `bundle.py` entrypoint |
| `ryeos-web`  | `ryeos_web/.ai/` items (rye/web/*) + `bundle.py` entrypoint   |
| `ryeos-code` | `ryeos_code/.ai/` items (rye/code/*) + `bundle.py` entrypoint |
