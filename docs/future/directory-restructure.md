```yaml
id: directory-restructure
title: ".ai/ Directory Restructure — Four-Authority Model"
description: Restructure .ai/ directories to reflect the four-authority model. Prerequisites for ryeosd implementation.
category: future
tags: [architecture, directories, restructure, prerequisites, ryeosd]
version: "0.2.0"
status: proposed
```

# .ai/ Directory Restructure — Four-Authority Model

> **Status:** Proposed — prerequisite for ryeosd phases

## Motivation

The current `.ai/` structure accumulated organically. `agent/` holds thread state but the agent is the signing key. `lockfiles/` exist but CAS handles reproducibility. `config/cas/remote.yaml` mixes remote node definitions with sync policy. The structure doesn't reflect the architecture.

With ryeosd unifying local and remote execution, every machine running the daemon becomes a node. Node state needs a formal home. The directory structure needs to match the system's actual authority model before ryeosd phases begin.

## The Four-Authority Model

Every directory in `.ai/` belongs to one of four authorities. The authority determines who signs it, whether it's portable, whether it's CAS-synced, and how it's managed.

| Authority   | What it governs                                 | Where it lives                   | Signed by                | Portable             | CAS-synced |
| ----------- | ----------------------------------------------- | -------------------------------- | ------------------------ | -------------------- | ---------- |
| **User**    | Identity, preferences, authored items           | `~/.ai/config/`, `~/.ai/{items}` | Your key                 | Items yes, config no | Items yes  |
| **Project** | Authored items, project config                  | `<project>/.ai/`                 | Your key                 | Yes                  | Yes        |
| **Node**    | Machine identity, attestation, ingress, secrets | `~/.ai/node/`                    | Node key                 | No                   | No         |
| **Runtime** | Execution state, CAS blobs, caches              | `<space>/.ai/state/`             | See contract table below | No                   | No         |

Runtime state is comprehensively integrity-verified: transcripts are checkpoint-signed (Ed25519 over byte offset), thread.json is signed (`sign_json`), CAS objects are SHA256 hash-verified, execution results are dual-signed (your key + node key). Only `cache/` is unsigned ephemeral.

### Resolution rules

- **Item types** (directives, tools, knowledge): project → user → system (first match wins, project shadows user, user shadows system)
- **Config**: project → user → system (3-tier merge/override, project overrides user defaults)
- **Node state**: `~/.ai/node/` only. Never resolved from project space. Enforced structurally — node path getters read only from `~/.ai/node/`, never use the generic 3-tier resolver.
- **Node-consumed config** (`config/node/`): resolves through normal config tiers (project → user → system). This is authored config the node reads, not node state.
- **Runtime state**: scoped to the space that produced it. Project threads live in project `state/`. Node execution records live in `~/.ai/node/executions/`.

### `config/node/` vs `~/.ai/node/` boundary

These are different things:

|                           | `config/node/`                                | `~/.ai/node/`                                                        |
| ------------------------- | --------------------------------------------- | -------------------------------------------------------------------- |
| **What**                  | Authored declarations the node consumes       | Machine-local node identity and mutable state                        |
| **Examples**              | Route declarations, policy hints              | Node keypair, attestation, authorized-keys, vault, execution records |
| **Resolution**            | Normal config tiers (project → user → system) | User space only, never project space                                 |
| **Mutable at runtime**    | No — authored, signed                         | Yes — node writes to it                                              |
| **Project space allowed** | Yes (route declarations, policy)              | No — forbidden                                                       |

**`daemon.yaml` is user-space only.** Bind addresses and listen ports are machine-local operational config, not project-authored portable config. `daemon.yaml` MUST NOT appear in project-space `config/node/`.

## Target Structure

### Project space (`<project>/.ai/`)

```
.ai/
  # Item types — authored, signed, content-addressed
  directives/
  tools/
  knowledge/

  # Config — authored, signed, consumer-domain namespaces
  config/
    keys/                    # project-level trust config
    agent/                   # agent behavior overrides for this project
    execution/               # execution policy overrides
    remotes/                 # project-specific remote definitions
    node/                    # route declarations only (NOT daemon.yaml)
      routes/                # one YAML per route — additive, bundle-shippable
    cas/                     # sync/manifest policy
    <tool-namespace>/        # tool/bundle config (web/, email/, etc)

  # Installed — from registry
  bundles/

  # Runtime — engine-managed, gitignored
  state/
    threads/                 # thread registry (registry.db), transcripts, budgets
    graphs/                  # graph run state (transcript.jsonl per run)
    objects/                 # CAS blobs (SHA256-sharded)
    cache/                   # tool runtime cache
```

