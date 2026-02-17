<!-- rye:signed:2026-02-17T23:54:02Z:b8f75013958a6f1fb02208adc73696bc6b5b77a4d7cbe6c680c1d0921c0683dc:nYt8EyymoSKU2KXyMmsalJsQazxvWty74tTiitRM_ahaGQW4RMo8Vj1ghoR3sbNYRUlqR2OiCdYzYwxjavRmDg==:440443d0858f0199 -->

```yaml
id: three-tier-spaces
title: Three-Tier Space System
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - spaces
  - resolution
  - paths
references:
  - ai-directory
  - "docs/internals/three-tier-spaces.md"
```

# Three-Tier Space System

How items are resolved across project, user, and system spaces.

## The Three Spaces

| Space       | Path                        | Precedence  | Mutability |
| ----------- | --------------------------- | ----------- | ---------- |
| **Project** | `{project_path}/.ai/`      | 3 (highest) | Read-write |
| **User**    | `{$USER_SPACE or ~}/.ai/`  | 2           | Read-write |
| **System**  | Discovered via `rye.bundles` entry points | 1 (lowest)  | Immutable  |

**First match wins.** Project space shadows user, user shadows system.

## Resolution Algorithm

```
for space in [project, user, system]:
    base_dir = get_type_path(space, item_type)
    if not base_dir.exists():
        continue
    for ext in valid_extensions:
        file_path = base_dir / f"{item_id}{ext}"
        if file_path.is_file():
            return file_path
return None
```

### Concrete Example

Resolving `rye_execute(item_type="tool", item_id="rye/bash/bash")`:

1. **Project:** `{project}/.ai/tools/rye/bash/bash.py` — if exists, use it
2. **User:** `~/.ai/tools/rye/bash/bash.py` — if exists, use it
3. **System:** `site-packages/rye/.ai/tools/rye/bash/bash.py` — fallback

## Resolver Classes

Three resolver classes in `rye/utils/resolvers.py`:

| Class                | Item Type  | Extensions                    |
| -------------------- | ---------- | ----------------------------- |
| `DirectiveResolver`  | directive  | `.md`                         |
| `ToolResolver`       | tool       | `.py`, `.yaml`, `.yml`, `.sh`, `.js`, etc. |
| `KnowledgeResolver`  | knowledge  | `.md`, `.yaml`, `.yml`        |

### Usage

```python
resolver = DirectiveResolver(project_path=Path("/my/project"))

# Resolve by ID (first match wins)
path = resolver.resolve("core/build")

# Resolve with space label
path, space = resolver.resolve_with_space("core/build")
# → (Path(".ai/directives/core/build.md"), "project")

# List all search paths
paths = resolver.get_search_paths()
# → [(Path("{project}/.ai/directives"), "project"),
#     (Path("~/.ai/directives"), "user"),
#     (Path("site-packages/rye/.ai/directives"), "system")]
```

## Path Utility Functions

`rye/utils/path_utils.py`:

| Function                               | Returns                                       |
| -------------------------------------- | --------------------------------------------- |
| `get_user_space()`                     | User base path (`$USER_SPACE` or `~`)         |
| `get_system_space()`                   | System base path (`site-packages/rye/`)       |
| `get_system_spaces()`                  | All bundle `BundleInfo` objects                |
| `get_project_type_path(project, type)` | `{project}/.ai/{type_dir}/`                   |
| `get_user_type_path(type)`             | `~/.ai/{type_dir}/`                           |
| `get_system_type_path(type)`           | Core system type path                         |
| `get_system_type_paths(type)`          | Type paths across all bundles                  |
| `extract_category_path(file, type, location)` | Category from file path              |
| `validate_path_structure(...)`         | Validates filename and category match metadata |

## USER_SPACE Environment Variable

`USER_SPACE` sets the **base path** (not the `.ai/` path). The `AI_DIR` constant (`.ai`) is always appended:

```bash
# Default: ~/.ai/
export USER_SPACE=/custom/home   # → /custom/home/.ai/
```

Consistent across all three spaces: `project_path / AI_DIR`, `get_user_space() / AI_DIR`, `get_system_space() / AI_DIR`.

## Overriding System Items

Place a file with the same item ID in project or user space:

```bash
# System provides: site-packages/rye/.ai/tools/rye/bash/bash.py
# Override by creating: my-project/.ai/tools/rye/bash/bash.py
```

Copy using `rye_load`:
```python
rye_load(item_type="tool", item_id="rye/bash/bash",
         source="system", destination="project", project_path=".")
```

## Search Deduplication

`rye_search` checks all three spaces and deduplicates by item ID:
- Project wins over user
- User wins over system

Restrict to a single space:
```python
rye_search(scope="directive", query="create", project_path=".", space="system")
```

## Space Compatibility in Chains

`ChainValidator` enforces:

```
SPACE_PRECEDENCE = {"project": 3, "user": 2, "system": 1}
```

**Rule:** A child can only depend on elements with equal or lower precedence.

| Chain                                      | Valid? | Reason                        |
| ------------------------------------------ | ------ | ----------------------------- |
| project tool → user runtime → system prim  | ✅     | Descending precedence         |
| project tool → system runtime              | ✅     | Project can use system        |
| user tool → project runtime                | ❌     | User cannot depend on project |
| system tool → user runtime                 | ❌     | System cannot depend on user  |

## Bundle System

System space supports multiple bundles via `rye.bundles` entry point group:

```python
@dataclass(frozen=True)
class BundleInfo:
    bundle_id: str        # e.g., "rye-core"
    version: str          # semver
    root_path: Path       # path to bundle root containing .ai/
    manifest_path: Path   # path to manifest.yaml
    source: str           # entry point name
    categories: List[str] # optional category filter
```

Bundle registration in `pyproject.toml`:
```toml
[project.entry-points."rye.bundles"]
my_bundle = "my_package:get_bundle_info"
```

Multi-bundle resolution labels each bundle: `system:rye-core`, `system:my-tools`.
