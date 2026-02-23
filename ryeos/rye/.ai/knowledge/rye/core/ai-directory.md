<!-- rye:signed:2026-02-23T00:43:10Z:8cdf2ffdd92b10b92e7f146eb473d418316c4297efbc8b9444b3c208d9464dac:33KZ3atAro851C--67eaMfnWTIfk47AtBsNWIfV72x9jbPcRhaKUOe3HowhTQcoTeKjHGaaLLMYnSB0HtPeQAA==:9fbfabe975fa5a7f -->

```yaml
id: ai-directory
title: The .ai/ Directory Structure
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - ai-directory
  - structure
  - file-system
  - dot-ai
  - directory-layout
  - directives
  - tools
  - knowledge
  - bundles
  - lockfiles
references:
  - terminology
  - three-tier-spaces
  - "docs/getting-started/ai-directory.md"
```

# The .ai/ Directory Structure

Layout and conventions for the `.ai/` portable data bundle.

## Directory Tree

```
.ai/
├── directives/    # Workflow instructions
├── tools/         # Executable items
├── knowledge/     # Domain information
├── bundles/       # Bundle manifests
├── lockfiles/     # Integrity pinning
├── threads/       # Thread execution state  (auto-generated)
└── outputs/       # Tool output artifacts   (auto-generated)
```

## Core Directories

| Directory      | Contents                            | File Extensions                         | Parser Used            |
| -------------- | ----------------------------------- | --------------------------------------- | ---------------------- |
| `directives/`  | Multi-step workflow definitions     | `.md` (Markdown with embedded XML)      | `markdown_xml`         |
| `tools/`       | Executable scripts and configs      | `.py`, `.yaml`, `.yml`, `.sh`, `.js`    | `python_ast` or `yaml` |
| `knowledge/`   | Domain info, patterns, learnings    | `.md` (Markdown with YAML frontmatter)  | `markdown_frontmatter` |

## Supporting Directories

| Directory    | Purpose                                                                    | Auto-Generated |
| ------------ | -------------------------------------------------------------------------- | -------------- |
| `bundles/`   | Bundle manifests (`manifest.yaml`) with SHA-256 hashes per item            | No             |
| `lockfiles/` | Chain integrity pinning files (`{tool_id}@{version}.lock.json`)            | Yes (on first execution) |
| `threads/`   | Thread execution state — registry, transcripts, budgets, artifacts         | Yes            |
| `outputs/`   | Artifacts produced by tool executions                                      | Yes            |

## Item ID ↔ File Path Mapping

The item ID is the relative path from `.ai/<type>/` to the file, without extension.

| Item ID                                 | Type      | File Path                                                |
| --------------------------------------- | --------- | -------------------------------------------------------- |
| `greet_user`                            | directive | `.ai/directives/greet_user.md`                           |
| `rye/core/create_directive`             | directive | `.ai/directives/rye/core/create_directive.md`            |
| `rye/bash/bash`                         | tool      | `.ai/tools/rye/bash/bash.py`                             |
| `rye/core/runtimes/python/script` | tool    | `.ai/tools/rye/core/runtimes/python/script.yaml` |
| `project_conventions`                   | knowledge | `.ai/knowledge/project_conventions.md`                   |
| `rye/core/directive-metadata-reference` | knowledge | `.ai/knowledge/rye/core/directive-metadata-reference.md` |

## Namespace Convention

First path segment identifies the owner. Subdirectories create deeper namespaces.

```
.ai/
├── directives/
│   ├── rye/core/           # rye/core namespace — standard library
│   └── my-project/         # my-project namespace — project-specific
├── tools/
│   ├── rye/bash/           # rye/bash namespace
│   └── my-project/utils/   # my-project/utils namespace
└── knowledge/
    └── rye/core/           # rye/core namespace
```

Common namespace prefixes:
- **`rye/core/`** — core items shipping with `rye-core`
- **`rye/bash/`**, **`rye/web/`**, **`rye/file-system/`** — built-in tool categories
- **`rye/agent/`** — thread orchestration system
- **`<your-project>/`** — project-specific items

## Search Scopes

Namespaces work with `rye_search` scope parameter:

```
rye_search(scope="tool.rye.bash.*", query="execute", project_path=".")
rye_search(scope="directive.rye.core.*", query="create", project_path=".")
```

Scope format: `{item_type}.{namespace.dotted}.*`

## File Extension Rules

| Item Type  | Valid Extensions                              | Primary      |
| ---------- | --------------------------------------------- | ------------ |
| directive  | `.md`                                         | `.md`        |
| tool       | `.py`, `.yaml`, `.yml`, `.sh`, `.js` + others | `.py`        |
| knowledge  | `.md`, `.yaml`, `.yml`                        | `.md`        |

Tool extensions are dynamic — discovered from extractor configs via `get_tool_extensions()`.

## Category ↔ Directory Relationship

The `category` metadata field must match the directory path:

```
category: rye/core  →  file lives at .ai/{type}/rye/core/{name}.{ext}
category: ""        →  file lives at .ai/{type}/{name}.{ext} (root)
```

`validate_path_structure()` enforces this correspondence.
