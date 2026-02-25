<!-- rye:signed:2026-02-23T05:24:41Z:1bfba037d142f4b65e1b634754a7542b88837171d0c615ac0eceb9ab7f6ee5b1:MKvnuBI36oxXOMYuVMoHDwv2JGURCm9W-Rbm9Gi7Dvd5P6UzenFzNsOnLXwrGILbnqPfXWMH7RQEVjxVqc2WDg==:9fbfabe975fa5a7f -->

```yaml
name: bundle-format
title: Bundle Format & Distribution
entry_type: reference
category: rye/core/bundler
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - bundles
  - packages
  - distribution
  - entry-points
references:
  - "docs/internals/packages-and-bundles.md"
```

# Bundle Format & Distribution

How Rye OS packages register and load `.ai/` item bundles into the system space.

## Package Hierarchy

```
┌───────────────────────────────────────────────────┐
│  ryeos-mcp         (MCP transport)                │
│  deps: ryeos, mcp                                 │
│  bundle: none (inherits from ryeos)               │
├───────────────────────────────────────────────────┤
│  ryeos             (standard bundle)              │
│  deps: lilux, pyyaml, cryptography, packaging     │
│  bundle: ryeos → all rye/* items                  │
├───────────────────────────────────────────────────┤
│  ryeos-core        (minimal install)              │
│  deps: lilux, pyyaml, cryptography, packaging     │
│  bundle: ryeos-core → rye/core/* items only       │
├───────────────────────────────────────────────────┤
│  ryeos-bare        (engine only)                  │
│  deps: lilux                                      │
│  bundle: none (engine only, no .ai/ items)        │
├───────────────────────────────────────────────────┤
│  ryeos-web         (opt-in web bundle)            │
│  deps: ryeos                                      │
│  bundle: ryeos-web → rye/web/* items              │
├───────────────────────────────────────────────────┤
│  ryeos-code        (opt-in code bundle)           │
│  deps: ryeos                                      │
│  bundle: ryeos-code → rye/code/* items            │
├───────────────────────────────────────────────────┤
│  lilux             (stateless microkernel)        │
│  deps: cryptography, httpx, lilux-proc            │
│  bundle: none (pure library, no .ai/ items)       │
└───────────────────────────────────────────────────┘
```

**Mutual exclusion:** `ryeos-core` and `ryeos` both install the `rye` Python module. Install one or the other, never both.

**lilux-proc dependency:** lilux depends on `lilux-proc` (hard dep, no fallbacks). The Rust binaries `lilux-proc` and `lilux-watch` live in `lilux/proc/` and `lilux/watch/` at the monorepo top level.

**node_modules not shipped:** Web and code bundles do not ship `node_modules`. Dependencies are installed on first use via the anchor system.

## What Each Install Provides

```
pip install ryeos-core      → system space: rye/core/* only
pip install ryeos            → system space: rye/* (standard items)
pip install ryeos-mcp        → system space: rye/* (via ryeos dep)
pip install ryeos-bare       → engine only (no .ai/ items)
pip install ryeos-web        → system space: rye/web/* (opt-in)
pip install ryeos-code       → system space: rye/code/* (opt-in)
pip install ryeos my-tools   → system space: rye/* + my-tools/*
```

Web tools (`rye/web/*`) are in `ryeos/bundles/web/`, code tools (`rye/code/*`) are in `ryeos/bundles/code/`.

## Entry Point Registration

Bundles are registered in `pyproject.toml` under the `rye.bundles` entry point group:

```toml
# ryeos — standard bundle
[project.entry-points."rye.bundles"]
ryeos = "rye.bundle_entrypoints:get_ryeos_bundle"

# ryeos-core — core-only bundle
[project.entry-points."rye.bundles"]
ryeos-core = "rye.bundle_entrypoints:get_ryeos_core_bundle"
```

Both entry point functions live in `rye/bundle_entrypoints.py`.

## bundle_info Dict Format

Each entry point function returns a `dict` with this shape:

| Key          | Type        | Description                                          | Example               |
| ------------ | ----------- | ---------------------------------------------------- | --------------------- |
| `bundle_id`  | `str`       | Unique bundle identifier                             | `"rye-os"`, `"rye/core"` |
| `root_path`  | `Path`      | Absolute path to the `rye/` Python module directory  | `Path(__file__).parent` |
| `version`    | `str`       | Optional semver version                              | `"1.0.0"`             |
| `categories` | `list[str]` | Category prefixes this bundle exposes                | `["rye"]`, `["rye/core"]` |
### Concrete Implementations

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

## Category Scoping

Categories control which `.ai/` items are visible from a bundle:

