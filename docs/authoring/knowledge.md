---
id: knowledge
title: "Authoring Knowledge"
description: How to write knowledge entries — domain information for AI agents
category: authoring
tags: [knowledge, authoring, format, frontmatter]
version: "1.0.0"
---

# Authoring Knowledge

Knowledge entries are markdown documents that provide **context to AI agents** — domain information, patterns, learnings, and reference material. They live in `.ai/knowledge/` and are loaded into agent context when needed.

## File Format

Knowledge entries use standard YAML frontmatter followed by markdown content:

```markdown
---
id: category/entry-name
title: Entry Title
description: What this knowledge covers
category: category
tags: [tag1, tag2]
entry_type: reference
version: "1.0.0"
author: rye-os
created_at: 2026-02-10T00:00:00Z
---

# Knowledge Content

Markdown content that the AI agent reads for context.
```

The file is parsed by the `markdown_frontmatter` parser, which extracts the YAML metadata and returns the markdown body.

## Frontmatter Fields

### Required

| Field | Type | Purpose | Example |
|-------|------|---------|---------|
| `id` | string (kebab-case) | Unique identifier | `terminology` |
| `title` | string | Human-readable title | `Terminology and Naming Conventions` |
| `category` | string | Directory path in `.ai/knowledge/` | `rye/core` |
| `version` | string (semver) | Content version | `"1.0.0"` |
| `author` | string | Creator | `rye-os` |

### Optional

| Field | Type | Purpose | Example |
|-------|------|---------|---------|
| `description` | string | Brief summary | `"What this knowledge covers"` |
| `tags` | list of strings | Searchable tags (3-5 recommended) | `[terminology, naming]` |
| `created_at` | ISO 8601 datetime | Creation timestamp | `2026-02-10T00:00:00Z` |
| `validated` | ISO 8601 datetime | Last validation timestamp | `2026-02-10T00:00:00Z` |
| `entry_type` | string | Classification of content | `reference` |
| `references` | list | Links to related knowledge/URLs | `[oauth-overview, "https://..."]` |
| `extends` | list | Knowledge this builds upon | `[authentication-basics]` |
| `used_by` | list | Directives/tools that use this | `[setup-oauth-provider]` |

### Entry Types

| Type | Purpose | When to Use |
|------|---------|-------------|
| `reference` | Stable documentation | Specs, API references, conventions that rarely change |
| `learning` | From experience | Insights discovered during execution, debugging findings |
| `pattern` | Reusable approaches | Design patterns, architectural decisions, best practices |

## Knowledge Graph

Knowledge entries form a navigable graph through explicit link relationships in the frontmatter:

```yaml
references:
  - jwt-overview                          # Internal knowledge link
  - cryptographic-algorithms              # Internal knowledge link
  - "https://tools.ietf.org/html/rfc7519" # External URL

extends:
  - authentication-basics                 # This builds on auth basics
  - cryptographic-signatures              # And on crypto knowledge

used_by:
  - api-authentication                    # Used by this directive
  - service-authorization                 # And this one
```

Navigation:
- **`extends`** → upward to foundational concepts
- **`references`** → lateral to related knowledge
- **`used_by`** → inbound from directives/tools that depend on this entry
- **Backlinks** are automatically derived from other entries pointing here

## Loading Knowledge

Knowledge is loaded via `rye_load` and injected into agent context:

```python
# In a directive process step
rye_load(item_type="knowledge", item_id="rye/core/terminology")
# Returns: "Use this knowledge to inform your decisions."
```

Knowledge can also be loaded automatically via thread hooks:

```xml
<!-- In a directive's hooks section -->
<hooks>
  <hook>
    <when>thread_started</when>
    <execute item_type="knowledge">rye/core/terminology</execute>
  </hook>
</hooks>
```

## File Resolution

Knowledge entries resolve by item_id to file path:

```
item_id: "rye/core/terminology"
  → .ai/knowledge/rye/core/terminology.md

item_id: "security/jwt-validation"
  → .ai/knowledge/security/jwt-validation.md
```

The category determines the directory path within `.ai/knowledge/`. Knowledge can also be YAML files (`.yaml`/`.yml` extension) instead of markdown.

## Real Examples