### User space (`~/.ai/`)

```
~/.ai/
  # Item types — personal items, resolve into every project
  directives/
  tools/
  knowledge/

  # Config — personal defaults
  config/
    keys/
      signing/               # your Ed25519 keypair (THE agent identity)
      trusted/               # trusted author key TOML files (TOFU)
    agent/
      agent.yaml             # provider defaults (e.g. provider: default: anthropic)
    execution/
      execution.yaml         # timeout defaults, per-tool overrides
    remotes/
      remotes.yaml           # named remote ryeosd instances + registry
    node/
      daemon.yaml            # this instance's bind config (user-space ONLY)
      routes/                # user-level route declarations
    cas/
      manifest.yaml          # sync exclusion policy
    web/                     # tool-specific config
      browser.json

  # Node — THIS MACHINE's identity and state (never in project space)
  node/
    identity/                # node Ed25519 keypair (generated on first boot)
    attestation/             # hardware, capabilities, isolation, restrictions
    authorized-keys/         # who can talk to this node (TOFU + admin CRUD)
    vault/                   # HPKE-encrypted secrets sealed to this node's key
    executions/              # execution records/state
    logs/                    # daemon operational logs

  # Runtime
  state/
    threads/
    graphs/
    objects/
    cache/
```

### Directory semantics contract

| Directory                     | Authority    | Signed                                                                                                                                            | Portable         | CAS-synced       | GC-managed | Secret         |
| ----------------------------- | ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------- | ---------------- | ---------------- | ---------- | -------------- |
| `directives/`                 | User/Project | ✅ Your key                                                                                                                                       | ✅               | ✅               | ❌         | ❌             |
| `tools/`                      | User/Project | ✅ Your key                                                                                                                                       | ✅               | ✅               | ❌         | ❌             |
| `knowledge/`                  | User/Project | ✅ Your key                                                                                                                                       | ✅               | ✅               | ❌         | ❌             |
| `config/`                     | User/Project | ✅ Your key                                                                                                                                       | Config-dependent | Config-dependent | ❌         | Keys only      |
| `bundles/`                    | User/Project | ✅ Author key                                                                                                                                     | ✅               | ✅               | ❌         | ❌             |
| `state/threads/`              | Runtime      | ✅ Transcript checkpoint-signed (Ed25519 over byte offset), thread.json signed (`sign_json`), execution results dual-signed (your key + node key) | ❌               | ❌               | ✅         | ❌             |
| `state/graphs/`               | Runtime      | ✅ Graph transcripts checkpoint-signed, state snapshots in CAS                                                                                    | ❌               | ❌               | ✅         | ❌             |
| `state/objects/`              | Runtime      | ✅ SHA256 hash-verified (content-addressed), domain objects signed via `sign_object` convention                                                   | ❌               | ❌               | ✅         | ❌             |
| `state/cache/`                | Runtime      | ❌                                                                                                                                                | ❌               | ❌               | ✅         | ❌             |
| `~/.ai/node/identity/`        | Node         | ✅ Node key                                                                                                                                       | ❌               | ❌               | ❌         | ✅ Private key |
| `~/.ai/node/attestation/`     | Node         | ✅ Node key                                                                                                                                       | ❌               | ❌               | ❌         | ❌             |
| `~/.ai/node/authorized-keys/` | Node         | ✅ Node key                                                                                                                                       | ❌               | ❌               | ❌         | ❌             |
| `~/.ai/node/vault/`           | Node         | ✅ Node key                                                                                                                                       | ❌               | ❌               | ❌         | ✅ Encrypted   |
| `~/.ai/node/executions/`      | Node         | ✅ Node key                                                                                                                                       | ❌               | ❌               | ✅         | ❌             |
| `~/.ai/node/logs/`            | Node         | ❌                                                                                                                                                | ❌               | ❌               | ✅         | ❌             |

### Config namespace convention

Config directories are organized by **consumer domain**, not ontology. Files are the merge units.

- `keys/` — identity + trust layer
- `agent/` — agent behavior (provider, hooks, resilience). **This is NOT renamed — `config/agent/` stays. It is agent behavior config, separate from the `state/` rename.**
- `execution/` — runtime execution policy (timeouts, per-tool overrides)
- `remotes/` — named remote ryeosd instances (absorbs old `cas/remote.yaml` and `registry/registry.yaml`)
- `node/` — declarations this node instance consumes (routes). `daemon.yaml` in user-space only.
- `cas/` — sync/manifest policy only (not remotes)
- `<tool-namespace>/` — tool/bundle config (web/, email/, etc) — grows organically with bundles