| Bundle       | `categories`  | Visible Items                                                       |
| ------------ | ------------- | ------------------------------------------------------------------- |
| `ryeos`      | `["rye"]`     | Standard items: `rye/core/*`, `rye/agent/*`, etc.                   |
| `ryeos-core` | `["rye/core"]`| Only core: `rye/core/runtimes/*`, `rye/core/registry/*`, etc.       |
| `ryeos-web`  | `["rye/web"]` | Web tools: `rye/web/*`                                              |
| `ryeos-code` | `["rye/code"]`| Code tools: `rye/code/*`                                            |

The resolver uses prefix matching — an item with category `rye/core/registry` is included by both `["rye"]` and `["rye/core"]`.

## Author Key Trust

Every bundle ships the author's Ed25519 public key as a TOML identity document at `.ai/trusted_keys/{fingerprint}.toml`. All items in the bundle are signed with this key. The Rye system bundle is signed by Leo Lilley — the same key used for registry publishing.

The trust model has **no exceptions**: system items go through the same signature verification as project and user items. The trust store uses standard 3-tier resolution (project → user → system), so the author's key in the system bundle is discovered automatically — no special bootstrap logic required.

Third-party bundles follow the same pattern: ship a `.toml` identity document in `.ai/trusted_keys/`, and the key is resolved via 3-tier lookup. Users trust the bundle author, not the package.

## How get_system_spaces() Loads Bundles

The resolver discovers bundles at startup via Python entry points:

1. **Discover** — `importlib.metadata.entry_points(group="rye.bundles")` enumerates all registered bundles
2. **Load** — Each entry point is called to get its `bundle_info` dict
3. **Compose** — Multiple bundles compose into the system space; a third-party package can register its own `rye.bundles` entry point
4. **Filter** — Items are filtered by the bundle's `categories` list using prefix matching

## Bundle Manifest Structure

Bundle manifests are signed YAML files:

```yaml
# rye:signed:TIMESTAMP:HASH:SIG:FP
bundle:
  id: rye-core
  version: 1.0.0
  entrypoint: rye/core/create_directive
  description: Core Rye OS bundle
files:
  .ai/tools/rye/core/registry/registry.py:
    sha256: a66665d3ef686944...
    inline_signed: true
    item_type: tool
  .ai/directives/rye/core/create_directive.md:
    sha256: 7c8a91b2f3d40e...
    inline_signed: true
    item_type: directive
```

### Manifest Fields

| Field                      | Description                                         |
| -------------------------- | --------------------------------------------------- |
| `bundle.id`                | Bundle identifier                                   |
| `bundle.version`           | Semver version string                               |
| `bundle.entrypoint`        | Default item to run                                 |
| `bundle.description`       | Human-readable description                          |
| `files.<path>.sha256`      | SHA256 hex digest of the file's content             |
| `files.<path>.inline_signed` | Whether the file has its own Ed25519 signature    |
| `files.<path>.item_type`   | Item type (`tool`, `directive`, `knowledge`)         |

## Bundle Verification

`validate_bundle_manifest()` performs layered verification:

| Layer                  | Check                                                              |
| ---------------------- | ------------------------------------------------------------------ |
| **Manifest signature** | `verify_item(manifest_path, ItemType.TOOL)` — Ed25519 on manifest |
| **Per-file SHA256**    | Compute `SHA256(file)` and compare to manifest's recorded hash     |
| **Inline signatures**  | If `inline_signed: true`, also `verify_item()` on that file       |
| **Missing files**      | Files in manifest but not on disk are flagged                      |

Verification report format:

```json
{
  "status": "verified",
  "manifest_valid": true,
  "files_checked": 42,
  "files_ok": 42,
  "files_missing": [],
  "files_tampered": []
}
```

Non-signable assets (images, data files) are covered by manifest per-file SHA256. Signable items (`.py`, `.md`, `.yaml`) get dual protection: manifest hash + inline Ed25519.

## Bundled Tools vs Package Dependencies

| Import Location                              | Resolution                                                      |
| -------------------------------------------- | --------------------------------------------------------------- |
| Core package code (`rye/rye/*.py`)           | Standard pip dependency — must be in `pyproject.toml`           |
| Bundled tools (`rye/rye/.ai/tools/**/*.py`)  | Resolved at runtime by executor chain                           |
| Tools with `DEPENDENCIES = [...]`            | Installed on-demand by `EnvManager` into tool's venv            |
| Lazy imports inside functions                | Available if transitive dep provides it — prefer `DEPENDENCIES` |

## Third-Party Bundle Registration

Any pip package can contribute items to the system space by registering a `rye.bundles` entry point:

```toml
[project.entry-points."rye.bundles"]
my-tools = "my_package.bundles:get_bundle"
```

```python
def get_bundle() -> dict:
    return {
        "bundle_id": "my-tools",
        "root_path": Path(__file__).parent,
        "categories": ["my-tools"],
    }
```

The resolver will discover and compose this bundle alongside any Rye bundles.
