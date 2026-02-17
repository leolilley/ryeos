---
id: three-tier-spaces
title: "Three-Tier Space System"
description: How items are resolved across project, user, and system spaces
category: internals
tags: [spaces, resolution, project, user, system, bundles]
version: "1.0.0"
---

# Three-Tier Space System

Every item in Rye OS (directives, tools, knowledge) lives in one of three spaces. When resolving an item by ID, spaces are checked in precedence order — the first match wins.

## The Three Spaces

| Space | Path | Precedence | Mutability |
|-------|------|------------|------------|
| **Project** | `{project}/.ai/` | 3 (highest) | Read-write |
| **User** | `{$USER_SPACE or ~}/.ai/` | 2 | Read-write |
| **System** | `site-packages/rye/.ai/` | 1 (lowest) | Immutable (ships with package) |

### Project Space

The `.ai/` directory in the current project root. This is where project-specific tools, directives, and knowledge live.

```
my-project/
  .ai/
    tools/
      my-custom-tool.py
    directives/
      my-workflow.md
    knowledge/
      project-patterns.md
```

Project space has the highest precedence — a project tool with the same ID as a system tool will shadow it.

### User Space

Cross-project items stored in the user's home directory (or a custom path).

```
~/.ai/
  tools/
    my-global-tool.py
  directives/
    my-global-workflow.md
```

The `USER_SPACE` environment variable controls the **base path** (not the `.ai/` folder itself). When set, user space is `$USER_SPACE/.ai/`. When unset, it defaults to `~/.ai/`.

```bash
# Default: ~/.ai/
export USER_SPACE=/custom/home   # → /custom/home/.ai/
```

### System Space

The immutable "standard library" that ships inside the `rye` Python package. Located at `site-packages/rye/.ai/`. Contains the core runtimes, built-in tools, directives, and knowledge entries.

System space items are never modified at runtime. To override a system tool, place a file with the same item ID in project or user space.

## Resolver Classes

Three resolver classes in `rye/utils/resolvers.py` implement the same pattern for each item type:

### DirectiveResolver

```python
resolver = DirectiveResolver(project_path=Path("/my/project"))

# Find directive by ID (first match wins)
path = resolver.resolve("core/build")
# → Checks: {project}/.ai/directives/core/build.md
# → Then:   ~/.ai/directives/core/build.md
# → Then:   site-packages/rye/.ai/directives/core/build.md

# Get path with space label
path, space = resolver.resolve_with_space("core/build")
# → (Path(".ai/directives/core/build.md"), "project")

# List all search paths
paths = resolver.get_search_paths()
# → [(Path("{project}/.ai/directives"), "project"),
#     (Path("~/.ai/directives"), "user"),
#     (Path("site-packages/rye/.ai/directives"), "system")]
```

Directives always have the `.md` extension.

### ToolResolver

```python
resolver = ToolResolver(project_path=Path("/my/project"))

path = resolver.resolve("rye/core/registry/registry")
# → Tries each extension: .py, .yaml, .yml, .sh, .js, etc.
# → For each space in order: project → user → system
# → First file found wins
```

Tool extensions are **dynamic** — determined by `get_tool_extensions()` which discovers supported extensions from extractor configs. The default set includes `.py`, `.yaml`, `.yml`, `.sh`, `.js`, and others.

### KnowledgeResolver

```python
resolver = KnowledgeResolver(project_path=Path("/my/project"))

path = resolver.resolve("patterns/singleton")
# → Tries extensions: .md, .yaml, .yml
# → project/.ai/knowledge/patterns/singleton.md → user → system
```

## Resolution Algorithm

All three resolvers follow the same algorithm:

```
for space in [project, user, system]:
    base_dir = get_type_path(space, item_type)
    if not base_dir.exists():
        continue

    for ext in valid_extensions:
        file_path = base_dir / f"{item_id}{ext}"
        if file_path.is_file():
            return file_path

return None  # not found
```

The item ID is a **relative path** from `.ai/{type}/` without the extension. For example:

| Item ID | File Path |
|---------|-----------|
| `core/build` | `.ai/directives/core/build.md` |
| `rye/core/runtimes/python_script_runtime` | `.ai/tools/rye/core/runtimes/python_script_runtime.yaml` |
| `patterns/singleton` | `.ai/knowledge/patterns/singleton.md` |

## Overriding System Items

To override a system tool, create a file with the same item ID in your project space:

```bash
# System provides: site-packages/rye/.ai/tools/rye/bash/bash.py
# Override by creating: my-project/.ai/tools/rye/bash/bash.py

# Your project version will be used instead of the system version.
```

This is useful for:

- Patching a bug in a system tool for your project
- Customizing runtime behavior (e.g., different venv path)
- Testing modified versions before contributing upstream

## The Bundle System

System space supports **multiple bundles** via the `rye.bundles` entry point group. Each bundle is a separate Python package that contributes items to the system space. See [Packages and Bundles](packages-and-bundles.md) for the full breakdown of how packages map to bundles and how dependencies are layered.

### BundleInfo

```python
@dataclass(frozen=True)
class BundleInfo:
    bundle_id: str        # e.g., "rye-core", "rye-mcp"
    version: str          # semver
    root_path: Path       # path to bundle root containing .ai/
    manifest_path: Path   # path to manifest.yaml (optional)
    source: str           # entry point name
    categories: List[str] # optional category filter
```

### Bundle Discovery

Bundles are discovered via Python entry points:

```toml
# In a bundle's pyproject.toml:
[project.entry-points."rye.bundles"]
my_bundle = "my_package:get_bundle_info"
```

The entry point function returns a dict:

```python
def get_bundle_info():
    return {
        "bundle_id": "my-tools",
        "version": "1.0.0",
        "root_path": Path(__file__).parent,
        "categories": ["my-tools"],  # optional: limit which subdirectories to include
    }
```

`get_system_spaces()` loads all bundles, caches the result at module level, and returns them sorted by entry point name.

### Bundle Manifests

Each bundle can optionally include a `manifest.yaml` at `.ai/bundles/{bundle_id}/manifest.yaml` describing its contents with integrity hashes. Auto-discovered if not explicitly set.

### Multi-Bundle Resolution

When resolving tools in system space, `PrimitiveExecutor._resolve_tool_path()` iterates over all bundles:

```python
system_entries = [
    (bundle.root_path / AI_DIR / "tools", f"system:{bundle.bundle_id}")
    for bundle in self.system_spaces
]
```

Each bundle gets a space label like `system:rye-core` or `system:my-tools`.

## Space Compatibility in Executor Chains

The space system directly affects what chains are valid. `ChainValidator` enforces:

```
SPACE_PRECEDENCE = {"project": 3, "user": 2, "system": 1}
```

**Rule**: A tool from a lower-precedence space cannot depend on a tool from a higher-precedence space.

| Chain | Valid? | Reason |
|-------|--------|--------|
| project tool → user runtime → system primitive | ✅ | Descending precedence |
| project tool → system runtime | ✅ | Project can use system |
| user tool → user runtime → system primitive | ✅ | Same then descending |
| user tool → project runtime | ❌ | User cannot depend on project |
| system tool → user runtime | ❌ | System cannot depend on user |

This ensures that:

- System tools work everywhere (no project/user dependencies)
- User tools work in any project (no project dependencies)
- Project tools can depend on anything (highest precedence)

## Path Utilities

`rye/utils/path_utils.py` provides helper functions used throughout the system:

| Function | Purpose |
|----------|---------|
| `get_user_space()` | Returns user base path (`$USER_SPACE` or `~`) |
| `get_system_space()` | Returns system base path (`site-packages/rye/`) |
| `get_system_spaces()` | Returns all bundle `BundleInfo` objects |
| `get_project_type_path(project, type)` | Returns `{project}/.ai/{type_dir}/` |
| `get_user_type_path(type)` | Returns `~/.ai/{type_dir}/` |
| `get_system_type_path(type)` | Returns core system type path |
| `get_system_type_paths(type)` | Returns type paths across all bundles |
| `extract_category_path(file, type, location)` | Extracts category from file path |
| `validate_path_structure(...)` | Validates filename and category match metadata |