### Route config format

Routes use a directory of individual YAML files, not a single monolithic file. This supports additive composition from bundles:

```
config/node/routes/
  tracking-pixel.yaml
  unsubscribe.yaml
  inbound-webhook.yaml
```

Each file defines one route. Bundles ship their routes in their own `config/node/routes/` which merge additively. See [node-custom-routes.md](../../.tmp/node-custom-routes.md) for the route YAML schema.

### Registry selection in remotes

Registry is a named remote with a `roles` field:

```yaml
# config/remotes/remotes.yaml
remotes:
  default:
    url: https://node-1.internal
    key_env: NODE_1_KEY
  registry:
    url: https://ryeos--ryeos-node-node.modal.run
    roles: [registry]
```

Publish/search operations select the remote with `roles: [registry]`. If no remote has the registry role, operations fail with a clear error.

---

## Prerequisites

Eight changes that must land before ryeosd Phase 1. Each is independently implementable. Dependency order shown below.

### 1. Directory contract doc

This document. Formalizes the four-authority model, directory semantics, and resolution rules. Governs everything that follows.

### 2. Remove HTTP primitive

Remove `HttpClientPrimitive` as a separate execution primitive. One execution primitive. Every chain terminates at Execute → Lillux.

**Changes:**

- Delete `ryeos/rye/runtime/http_client.py`
- Rename `ryeos/rye/primitives/subprocess.py` → `execute.py`
- Rename `SubprocessPrimitive` → `ExecutePrimitive`, `SubprocessResult` → `ExecuteResult`
- Update `PRIMITIVE_MAP` in `primitive_executor.py` — single entry: `rye/core/primitives/execute`
- Update `primitives/__init__.py` exports
- Convert tools chaining to `http_client` primitive into Python tools with `httpx`
- Update/remove `rye/core/primitives/http_client` item
- Rename `rye/core/primitives/subprocess` item → `rye/core/primitives/execute`
- Update system knowledge, docs, tests

**See:** [remove-http-primitive.md](remove-http-primitive.md)

**Done when:**

- `rg HttpClientPrimitive` returns zero results
- `rg "primitives/http_client"` returns zero results
- `rg SubprocessPrimitive` returns zero results
- `PRIMITIVE_MAP` has exactly one entry: `rye/core/primitives/execute` → `ExecutePrimitive`
- All tools that previously chained to HTTP primitive have been converted to Python tools with `httpx`
- All tests pass

### 3. Rename `agent/` → `state/` + restructure

Rename the runtime state directory. The agent is the signing key, not a directory.

**Rename `rye/agent/threads/*` item IDs too** — these physically move from `agent/` to `state/`. All other `rye/agent/*` namespaces (providers, permissions, config-schemas, core, graphs) are item namespaces inside `tools/`/`knowledge/`/`directives/` and are out of scope.

| Old                                 | New                                                               |
| ----------------------------------- | ----------------------------------------------------------------- |
| `rye/agent/threads/*` (all)         | `rye/state/threads/*`                                             |
| `.ai/tools/rye/agent/threads/`      | `.ai/tools/rye/state/threads/`                                    |
| `.ai/knowledge/rye/agent/threads/`  | `.ai/knowledge/rye/state/threads/`                                |
| `.ai/directives/rye/agent/threads/` | `.ai/directives/rye/state/threads/`                               |
| `.ai/knowledge/agent/graphs/`       | `.ai/knowledge/state/graphs/` (runtime-generated graph knowledge) |
| `category = "agent/graphs/{id}"`    | `category = "state/graphs/{id}"` (walker category field)          |

**Path migration matrix:**

| Old path               | New path               | Notes                                                |
| ---------------------- | ---------------------- | ---------------------------------------------------- |
| `.ai/agent/threads/**` | `.ai/state/threads/**` | registry.db, transcripts, budgets, thread.json       |
| `.ai/agent/graphs/**`  | `.ai/state/graphs/**`  | graph run transcripts, state                         |
| `.ai/objects/**`       | `.ai/state/objects/**` | entire CAS root moves (blobs, refs, sharded objects) |
| `.ai/cache/**`         | `.ai/state/cache/**`   | tool runtime cache                                   |

**Applies to:** project space AND user space. Both `<project>/.ai/` and `~/.ai/` get the same restructure.