### Reference Entry: `terminology`

From `.ai/knowledge/rye/core/terminology.md`:

```markdown
---
id: terminology
title: Terminology and Naming Conventions
category: rye/core
version: "1.0.0"
author: rye-os
tags:
  - terminology
  - naming
  - conventions
  - style-guide
created: 2026-02-10T00:00:00Z
validated: 2026-02-10T00:00:00Z
---

# Terminology and Naming Conventions

This document establishes consistent terminology and naming conventions
for Rye OS documentation and code.

## Project Names

| Term       | Usage           | Notes                |
| ---------- | --------------- | -------------------- |
| **Rye OS** | Preferred usage | Official project name |
| **RYE**    | Acceptable      | Uppercase abbreviation |
| **rye**    | Acceptable      | Package name, lowercase |

## Item Types

| Type          | Location          | Format                 | Purpose                    |
| ------------- | ----------------- | ---------------------- | -------------------------- |
| **directive** | `.ai/directives/` | XML in Markdown        | Workflow orchestration     |
| **tool**      | `.ai/tools/`      | Python, YAML, scripts  | Executable operations      |
| **knowledge** | `.ai/knowledge/`  | Markdown + frontmatter | Documentation and patterns |

## Naming Conventions

### Knowledge Entry IDs — use kebab-case:
  - ✅ `data-driven-architecture`
  - ❌ `data_driven_architecture`

### Directive Names — use kebab-case:
  - ✅ `deploy-production`
  - ❌ `deploy_production`

### Tool IDs — use kebab-case:
  - ✅ `deploy-kubernetes`
  - ❌ `DeployKubernetes`
```

**What to notice:**
- Uses `tags` as a YAML list (not inline `[...]`) — both formats work
- `created` and `validated` timestamps for tracking
- Contains tables, code examples — any markdown is valid in the body
- Cross-references other knowledge entries in `## Related` section

### Specification Entry: `directive-metadata-reference`

From `rye/rye/.ai/knowledge/rye/core/directive-metadata-reference.md`:

```yaml
---
id: directive-metadata-reference
title: Directive Metadata Reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-02T16:50:00Z
tags:
  - metadata
  - directives
  - specification
references:
  - knowledge-metadata-reference
  - tool-metadata-reference
---
```

**What to notice:**
- `references` links to sibling knowledge entries, forming a knowledge graph
- Serves as canonical specification — `entry_type: reference` material
- Tags include both domain (`directives`) and content type (`specification`)

### Knowledge Graph Entry: `knowledge-metadata-reference`

From `rye/rye/.ai/knowledge/rye/core/knowledge-metadata-reference.md`:

```yaml
---
id: knowledge-metadata-reference
title: Knowledge Metadata Reference
category: rye/core
version: "1.0.0"
author: rye-os
created_at: 2026-02-02T16:50:00Z
tags:
  - metadata
  - knowledge
  - specification
references:
  - directive-metadata-reference
  - tool-metadata-reference
---
```

All three `*-metadata-reference` entries cross-reference each other via `references`, creating a navigable cluster in the knowledge graph.

## Creating Knowledge via Directive

The `create_knowledge` directive automates knowledge creation:

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_knowledge",
    parameters={
        "id": "jwt-validation",
        "title": "JWT Validation Patterns",
        "category": "security/authentication",
        "content": "Best practices for validating JWT tokens...",
        "tags": "jwt, tokens, security"
    }
)
```

This handles file creation, frontmatter generation, and signing.

## Best Practices

- **Focused scope** — one topic per entry; split if it exceeds ~2000 words
- **Kebab-case IDs** — `data-driven-architecture`, not `data_driven_architecture`
- **3-5 tags** — include both domain and content type
- **Use `references`** — link to related knowledge to build the graph
- **Include examples** — code samples, tables, and diagrams make knowledge actionable
- **Version on content changes** — bump version when the body changes, not for metadata tweaks
- **Plain language** — agents read this as context; avoid unnecessary jargon

## References

- [Knowledge Metadata Reference](../../rye/rye/.ai/knowledge/rye/core/knowledge-metadata-reference.md)
- [Terminology](../../rye/rye/.ai/knowledge/rye/core/terminology.md)
