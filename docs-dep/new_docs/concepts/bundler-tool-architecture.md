# Bundler Tool Architecture

How bundles are created, verified, and distributed as a core tool — without adding a 4th item type to the MCP interface.

## Design Decision

Bundles are **not** a new `ItemType`. The MCP interface stays at 3 item types (`directive`, `tool`, `knowledge`) and 4 tools (`search`, `load`, `execute`, `sign`). A bundle is an operational concept — a group of items managed by a core tool — not a fundamental data type in the item model.

The bundler lives at `.ai/tools/rye/core/bundler/bundler.py` and follows the same action-dispatch pattern as [registry.py](../../rye/rye/.ai/tools/rye/registry/registry.py) and [system.py](../../rye/rye/.ai/tools/rye/core/system/system.py). It is executed via the standard path:

```
rye_execute item_type=tool item_id=rye/core/bundler/bundler action=create ...
```

### Why Not a 4th Item Type

Adding `bundle` to `ItemType.ALL` would require changes to:

- `constants.py` — new enum value + `TYPE_DIRS` mapping
- `server.py` — expanded enums on all 4 MCP tool schemas
- `search.py` — new search path resolution, new extractor
- `load.py` — new load semantics (manifest vs content)
- `sign.py` — new signing semantics (manifest generation vs inline signature)
- `execute.py` — new execution semantics (resolve entrypoint → delegate to directive)
- Every extractor — new data-driven configuration

That's 7+ files changed for what is fundamentally a grouping/packaging concern. A bundle doesn't have its own execution semantics — you execute its entrypoint directive. A bundle doesn't have its own search semantics — you search for directives/tools/knowledge and discover they belong to a bundle. A bundle doesn't have its own signing semantics — inline items get inline signatures, and the manifest gets its own signature via the bundler tool.

The tool-based approach requires 1 new file (`bundler.py`) and minor additions to `registry.py` for transport.

## Responsibility Split

| Concern                                                         | Owner                                        | Actions                               |
| --------------------------------------------------------------- | -------------------------------------------- | ------------------------------------- |
| **Bundle semantics** (create, verify, inspect, list)            | Bundler tool (`rye/core/bundler/bundler.py`) | `create`, `verify`, `inspect`, `list` |
| **Bundle transport** (push to registry, pull from registry)     | Registry tool (`rye/registry/registry.py`)   | `push_bundle`, `pull_bundle`          |
| **Item signing** (inline Ed25519 on directives/tools/knowledge) | Sign MCP tool (`rye/rye/tools/sign.py`)      | Existing batch sign with glob         |
| **Item discovery** (search/load individual items)               | Search/Load MCP tools                        | Unchanged                             |

The bundler tool owns **manifest lifecycle**. The registry tool owns **network transport**. The MCP tools own **individual item operations**. No overlap.

## Bundle Layout on Disk

A bundle is defined by a `manifest.yaml` file under `.ai/bundles/`:

```
.ai/
├── bundles/
│   └── apps/task-manager/
│       └── manifest.yaml              ← signed manifest (bundler creates this)
│
├── directives/
│   └── apps/task-manager/
│       ├── build_crud_app.md          ← inline signed
│       ├── scaffold_project.md        ← inline signed
│       └── implement_feature.md       ← inline signed
│
├── tools/
│   └── apps/task-manager/
│       ├── dev_server.py              ← inline signed
│       ├── test_runner.py             ← inline signed
│       └── build.py                   ← inline signed
│
├── knowledge/
│   └── apps/task-manager/
│       ├── react-patterns.md          ← inline signed
│       └── api-design.md             ← inline signed
│
├── plans/
│   └── task-manager/
│       └── phase_1/
│           ├── plan_db_schema.md
│           └── plan_api_routes.md
│
└── lockfiles/
    └── apps_task-manager_build_crud_app.lock.yaml
```

The manifest **references** files in existing item directories by relative path from `.ai/`. It does not duplicate them. The canonical path convention:

```
.ai/bundles/{bundle_id}/manifest.yaml
```

Where `bundle_id` matches the category prefix used across item directories (e.g., `apps/task-manager`).

### What Gets Bundled

The bundler walks these prefixes under `.ai/` for a given `bundle_id`:

| Prefix                                   | Item Type | Inline Signed |
| ---------------------------------------- | --------- | ------------- |
| `directives/{bundle_id}/**`              | directive | Yes           |
| `tools/{bundle_id}/**`                   | tool      | Yes           |
| `knowledge/{bundle_id}/**`               | knowledge | Yes           |
| `plans/{bundle_id_without_namespace}/**` | asset     | No            |
| `lockfiles/{bundle_id_slug}_*`           | asset     | No            |

### What Does NOT Get Bundled

| Path               | Reason                                           |
| ------------------ | ------------------------------------------------ |
| `.ai/threads/`     | Runtime state — not reproducible                 |
| `**/node_modules/` | NPM dependencies — reproducible via package.json |
| `.ai/bundles/`     | Meta — manifests are regenerated on push         |
| `**/__pycache__/`  | Build artifacts                                  |