**Files that reference old paths (must update all):**

Engine:

- `ryeos/rye/cas/store.py` — skip list references `.ai/agent/`, CAS root at `.ai/objects/`
- `ryeos/rye/cas/objects.py` — `.ai/agent/graphs/`
- `ryeos/rye/cas/gc.py` — references to objects path
- `ryeos/rye/cas/node_cache.py` — `.ai/objects/cache/nodes`
- `ryeos/rye/utils/async_runner.py` — `.ai/agent/threads/`
- `ryeos/rye/actions/execute.py` — references to agent thread paths

Bundles/standard:

- Thread registry path construction
- Transcript path construction
- Budget ledger path construction

Bundles/core:

- `walker.py` — `.ai/agent/graphs/`, graph transcript paths, JSONL directory, `category = "agent/graphs/..."`
- `walker.py` — `.ai/knowledge/agent/graphs/` (generated knowledge output path)
- Walker knowledge docs — references to `agent/graphs/`, `agent/threads/`
- Capability strings knowledge — `rye/agent/threads/internal/*`
- Remote operations knowledge — `.ai/agent/graphs/`, `.ai/knowledge/agent/graphs/`, `.ai/agent/`

Config:

- `cas/remote.yaml` or equivalent — sync exclusion for `.ai/agent/` → `.ai/state/`
- `remote.py` — sync exclusion list

**Done when:**

- `rg '\.ai/agent/threads'` returns zero results everywhere (engine, bundles, knowledge, docs)
- `rg '\.ai/agent/graphs'` returns zero results (replaced by `.ai/state/graphs/`)
- `rg 'knowledge/agent/graphs'` returns zero results (replaced by `knowledge/state/graphs/`)
- `rg '\.ai/objects/'` returns zero results (replaced by `.ai/state/objects/`)
- `rg '\.ai/cache/'` returns zero results (replaced by `.ai/state/cache/`)
- Runtime outputs materialize to new paths
- CAS manifest/exclusion skips `.ai/state/`
- `rg 'rye/agent/threads'` returns zero results — all renamed to `rye/state/threads`
- `config/agent/` is untouched
- All tests pass

### 4. Restructure `config/`

Reorganize config subdirectories by consumer domain.

**Config migration matrix:**

| Old path                                         | Old content              | New path                                                    | New content                                                    |
| ------------------------------------------------ | ------------------------ | ----------------------------------------------------------- | -------------------------------------------------------------- |
| `config/cas/remote.yaml` → `remotes:` section    | Named remote definitions | `config/remotes/remotes.yaml`                               | `remotes:` with same structure                                 |
| `config/cas/remote.yaml` → sync/exclude sections | Sync policy              | `config/cas/manifest.yaml`                                  | `sync:` or `manifest:`                                         |
| `config/registry/registry.yaml`                  | `registry: url: ...`     | `config/remotes/remotes.yaml`                               | Absorbed as `remotes: registry: {url: ..., roles: [registry]}` |
| `config/node/node.yaml` (if exists)              | Node identity/features   | Split: declarations → `config/node/`, state → `~/.ai/node/` |
| `config/keys/`                                   | Keys                     | `config/keys/`                                              | Unchanged                                                      |
| `config/agent/`                                  | Agent behavior           | `config/agent/`                                             | Unchanged                                                      |
| `config/execution/`                              | Execution policy         | `config/execution/`                                         | Unchanged                                                      |
| `config/web/`                                    | Tool config              | `config/web/`                                               | Unchanged                                                      |

**Loader changes:**

- `ryeos/rye/cas/manifest.py` — `_load_config_3tier("cas/remote.yaml")` → `_load_config_3tier("remotes/remotes.yaml")` for remotes, `_load_config_3tier("cas/manifest.yaml")` for sync policy
- `bundles/core/remote_config.py` — `_CONFIG_REL_PATH = "cas/remote.yaml"` → `"remotes/remotes.yaml"`
- Registry operations — read `roles: [registry]` from remotes instead of separate `registry.yaml`
- No fallback to old paths. Clean break.

**Example `remotes.yaml`:**

```yaml
remotes:
  default:
    url: https://node-1.internal
    key_env: NODE_1_KEY
  gpu-west:
    url: https://gpu-west.internal
    key_env: GPU_WEST_KEY
  registry:
    url: https://ryeos--ryeos-node-node.modal.run
    roles: [registry]
```

**Done when:**

