<!-- rye:signed:2026-04-06T04:15:08Z:92119c85dcd73d06f1d23809be71344427304c85b330f143b1c488504e4a329b:vUh2KKJovzYacSnTDlSFCa4sH7wo1itwNJGkJOGRqRkqSyx3O9eyD7KL4oZUC9vAqd6EAEkM1X_mbsVxyO9eDg:4b987fd4e40303ac -->

```yaml
name: ai-directory
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
  - state
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
├── config/
│   ├── agent/
│   │   ├── agent.yaml
│   │   ├── coordination.yaml
│   │   ├── resilience.yaml
│   │   ├── events.yaml
│   │   ├── error_classification.yaml
│   │   ├── capability_risk.yaml
│   │   ├── hook_conditions.yaml
│   │   └── budget_ledger_schema.yaml
│   ├── keys/
│   │   ├── signing/   # Ed25519 signing keypairs
│   │   └── trusted/   # Trusted public keys
│   └── web/
│       ├── websearch.yaml
│       └── browser.json
├── bundles/       # Bundle manifests
└── state/         # Runtime state (auto-generated, gitignored)
    ├── threads/   # Thread execution state
    ├── graphs/    # Graph run state
    ├── objects/   # CAS blobs
    └── cache/     # Tool runtime cache
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
| `state/`     | Runtime state — threads, graphs, CAS objects, cache (gitignored)           | Yes            |

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

Namespaces work with `rye_fetch` scope parameter:

```
rye_fetch(scope="tool.rye.bash.*", query="execute", project_path=".")
rye_fetch(scope="directive.rye.core.*", query="create", project_path=".")
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