## Manifest Schema

The manifest is a YAML file with an Ed25519 inline signature on line 1:

```yaml
# rye:signed:2026-02-11T00:00:00Z:MANIFEST_HASH:ED25519_SIG:PUBKEY_FP
bundle:
  id: apps/task-manager
  version: 1.0.0
  created: 2026-02-11T00:00:00Z
  entrypoint:
    item_type: directive
    item_id: apps/task-manager/build_crud_app
  description: CRUD task manager with React + Express + SQLite

files:
  # Directives (inline signed + manifest hash)
  directives/apps/task-manager/build_crud_app.md:
    sha256: a1b2c3d4...
    inline_signed: true
  directives/apps/task-manager/scaffold_project.md:
    sha256: e5f6a7b8...
    inline_signed: true

  # Tools (inline signed + manifest hash)
  tools/apps/task-manager/dev_server.py:
    sha256: 1a2b3c4d...
    inline_signed: true

  # Knowledge (inline signed + manifest hash)
  knowledge/apps/task-manager/react-patterns.md:
    sha256: 3a4b5c6d...
    inline_signed: true

  # Plans (manifest hash only — no inline signatures)
  plans/task-manager/phase_1/plan_db_schema.md:
    sha256: 7e8f9a0b...

  # Lockfiles (manifest hash only)
  lockfiles/apps_task-manager_build_crud_app.lock.yaml:
    sha256: 9c0d1e2f...
```

The `entrypoint` field tells consumers which directive to execute when "running" the bundle. The bundler tool does not execute anything — it just records the entrypoint for documentation and tooling.

## Bundler Tool Interface

```python
# .ai/tools/rye/core/bundler/bundler.py

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/core/bundler"
__tool_description__ = "Create, verify, and inspect bundle manifests"

ACTIONS = ["create", "verify", "inspect", "list"]
```

### `action=create` — Generate and sign a bundle manifest

```python
async def _create(
    bundle_id: str,          # e.g., "apps/task-manager"
    version: str,            # semver, e.g., "1.0.0"
    entrypoint: str = None,  # directive item_id, e.g., "apps/task-manager/build_crud_app"
                             # Stored in manifest as {item_type: "directive", item_id: entrypoint}
    description: str = "",
    project_path: str = None,
) -> Dict[str, Any]:
    """Walk .ai/ for files matching bundle_id prefix, compute SHA256s,
    generate manifest YAML, sign it with local Ed25519 key.

    Writes manifest to .ai/bundles/{bundle_id}/manifest.yaml.

    Returns:
        {status, manifest_path, file_count, files_by_type: {directive: N, tool: N, ...}}
    """
```

Flow:

1. Walk `.ai/directives/{bundle_id}/`, `.ai/tools/{bundle_id}/`, `.ai/knowledge/{bundle_id}/`, `.ai/plans/...`, `.ai/lockfiles/...`
2. For each file: compute SHA256, check if it has an inline `rye:signed:` signature
3. Generate manifest YAML with `bundle` header + `files` dict
4. Sign the manifest with local Ed25519 key (same signing system as `rye_sign`)
5. Write to `.ai/bundles/{bundle_id}/manifest.yaml`

### `action=verify` — Verify a bundle manifest against disk

```python
async def _verify(
    bundle_id: str,
    project_path: str = None,
) -> Dict[str, Any]:
    """Load manifest, verify its signature, check SHA256 of every
    referenced file against disk.

    Returns:
        {status, manifest_valid: bool, files_checked: N, files_ok: N,
         files_missing: [...], files_tampered: [...]}
    """
```

Flow:

1. Load `.ai/bundles/{bundle_id}/manifest.yaml`
2. Verify manifest's own Ed25519 inline signature
3. For each file in manifest: compute SHA256, compare to manifest entry
4. For files with `inline_signed: true`: also verify inline signature via `verify_item()`
5. Report results