- `rg "cas/remote\.yaml"` returns zero results in code
- `rg "registry/registry\.yaml"` returns zero results in code
- Remote config loads from `remotes/remotes.yaml`
- Manifest/sync policy loads from `cas/manifest.yaml`
- Registry publish/search selects remote by `roles: [registry]`
- No code references old paths — clean break, no fallback
- All tests pass

### 5. Remove `lockfiles/`

CAS content-addresses everything. Tool hashes capture exact content. Lockfiles duplicate what CAS already guarantees.

**Execution lockfiles vs bundle install receipts:** These are different things. Execution lockfiles (`.ai/lockfiles/`) pin tool dependency versions — these are removed. Bundle install receipts (`.ai/bundles/<id>/.bundle-lock.json`) track installed files for uninstall — these stay but are renamed to `install-receipt.json`.

**Files to delete/update:**

- Delete `ryeos/rye/executor/lockfile_resolver.py`
- Remove `LockfileResolver` from `ryeos/rye/executor/__init__.py` exports
- Remove `LockfileResolver` usage from `ryeos/rye/executor/primitive_executor.py` (construction, `get_lockfile`, `create_lockfile`, `save_lockfile`)
- Remove `lockfile_hash` field from `ryeos/rye/cas/objects.py` (`ExecutionSnapshot` dataclass)
- Remove `lockfile_hash` from `ryeos/rye/cas/gc.py`
- Remove `lockfile_hash` from `ryeos/rye/cas/node_cache.py` (`NodeCacheKey`)
- Remove lockfile validation from `ryeos/rye/actions/sign.py` (`LockfileResolver` import and stale lockfile check)
- Update graph walker references — `lockfile_hash=None` already passed, remove the parameter entirely
- Delete `.ai/lockfiles/` directory
- Rename `.bundle-lock.json` → `install-receipt.json` in bundle install/uninstall code

**Cache key impact:** Current graph node cache key includes `lockfile_hash`. After removal, the cache key components are: `graph_hash + node_name + interpolated_action + config_snapshot_hash`. The tool's content hash (already part of `graph_hash` via CAS) provides the reproducibility guarantee that lockfile_hash was providing.

**Done when:**

- `rg lockfile` returns zero results in engine code
- `rg LockfileResolver` returns zero results
- `rg lockfile_hash` returns zero results
- `.ai/lockfiles/` directory does not exist
- `.bundle-lock.json` renamed to `install-receipt.json` in bundle code
- Bundle install/uninstall still works correctly
- Graph node cache invalidation still works correctly
- All tests pass

### 6. Establish `~/.ai/node/`

Define the machine-local node state structure. Node resolution is user-space only — never consults project space.

**Structure:**

```
~/.ai/node/
  identity/            # node Ed25519 keypair (generated on first boot)
  attestation/         # hardware, capabilities, isolation, restrictions
  authorized-keys/     # who can talk to this node
  vault/               # HPKE-encrypted secrets sealed to this node's key
  executions/          # execution records/state
  logs/                # daemon operational logs
```

**Migration from ryeos-node:**

| Old (ryeos-node)                                                          | New                           |
| ------------------------------------------------------------------------- | ----------------------------- |
| `<cas_volume>/signing/`                                                   | `~/.ai/node/identity/`        |
| `<cas_volume>/config/authorized_keys/`                                    | `~/.ai/node/authorized-keys/` |
| Vault storage (endpoint-managed)                                          | `~/.ai/node/vault/`           |
| `executions.py` filesystem storage (`running/`, `executions/by-id/`, log) | `~/.ai/node/executions/`      |
| Operational logs                                                          | `~/.ai/node/logs/`            |

**Enforcement:**

All node path getters MUST read only from `~/.ai/node/`. No generic 3-tier resolver is used for node identity, vault, authorized keys, or execution records. Project space `<project>/.ai/node/` is never consulted for node state.

**Done when:**

- `~/.ai/node/` structure is created on first daemon boot
- Node key generation writes to `~/.ai/node/identity/`
- Authorized key CRUD reads/writes `~/.ai/node/authorized-keys/`
- Vault operations use `~/.ai/node/vault/`
- A fake `<project>/.ai/node/identity/` is ignored — node key is always from `~/.ai/node/`
- All node path getters resolve to `~/.ai/node/` only
- All tests pass

### 7. Update `constants.py`

Reflect the final directory structure in the engine's constants.

**Changes:**

