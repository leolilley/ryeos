```yaml
id: ai-directory
title: "The .ai/ Directory"
description: Structure and conventions of the .ai/ portable data bundle
category: getting-started
tags: [ai-directory, structure, conventions, spaces]
version: "1.0.0"
```

# The .ai/ Directory

The `.ai/` directory is a portable data bundle that gives AI agents structured access to workflows, tools, and domain knowledge. It travels with your project in version control, and Rye OS resolves items from it at runtime.

## Directory structure

```
.ai/
├── directives/    # Workflow instructions (.md files)
├── tools/         # Executable items (.py, .yaml, .sh, .js)
├── knowledge/     # Domain information (.md files)
├── bundles/       # Bundle manifests
├── lockfiles/     # Integrity pinning
├── threads/       # Thread execution state (auto-generated at runtime)
└── outputs/       # Tool output artifacts (auto-generated at runtime)
```

### Core directories

| Directory     | Contents                                    | File types                             |
| ------------- | ------------------------------------------- | -------------------------------------- |
| `directives/` | Multi-step workflow definitions             | `.md` (Markdown with embedded XML)     |
| `tools/`      | Executable scripts and configurations       | `.py`, `.yaml`, `.sh`, `.js`           |
| `knowledge/`  | Domain information, patterns, and learnings | `.md` (Markdown with YAML frontmatter) |

### Supporting directories

| Directory    | Purpose                                                                                                                         |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------- |
| `bundles/`   | Bundle manifests (`manifest.yaml`) that declare which items belong to a bundle, with SHA-256 hashes for integrity verification. |
| `lockfiles/` | Integrity pinning files that lock item versions and hashes.                                                                     |
| `threads/`   | Auto-generated at runtime. Stores thread execution state when directives run as threaded workflows.                             |
| `outputs/`   | Auto-generated at runtime. Stores artifacts produced by tool executions.                                                        |

## Item ID convention

Every item in Rye OS is identified by its **item ID** — the relative path from `.ai/<type>/` to the file, without the file extension.

| Item ID                                 | Item type | File path                                                |
| --------------------------------------- | --------- | -------------------------------------------------------- |
| `greet_user`                            | directive | `.ai/directives/greet_user.md`                           |
| `rye/core/create_directive`             | directive | `.ai/directives/rye/core/create_directive.md`            |
| `rye/bash/bash`                         | tool      | `.ai/tools/rye/bash/bash.py`                             |
| `rye/file-system/write`                 | tool      | `.ai/tools/rye/file-system/write.py`                     |
| `project_conventions`                   | knowledge | `.ai/knowledge/project_conventions.md`                   |
| `rye/core/directive-metadata-reference` | knowledge | `.ai/knowledge/rye/core/directive-metadata-reference.md` |

The item ID is what you pass to `rye_execute`, `rye_load`, `rye_sign`, and `rye_search`:

```
rye_execute(item_type="directive", item_id="rye/core/create_directive", ...)
rye_load(item_type="tool", item_id="rye/bash/bash", ...)
rye_sign(item_type="knowledge", item_id="project_conventions", ...)
```

## Namespace convention

Items are organized into **namespaces** using directory nesting. The first path segment typically identifies the project or bundle that owns the item.

```
.ai/
├── directives/
│   ├── rye/core/              # rye/core namespace — ships with ryeos
│   │   ├── create_directive.md
│   │   ├── create_tool.md
│   │   └── create_knowledge.md
│   └── my-project/            # my-project namespace — your custom items
│       └── deploy.md
├── tools/
│   ├── rye/bash/              # rye/bash namespace
│   │   └── bash.py
│   └── my-project/utils/      # my-project/utils namespace
│       └── lint.py
└── knowledge/
    └── rye/core/              # rye/core namespace
        └── directive-metadata-reference.md
```

Common namespace prefixes:

- **`rye/core/`** — core items that ship with the `ryeos-core` package.
- **`rye/bash/`**, **`rye/web/`**, **`rye/file-system/`** — built-in tool categories.
- **`<your-project>/`** — your project-specific items.

Namespaces also work with search scopes. To search only within a namespace:

```
rye_search(scope="tool.rye.bash.*", query="execute", project_path=".")
```

## The 3-tier space system

Rye OS resolves items across three **spaces**, checked in priority order:

```
┌─────────────────────────────────────────────┐
│  1. Project Space  (highest priority)       │
│     {project_path}/.ai/                     │
│     Project-specific items, committed to    │
│     version control with your code.         │
├─────────────────────────────────────────────┤
│  2. User Space                              │
│     {$USER_SPACE or ~}/.ai/                 │
│     Shared across all projects for a user.  │
│     Personal customizations and overrides.  │
├─────────────────────────────────────────────┤
│  3. System Space   (lowest priority)        │
│     site-packages/rye/.ai/                  │
│     Immutable. Ships with the ryeos          │
│     package — the "standard library."       │
└─────────────────────────────────────────────┘
```

### Project space

**Location:** `{project_path}/.ai/`

The project space is the `.ai/` directory in your project root. Items here are specific to the project and should be committed to version control. This space has the highest priority — if an item ID exists here, it wins.

Use project space for:

- Project-specific workflows and directives
- Custom tools tailored to your codebase
- Domain knowledge about your project's architecture and conventions

### User space

**Location:** `{$USER_SPACE or ~}/.ai/`

The user space lives in your home directory (or the path set by `$USER_SPACE`). Items here are shared across all your projects. This is the place for personal preferences and tools you use everywhere.

Use user space for:

- Personal workflow customizations
- Tools you use across multiple projects
- Overrides to system-space defaults

### System space

**Location:** `site-packages/rye/.ai/`

The system space ships inside the `ryeos` Python package. It is immutable — you cannot modify it directly. It provides the built-in "standard library" of directives, tools, and knowledge.

System-space items include:

- `rye/core/create_directive` — directive for creating new directives
- `rye/core/create_tool` — directive for creating new tools
- `rye/core/create_knowledge` — directive for creating new knowledge entries
- `rye/bash/bash` — shell command execution tool
- `rye/web/search/search`, `rye/web/fetch/fetch` — web interaction tools
- `rye/core/directive-metadata-reference` — knowledge entry documenting directive format

## Resolution order

When you call `rye_execute(item_type="tool", item_id="rye/bash/bash", ...)`, Rye OS resolves the item by checking each space in order:

1. **Project:** `{project_path}/.ai/tools/rye/bash/bash.py` — if it exists, use it.
2. **User:** `~/.ai/tools/rye/bash/bash.py` — if it exists, use it.
3. **System:** `site-packages/rye/.ai/tools/rye/bash/bash.py` — fallback.

**First match wins.** This means you can override any system item by placing a file with the same item ID in your project or user space. For example, to customize the bash tool for your project, copy it into your project space:

```
rye_load(item_type="tool", item_id="rye/bash/bash", source="system", destination="project", project_path=".")
```

This copies the system version into `.ai/tools/rye/bash/bash.py` in your project, where you can modify it. Your project's version will take priority over the system version.

When searching, `rye_search` checks all three spaces by default and deduplicates by item ID (project wins over user, user wins over system). You can restrict the search to a specific space:

```
rye_search(scope="directive", query="create", project_path=".", space="system")
```

## Bundles

The system space supports **bundles** — packaged collections of items distributed as Python packages. Each bundle registers itself via the `rye.bundles` entry point group.

A bundle manifest lives at `.ai/bundles/<bundle_id>/manifest.yaml` and declares:

```yaml
bundle:
  id: ryeos-core
  version: 0.1.0
  type: package
  description: Core directives, tools, and knowledge for Rye OS
files:
  .ai/directives/rye/core/create_directive.md:
    sha256: c7deaec3367b868e9fc42f9626b347ed21819baa...
  .ai/tools/rye/bash/bash.py:
    sha256: 5d4ac0daaa9f4b5070b677bfdc8325d201ecef6a...
  # ... all items in the bundle with integrity hashes
```

The manifest enables integrity verification — Rye OS can confirm that no bundled file has been tampered with by comparing its SHA-256 hash against the manifest.

Multiple bundles can coexist in the system space. The `ryeos-core` package ships the `ryeos-core` bundle, the `ryeos` and `ryeos-mcp` packages ship the `ryeos` bundle. Third-party packages can register their own bundles by declaring a `rye.bundles` entry point in their `pyproject.toml`:

```toml
[project.entry-points."rye.bundles"]
my-bundle = "my_package.bundle:get_bundle_info"
```

The entry point function returns a dict with `bundle_id`, `root_path`, `version`, and optionally `categories` to scope which namespace prefixes the bundle provides.

## What's next

- [Installation](installation.md) — Set up Rye OS and connect it to your AI agent.
- [Quickstart](quickstart.md) — Create your first directive, tool, and knowledge entry.
