# Package Architecture

RYE's multi-package, data-driven bundle system.

## 1. Overview

RYE treats tools, directives, and knowledge as **data files** living in `.ai/` directories. Items resolve across three tiers — **project → user → system** — so project-local definitions override user defaults, which override system-bundled items.

The system tier uses **bundles** to control what `.ai/` items are exposed. Bundles are registered via Python entry points under the `rye.bundles` group. Two concepts are central:

- **Packages** = code separation (PyPI dependency management)
- **Bundles** = data separation (which `.ai/` items are exposed to the resolver)

A single package can ship all the data while multiple bundles expose different subsets of it.

## 2. Packages vs Bundles

Packages and bundles serve different purposes and don't map 1:1:

| Concept | Purpose | Example |
|---------|---------|---------|
| Package | Code separation, dependency management | `rye-core` has zero MCP deps |
| Bundle  | Data exposure, controls what items are visible | `rye/core` exposes only `rye/core/*` items |

**Key insight:** `rye-core` ships ALL `.ai/` data (every category under `rye/*`), but the `rye/core` bundle only exposes the `rye/core/` subset. The `rye-os` bundle (registered by `rye-mcp`) exposes everything under `rye/*` from that same data.

## 3. Package Hierarchy

| Directory  | PyPI Name  | Description | Ships `.ai/` data? |
|------------|------------|-------------|---------------------|
| `lilux/`   | `lilux`    | Microkernel — execution primitives, env resolver, signing, schemas. Zero dependencies. | No |
| `rye/`     | `rye-core`  | Core engine — resolvers, executor, handlers. Ships ALL `.ai/` data. Registers `rye/core` bundle. | **Yes** — all `{directives,tools,knowledge}/rye/*` |
| `rye-mcp/` | `rye-mcp`  | MCP server — transport layer exposing 4 primary tools. No `.ai/` data. Registers `rye-os` bundle. | No — references `rye-core`'s data via `importlib.util.find_spec` |

## 4. Bundle Discovery

### Entry Points

Bundles register via the `rye.bundles` entry point group:

```toml
# rye/pyproject.toml
[project.entry-points."rye.bundles"]
rye-core = "rye.bundle_entrypoints:get_rye_core_bundle"

# rye-mcp/pyproject.toml
[project.entry-points."rye.bundles"]
rye-os = "rye_mcp.bundle_entrypoints:get_rye_os_bundle"
```

Entry point functions return a dict:

```python
# rye/rye/bundle_entrypoints.py
def get_rye_core_bundle() -> dict:
    return {
        "bundle_id": "rye/core",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["rye/core"],
    }

# rye-mcp/rye_mcp/bundle_entrypoints.py
def get_rye_os_bundle() -> dict:
    return {
        "bundle_id": "rye-os",
        "version": "0.1.0",
        "root_path": _package_root("rye"),  # finds rye-core's data
        "categories": ["rye"],
    }
```

### BundleInfo

On startup, `get_system_spaces()` in `rye/rye/utils/path_utils.py` discovers all bundles and parses them into `BundleInfo` dataclass instances:

```python
@dataclass(frozen=True)
class BundleInfo:
    bundle_id: str
    version: str
    root_path: Path
    manifest_path: Optional[Path]
    source: str
    categories: Optional[List[str]] = None
```

Key behaviors:
- Results are **cached at module level** after first computation
- Entry points are sorted **alphabetically by entry point name**
- `manifest_path` is auto-discovered at `.ai/bundles/{bundle_id}/manifest.yaml` if not explicitly set

### Categories

Categories control which subdirectories under `.ai/{type}/` a bundle exposes:

```python
def get_type_paths(self, item_type: str) -> List[Path]:
    base = self.root_path / ".ai" / folder_name
    if self.categories:
        return [base / cat for cat in self.categories]
    return [base]
```

| Bundle | `categories` | Tools path resolves to |
|--------|-------------|----------------------|
| `rye/core` | `["rye/core"]` | `.ai/tools/rye/core/` |
| `rye-os` | `["rye"]` | `.ai/tools/rye/` |

When `categories` is `None`, the full type directory is used (e.g., `.ai/tools/`).

## 5. Resolution Order

When resolving an item, first match wins:

1. **Project** — `.ai/{type}/` in the project root
2. **User** — `~/.ai/{type}/`
3. **System bundles** — alphabetically by entry point name:
   - `rye-core` → scoped to `rye/core/*`
   - `rye-os` → all `rye/*`

Each system bundle is labeled `"system:{bundle_id}"` for provenance in search results.

> **Note:** If only `rye-core` is installed (no `rye-mcp`), only `rye/core/*` items are visible. Installing `rye-mcp` adds the `rye-os` bundle which exposes all categories.

## 6. What Ships Where

All `.ai/` data lives in `rye/rye/.ai/`. The two bundles expose different subsets:

### `rye/core` bundle (registered by `rye-core`)

Exposes only `rye/core/*`:

```
rye/rye/.ai/
├── directives/rye/core/
│   ├── create_directive.md
│   ├── create_knowledge.md
│   ├── create_threaded_directive.md
│   └── create_tool.md
├── knowledge/rye/core/
│   ├── directive-metadata-reference.md
│   ├── knowledge-metadata-reference.md
│   └── tool-metadata-reference.md
└── tools/rye/core/
    ├── bundler/
    ├── extractors/      (directive/, tool/, knowledge/)
    ├── parsers/
    ├── primitives/
    ├── runtimes/
    ├── sinks/
    ├── system/
    └── telemetry/
```

### `rye-os` bundle (registered by `rye-mcp`)

Exposes ALL `rye/*` — everything above plus:

```
rye/rye/.ai/tools/rye/
├── agent/
│   ├── permissions/
│   ├── providers/
│   └── threads/
├── fs/               (fs_read, fs_write)
├── mcp/              (connect, discover, manager)
├── primary/          (rye_execute, rye_load, rye_search, rye_sign)
└── registry/         (registry)
```

> The `rye-mcp` package ships **no `.ai/` data** of its own. It points `root_path` at `rye-core`'s package directory using `importlib.util.find_spec("rye")`.

## 7. Creating an Addon Package

**Step 1: Create the package structure**

```
my-addon/
├── my_addon/
│   ├── __init__.py
│   ├── bundle_entrypoints.py
│   └── .ai/
│       ├── tools/my_addon/
│       │   └── some_tool.yaml
│       ├── directives/my_addon/
│       │   └── some_directive.md
│       └── knowledge/my_addon/
│           └── some_reference.md
└── pyproject.toml
```

**Step 2: Create `bundle_entrypoints.py`**

```python
from pathlib import Path

def get_my_addon_bundle() -> dict:
    return {
        "bundle_id": "my-addon",
        "version": "0.1.0",
        "root_path": Path(__file__).parent,
        "categories": ["my_addon"],
    }
```

**Step 3: Register the entry point**

```toml
[project.entry-points."rye.bundles"]
my-addon = "my_addon.bundle_entrypoints:get_my_addon_bundle"
```

**Step 4: Include `.ai/` in the wheel**

```toml
[build-system]
requires = ["hatchling"]
build-backend = "hatchling.build"

[tool.hatch.build.targets.wheel]
packages = ["my_addon"]

[tool.hatch.build.targets.wheel.force-include]
"my_addon/.ai" = "my_addon/.ai"
```

**Step 5: Install**

```bash
pip install -e ./my-addon
```

Items are automatically discoverable — no configuration beyond the entry point.
