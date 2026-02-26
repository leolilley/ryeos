<!-- rye:signed:2026-02-26T03:49:26Z:4ec96950148a895d57cff9b86dae0118a0f6c4fec65058a46c648c43a651fdcb:_5chogV0pN_k93fWe6bNG2acHLNUZBx3-RVoUAvk2M0uBYDvFcugQfcf0ExOdNj2CWE2E6Xi2LAsyAr-_6WcCg==:9fbfabe975fa5a7f -->
 -->
```yaml
name: three-tier-spaces
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
  - project-space
  - user-space
  - system-space
  - resolution-order
  - override
  - precedence
  - space-resolution
references:
  - ai-directory
  - "docs/internals/three-tier-spaces.md"
```

# Three-Tier Space System

How items are resolved across project, user, and system spaces.

## The Three Spaces

| Space       | Path                                      | Precedence  | Mutability |
| ----------- | ----------------------------------------- | ----------- | ---------- |
| **Project** | `{project_path}/.ai/`                     | 3 (highest) | Read-write |
| **User**    | `{USER_SPACE}/.ai/`                       | 2           | Read-write |
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
2. **User:** `{USER_SPACE}/.ai/tools/rye/bash/bash.py` — if exists, use it
3. **System:** `site-packages/rye/.ai/tools/rye/bash/bash.py` — fallback

## Resolver Classes

Three resolver classes in `rye/utils/resolvers.py`:

| Class               | Item Type | Extensions                                 |
| ------------------- | --------- | ------------------------------------------ |
| `DirectiveResolver` | directive | `.md`                                      |
| `ToolResolver`      | tool      | `.py`, `.yaml`, `.yml`, `.sh`, `.js`, etc. |
| `KnowledgeResolver` | knowledge | `.md`, `.yaml`, `.yml`                     |

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
#     (Path("{USER_SPACE}/.ai/directives"), "user"),
#     (Path("site-packages/rye/.ai/directives"), "system")]
```

## Path Utility Functions

`rye/utils/path_utils.py`:

| Function                                      | Returns                                        |
| --------------------------------------------- | ---------------------------------------------- |
| `get_user_space()`                            | User base path (`$USER_SPACE` or `~`)          |
| `get_system_spaces()`                         | All bundle `BundleInfo` objects                |
| `get_project_type_path(project, type)`        | `{project}/.ai/{type_dir}/`                    |
| `get_user_type_path(type)`                    | `{USER_SPACE}/.ai/{type_dir}/`                 |
| `get_system_type_paths(type)`                 | Type paths across all bundles                  |
| `extract_category_path(file, type, location)` | Category from file path                        |
| `validate_path_structure(...)`                | Validates filename and category match metadata |

## USER_SPACE Environment Variable

`USER_SPACE` sets the **base path** (not the `.ai/` path). The `AI_DIR` constant (`.ai`) is always appended:

```bash
# Default: ~/.ai/
export USER_SPACE=/custom/home   # → /custom/home/.ai/
```

Consistent across all three spaces: `project_path / AI_DIR`, `get_user_space() / AI_DIR`, `bundle.root_path / AI_DIR` (for each bundle in `get_system_spaces()`).

## Configuration Overrides

Config files follow the same three-tier pattern under `.ai/config/`, namespaced by category:

```
.ai/config/
├── agent/
│   ├── agent.yaml              # agent settings (default provider, tiers)
│   └── coordination.yaml       # thread coordination
└── web/
    └── websearch.yaml           # web search provider config
```

Resolution: system → user → project (deep merge). Each layer overrides fields from the layer below.

| Config       | System default                | User/Project override                |
| ------------ | ----------------------------- | ------------------------------------ |
| Agent        | `.ai/config/agent/agent.yaml` | `.ai/config/agent/agent.yaml`        |
| Coordination | —                             | `.ai/config/agent/coordination.yaml` |
| Websearch    | —                             | `.ai/config/web/websearch.yaml`      |

Config subdirectories are created by the features that need them (e.g., `setup_provider` creates `config/agent/`).

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

| Chain                                     | Valid? | Reason                        |
| ----------------------------------------- | ------ | ----------------------------- |
| project tool → user runtime → system prim | ✅     | Descending precedence         |
| project tool → system runtime             | ✅     | Project can use system        |
| user tool → project runtime               | ❌     | User cannot depend on project |
| system tool → user runtime                | ❌     | System cannot depend on user  |

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

Multi-bundle resolution labels each bundle: `system:ryeos-core`, `system:my-tools`.