Verification is **eager** when called directly (check all files). At runtime, verification is **lazy** — the `verify_bundle_manifest()` function called by tools checks files only when accessed (see [app-bundling-and-orchestration.md](app-bundling-and-orchestration.md#manifest-generation-and-verification)).

### `action=inspect` — Return manifest metadata without verification

```python
async def _inspect(
    bundle_id: str,
    project_path: str = None,
) -> Dict[str, Any]:
    """Parse manifest and return metadata + file inventory.

    Returns:
        {bundle: {id, version, entrypoint, description},
         files: [{path, sha256, inline_signed, type}, ...],
         file_count: N, files_by_type: {...}}
    """
```

### `action=list` — Find all local bundles

```python
async def _list(
    project_path: str = None,
) -> Dict[str, Any]:
    """Find all manifest.yaml files under .ai/bundles/.

    Returns:
        {bundles: [{bundle_id, version, entrypoint, description, manifest_path}, ...]}
    """
```

## Registry Integration

Bundle transport (push/pull) stays in `registry.py` as new actions. The registry tool calls bundler logic internally — it imports manifest creation/verification functions from the bundler's library code.

### Push Flow

```
1. User signs individual items:
   rye_sign item_type=directive item_id=apps/task-manager/*
   rye_sign item_type=tool item_id=apps/task-manager/*
   rye_sign item_type=knowledge item_id=apps/task-manager/*

2. User creates bundle manifest:
   rye_execute item_type=tool item_id=rye/core/bundler/bundler
     action=create bundle_id=apps/task-manager version=1.0.0
     entrypoint=apps/task-manager/build_crud_app

3. User pushes bundle to registry:
   rye_execute item_type=tool item_id=rye/registry/registry
     action=push_bundle bundle_id=apps/task-manager version=1.0.0

4. Registry server:
   a. Verifies manifest signature
   b. Verifies each item's content_hash matches sha256(content)
   c. For items with inline signatures, verifies those too
   d. Re-signs manifest with registry provenance (|registry@username)
   e. Stores bundle metadata + items
```

### Pull Flow

```
1. User pulls bundle:
   rye_execute item_type=tool item_id=rye/registry/registry
     action=pull_bundle bundle_id=leolilley/apps/task-manager version=1.0.0

2. Registry client:
   a. Downloads manifest + all items
   b. Verifies manifest signature (including registry provenance)
   c. Verifies content hashes
   d. Writes files to .ai/ preserving directory structure
   e. Writes manifest to .ai/bundles/{bundle_id}/manifest.yaml

3. User runs the app:
   rye_execute item_type=directive item_id=apps/task-manager/build_crud_app
```

### Registry Actions (in `registry.py`)

Only transport actions belong in the registry tool:

| Action        | Purpose                             |
| ------------- | ----------------------------------- |
| `push_bundle` | Upload manifest + items to registry |
| `pull_bundle` | Download + verify + extract bundle  |

Discovery of remote bundles uses the existing `search` action with appropriate query parameters — bundles on the server side are searchable entities in their own tables.

## Relationship to Existing Signing

The two-layer signing model from [app-bundling-and-orchestration.md](app-bundling-and-orchestration.md#two-layer-signing-model) stays exactly the same:

| Layer                          | Mechanism                           | Who Creates It                     |
| ------------------------------ | ----------------------------------- | ---------------------------------- |
| **Layer 1: Inline signatures** | `rye:signed:` comment in code files | `rye_sign` MCP tool (existing)     |
| **Layer 2: Bundle manifest**   | `manifest.yaml` with SHA256 hashes  | Bundler tool `action=create` (new) |

The bundler tool creates Layer 2. The sign MCP tool creates Layer 1. They're independent — you can sign items without a bundle, and you can create a manifest referencing unsigned items (the manifest records `inline_signed: false` for those).

At runtime, the `verify_bundle_manifest()` function (used by tools that load bundle files) checks whichever layer applies:

- Files with inline signatures: `verify_item()` (Layer 1) + manifest hash check (Layer 2)
- Files without inline signatures (assets, plans, lockfiles): manifest hash check only (Layer 2)

## Supabase Schema Notes

The existing Supabase tables (`ratings`, `reports`, `favorites`) accept `item_type = 'bundle'`. This is a **registry-level entity type**, not a core `ItemType`. The distinction:

- **Core `ItemType`** (`directive`, `tool`, `knowledge`): used by MCP search/load/sign/execute, resolved via 3-tier space system, has extractors
- **Registry entity type** (`directive`, `tool`, `knowledge`, `bundle`): used by the registry database for ratings, favorites, visibility, versioning

The registry database can have its own notion of "bundle" as a thing you can rate/favorite/search — that doesn't require the MCP interface to know about bundles.

## Canonical Vocabulary Addition

Add to the [Canonical Vocabulary](thread-orchestration-internals.md#canonical-vocabulary) Tool Names table:

| Tool      | Purpose                                                        |
| --------- | -------------------------------------------------------------- |
| `bundler` | Create, verify, inspect bundle manifests; manage bundle layout |

## Cross-Document Alignment

All design docs and the implementation plan have been updated to reflect the bundler-as-tool architecture:

- **[app-bundling-and-orchestration.md](app-bundling-and-orchestration.md)** — manifest schema uses `inline_signed` field name, manifest path is `.ai/bundles/{bundle_id}/manifest.yaml`, sharing flow uses `bundler action=create` → `registry action=push_bundle`, bundle structure includes `bundles/` directory
- **[thread-orchestration-internals.md](thread-orchestration-internals.md)** — Canonical Vocabulary includes `bundler` tool, State File Locations includes manifest path
- **[IMPLEMENTATION_PLAN.md](../IMPLEMENTATION_PLAN.md)** — Phase C2 creates bundler core tool (not `create_manifest.py`), Phase D2 has only transport actions in registry (`push_bundle`, `pull_bundle`), Phase D3 references bundler library functions, Supabase `item_type='bundle'` clarified as registry entity type
