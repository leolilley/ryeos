---
category: ryeos/core
tags: [fundamentals, addressing, refs]
version: "1.0.0"
description: >
  How items are addressed in Rye OS. Canonical refs, bare IDs,
  and the resolution algorithm.
---

# Canonical Refs

Every item in Rye OS is addressed by a **canonical ref**: a structured
identifier that encodes the kind and the path.

## Format

```
kind:path/to/item
```

- **kind** — one of `directive`, `tool`, `knowledge`, `config`, `graph`,
  `handler`, `parser`, `protocol`, `runtime`, `service`, `node`
- **path** — slash-separated path *without* file extension

The kind determines which subdirectory of `.ai/` to look in:

| Kind        | Directory       | Example                                  |
|-------------|-----------------|------------------------------------------|
| `directive` | `directives/`   | `directive:my/deploy` → `directives/my/deploy.md` |
| `tool`      | `tools/`        | `tool:ryeos/core/sign` → `tools/ryeos/core/sign.yaml` |
| `knowledge` | `knowledge/`    | `knowledge:ryeos/core/signing` → `knowledge/ryeos/core/signing.md` |
| `config`    | `config/`       | `config:execution/execution` → `config/execution/execution.yaml` |
| `graph`     | `graphs/`       | `graph:my/pipeline` → `graphs/my/pipeline.yaml` |
| `handler`   | `handlers/`     | `handler:ryeos/core/identity` → `handlers/ryeos/core/identity.yaml` |
| `parser`    | `parsers/`      | `parser:ryeos/core/yaml/yaml` → `parsers/ryeos/core/yaml/yaml.yaml` |
| `protocol`  | `protocols/`    | `protocol:ryeos/core/opaque` → `protocols/ryeos/core/opaque.yaml` |
| `runtime`   | `runtimes/`     | `runtime:directive-runtime` → `runtimes/directive-runtime.yaml` |
| `service`   | `services/`     | `service:fetch` → `services/fetch.yaml` |
| `node`      | `node/`         | Various sub-paths (verbs, aliases, routes, engine) |

## Bare IDs

In `execute` and `fetch`, you can omit the kind prefix (bare ID):
`my/deploy` instead of `directive:my/deploy`. The system auto-detects
the kind by searching directories in kind priority order.

## Resolution Algorithm

When resolving `kind:path/to/item`, the engine searches three spaces
in order:

1. **Project** — `<project_root>/.ai/<kind_dir>/path/to/item.*`
2. **User** — `~/.ai/<kind_dir>/path/to/item.*`
3. **System** — Each installed bundle's `.ai/<kind_dir>/path/to/item.*`

First match wins. If no match is found, the engine returns a
resolution error.

## Globs

The `sign` verb accepts glob patterns in canonical refs for batch
operations:

- `directive:*` — sign all directives
- `tool:ryeos/core/*` — sign all tools under the `ryeos/core` namespace
- `knowledge:**` — sign all knowledge entries recursively

## Usage in Directives

Inside directive YAML frontmatter, canonical refs appear in:

- `extends: "directive:base/workflow"` — inheritance chain
- `context:` blocks — `ref: "knowledge:ryeos/core/signing"`
- `permissions.execute:` — `["ryeos.execute.tool.ryeos.file-system.*"]`
- Action targets — `item_id: "tool:my/project/deploy"`
