---
category: "ryeos/concepts"
name: "canonical-refs"
description: "How items are addressed: canonical ref syntax and resolution rules"
---

# Canonical Refs

Items are addressed by **canonical refs** in the format `kind:item_id`.

## Syntax

```
kind:path/without/extension
```

- `kind` — one of: `directive`, `tool`, `knowledge`, `config`, `service`, `handler`, `protocol`, `parser`, `runtime`, `graph`, `streaming_tool`, `node`
- `path/without/extension` — the item's path relative to the kind's directory, without file extension

The kind prefix is optional on `execute` and `fetch` — the system auto-detects type by searching all kind directories.

## Examples

| Canonical Ref | File Path |
|---|---|
| `tool:ryeos/core/identity/public_key` | `.ai/tools/ryeos/core/identity/public_key.yaml` |
| `directive:ryeos/core/init` | `.ai/directives/ryeos/core/init.md` |
| `knowledge:ryeos/development/architecture` | `.ai/knowledge/ryeos/development/architecture.md` |
| `handler:ryeos/core/yaml-document` | `.ai/handlers/ryeos/core/yaml-document.yaml` |
| `protocol:ryeos/core/runtime_v1` | `.ai/protocols/ryeos/core/runtime_v1.yaml` |
| `parser:ryeos/core/markdown/frontmatter` | `.ai/parsers/ryeos/core/markdown/frontmatter.yaml` |
| `service:ryeos/core/fetch` | `.ai/services/ryeos/core/fetch.yaml` |
| `config:ryeos-runtime/model-providers/anthropic` | `.ai/config/ryeos-runtime/model-providers/anthropic.yaml` |

## Resolution

The engine resolves canonical refs through the three-tier space system:

1. **Project** (`.ai/` in project root) — first match wins
2. **User** (`~/.ai/`) — second
3. **System** (`$XDG_DATA_DIR/ryeos/.ai/`) — last

For each space, the engine:
1. Determines the kind's directory from the kind schema (e.g., `tools` for `tool`)
2. Appends the item_id path
3. Tries known extensions for the kind's format (`.yaml`, `.yml`, `.md`)
4. Returns the first file that exists and verifies

## Bare refs

Without a kind prefix, the system searches all kind directories:

```
ryeos/core/identity/public_key
```

The engine tries this against every kind's directory until it finds a match. This is slower but convenient for CLI use and MCP calls where the user may not know the kind.

## Globs for signing

`rye_sign` supports glob patterns in canonical refs:

- `directive:*` — sign all directives
- `tool:ryeos/core/*` — sign all tools under the ryeos/core namespace
- `knowledge:*` — sign all knowledge entries

Globs expand by scanning the resolved directories for matching paths.