- Remove any lockfile references
- Add `STATE_DIR = "state"` constant
- Add subdirectory constants: `STATE_THREADS = "threads"`, `STATE_GRAPHS = "graphs"`, `STATE_OBJECTS = "objects"`, `STATE_CACHE = "cache"`
- Add `NODE_DIR = "node"` constant for `~/.ai/node/`
- Verify `TYPE_DIRS` still correct (directives, tools, knowledge — unchanged)
- Verify `SIGNABLE_DIRS` still correct (includes config — unchanged)
- Update CAS skip list constants if they reference old dir names

**Done when:**

- All directory path construction uses constants, not hardcoded strings
- `rg '"agent"'` in path construction code returns zero results
- `rg '"objects"'` in path construction code uses the new constant
- All tests pass

### 8. Update all knowledge and docs

Comprehensive pass over agent-facing and user-facing documentation.

**Scope:**

- System knowledge referencing old paths (`agent/threads`, `agent/graphs`, `cas/remote.yaml`, lockfiles, HTTP primitive)
- Bundle knowledge (walker knowledge, capability strings, remote operations, CAS object kinds)
- AGENTS.md / agent instructions
- Tool creation directives (if they reference HTTP primitive as chain target)
- Config schema descriptions and `target_config` paths
- Three-tier-spaces knowledge (references `.bundle-lock.json`)

**Done when:**

- `rg '\.ai/agent/' docs/ ryeos/bundles/*/ryeos_*/.ai/knowledge/` returns zero results
- `rg 'cas/remote\.yaml' docs/ ryeos/bundles/*/ryeos_*/.ai/knowledge/` returns zero results
- `rg 'lockfile' docs/ ryeos/bundles/*/ryeos_*/.ai/knowledge/` returns zero results
- `rg 'HttpClientPrimitive\|http_client primitive' docs/ ryeos/bundles/*/ryeos_*/.ai/knowledge/` returns zero results
- All knowledge docs reflect the new directory structure

---

## Dependency Order

```
1 (contract doc)              — this document, governs everything
    ↓
2 (remove http primitive)     — independent, engine-only
3 (agent/ → state/)           — largest change, touches everywhere
5 (remove lockfiles/)         — independent, small
    ↓
4 (restructure config/)       — after 3 settles (config/agent/ stays)
6 (establish ~/.ai/node/)     — after 4 (config/node/ defined)
7 (update constants.py)       — after 3 + 4 + 5 settle names
    ↓
8 (update knowledge/docs)     — last, after all structural changes land
```

Items 2, 3, and 5 can parallelize. Item 3 is the critical path.

---

## Invariants

These MUST hold throughout the entire restructure:

1. **Rename `rye/agent/threads/*` item IDs alongside filesystem paths.** `rye/agent/threads/*` → `rye/state/threads/*`. No legacy references. Clean break. All other `rye/agent/*` namespaces are out of scope.
2. **MUST NOT rename `config/agent/`.** Agent behavior config is not part of this restructure.
3. **MUST NOT allow `daemon.yaml` in project space.** Bind/listen config is machine-local only.
4. **MUST NOT delete `.bundle-lock.json` without replacement.** Rename to `install-receipt.json`.
5. **MUST NOT use generic 3-tier resolver for node state.** Node path getters read `~/.ai/node/` only.
6. **No backwards compatibility.** No fallback to old paths, no deprecation warnings, no legacy references.

---

## What This Enables

After these prerequisites, ryeosd phases begin on clean ground:

- **Phase 1** (scaffold) has a clear directory model — `state/threads/` for registry, `~/.ai/node/` for node state
- **Custom routes** have a home — `config/node/routes/` declarations, node loads at startup
- **Node sandboxed execution** has a home — `~/.ai/node/attestation/` for hardware/isolation declarations
- **One execution primitive** — every chain terminates at Execute → Lillux, no special cases
- **ryeosd phases plan** will need updating to reflect these structural changes, but the architecture is stable

## Relationship to Future Docs

| Doc                                                        | Relationship                                                          |
| ---------------------------------------------------------- | --------------------------------------------------------------------- |
| [ryeosd architecture](../../.tmp/ryeosd/architecture.md)   | Phases depend on this restructure completing first                    |
| [node-sandboxed-execution.md](node-sandboxed-execution.md) | `~/.ai/node/attestation/` is where node environment declarations live |
| [node-custom-routes.md](../../.tmp/node-custom-routes.md)  | Routes are `config/node/routes/` declarations, not a new item type    |
| [remove-http-primitive.md](remove-http-primitive.md)       | Prerequisite 2 — implements this doc                                  |
