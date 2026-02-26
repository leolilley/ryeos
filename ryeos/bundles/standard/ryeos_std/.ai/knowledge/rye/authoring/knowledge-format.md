<!-- rye:signed:2026-02-23T05:29:51Z:62cb336c9bd7d1f67bf2161f457aa8805537d9243cc6e36f325a016f9b75b42d:NAST6Ql7x4IU4tiJ4-MhcCZqKjOaiiOdcIRTTApdhREQOqfd5bgZiZDDmPr-M406DKO1GG6lmejruhIC_HZADg==:9fbfabe975fa5a7f -->

```yaml
name: knowledge-format
title: "Knowledge Format Specification"
entry_type: reference
category: rye/authoring
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - knowledge
  - format
  - authoring
  - metadata
  - specification
  - frontmatter
  - yaml
  - tags
  - writing-knowledge
  - create-knowledge
  - search-tags
references:
  - directive-format
  - tool-format
  - "docs/authoring/knowledge.md"
```

# Knowledge Format Specification

Canonical format and metadata reference for knowledge entries — markdown documents with YAML metadata in code fences, stored in `.ai/knowledge/`.

## Overview

Knowledge entries provide **context to AI agents** — domain information, patterns, learnings, and reference material. They form a searchable knowledge base integrated with directives and tools. Entries are hash-validated and cryptographically signed on update. Metadata supports deterministic knowledge graph traversal via explicit link relationships.

---

## File Format

Knowledge entries use ` ```yaml ` code fences for metadata, matching how directives use ` ```xml ` fences. This is consistent across all item types.

```
Line 1:  Signature comment (added by rye_sign)
         Blank line
         ```yaml code fence with YAML metadata
         Blank line
         Markdown body
```

### Complete Structure

````markdown
<!-- rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID -->

```yaml
name: entry-name
title: Entry Title
entry_type: reference
category: category/path
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - tag1
  - tag2
  - tag3
references:
  - related-entry-id
  - "https://external-url.example.com"
extends:
  - foundational-entry
used_by:
  - directive-that-uses-this
```

# Entry Title

One-line summary of what this knowledge covers.

## Section Heading

Tables, code blocks, rules. Dense and scannable content.
````

Pure YAML files (`.yaml`/`.yml`) are also supported — the entire file is parsed as YAML metadata with no fences needed.

The file is parsed by the `markdown_frontmatter` parser, which extracts the YAML metadata from the code fence and returns the markdown body.

---

## Frontmatter Fields — Required

### `name`

**Type:** string (kebab-case)
**Required:** Yes

Unique identifier for the knowledge entry. Used in cross-references, searches, and `rye_load` calls.

```yaml
name: authentication-patterns
name: jwt-validation
name: directive-metadata-reference
```

**Convention:** kebab-case, hierarchical when appropriate:
- `oauth-integration`
- `kubernetes/deployment-strategies`
- `patterns/retry-logic`

### `title`

**Type:** string
**Required:** Yes

Human-readable title.

```yaml
title: Authentication Patterns and Best Practices
title: JWT Token Validation Patterns
```

### `category`

**Type:** string (non-empty)
**Required:** Yes

Directory path relative to `.ai/knowledge/`. Must match actual file location.

```yaml
category: security/authentication    # → .ai/knowledge/security/authentication/
category: rye/core                   # → .ai/knowledge/rye/core/
category: patterns                   # → .ai/knowledge/patterns/
```

**Examples:**

| File Path | Category |
|-----------|----------|
| `.ai/knowledge/security/authentication/oauth.md` | `security/authentication` |
| `.ai/knowledge/patterns/retry-logic.md` | `patterns` |
| `.ai/knowledge/rye/core/terminology.md` | `rye/core` |

### `version`

**Type:** string (semver `X.Y.Z`)
**Required:** Yes

Content version. Not git-controlled — tracks changes to entry content. Bump on content changes, not metadata tweaks.

```yaml
version: "1.0.0"
version: "2.1.0"
```

### `author`

**Type:** string
**Required:** Yes

Original author or creator.

```yaml
author: rye-os
author: security-team
```

### `created_at`

**Type:** ISO 8601 datetime string
**Required:** Yes

When the entry was created.

```yaml
created_at: 2026-02-18T00:00:00Z
created_at: 2025-12-15T10:30:00Z
```

---

## Frontmatter Fields — Optional

### `description`

**Type:** string
**Purpose:** Brief summary of what this knowledge covers.

```yaml
description: "Complete specification of metadata fields for directives"
```

### `entry_type`

**Type:** string
**Purpose:** Classification of content.

```yaml
entry_type: reference
entry_type: learning
entry_type: pattern
```

See [Entry Types](#entry-types) below for when to use each.

### `tags`

**Type:** list of strings
**Purpose:** Searchable tags for discovery. 3–5 recommended.

```yaml
tags:
  - authentication
  - oauth2
  - security
```

**Rules:**
- Lowercase, kebab-case
- Include both domain and content type
- 3–5 tags per entry

Both formats are valid:

```yaml
tags: [authentication, oauth2, security]    # Inline
tags:                                        # Block
  - authentication
  - oauth2
  - security
```

### `validated`

**Type:** ISO 8601 datetime
**Purpose:** Last validation/review timestamp.

```yaml
validated: 2026-02-10T00:00:00Z
```

### `references`

**Type:** list of knowledge IDs or external URLs
**Purpose:** Link to related or referenced knowledge. Part of the outbound knowledge graph.

```yaml
references:
  - oauth-overview                           # Internal knowledge ID
  - token-validation                         # Internal knowledge ID
  - "https://oauth.net/2/"                   # External URL
  - "https://tools.ietf.org/html/rfc6749"    # External URL
  - "docs/authoring/directives.md"           # Relative doc path
```

**Format:**
- Internal: knowledge ID (no prefix), bare string
- External: full URL starting with `http://` or `https://`
- Relative: path starting with a directory name (e.g., `docs/...`)

### `extends`

**Type:** list of knowledge IDs
**Purpose:** Declare what knowledge this entry builds upon. Creates an inheritance/dependency chain.

```yaml
extends:
  - authentication-basics
  - http-security
```

### `used_by`

**Type:** list of directive or tool IDs
**Purpose:** Track where this knowledge is applied. Helps understand impact of changes.

```yaml
used_by:
  - setup-oauth-provider
  - secure-api-endpoint
```

---

## Entry Types

| Type | Purpose | When to Use | Typical Size |
|------|---------|-------------|-------------|
| `reference` | Stable documentation | Specs, API references, conventions that rarely change | 200–600 lines |
| `learning` | From experience | Insights discovered during execution, debugging findings | 50–200 lines |
| `pattern` | Reusable approaches | Design patterns, architectural decisions, best practices | 100–400 lines |

### `reference`

Canonical specifications, format definitions, API documentation. Stable, comprehensive, rarely changes.

```yaml
entry_type: reference
# Examples: directive-format, tool-metadata-reference, terminology
```

### `learning`

Insights captured during execution. May evolve or become obsolete. Document the context and the insight.

```yaml
entry_type: learning
# Examples: debugging-xml-parser, migration-gotchas
```

### `pattern`

Reusable approaches and architectural decisions. Prescriptive — tells agents how to approach a category of problem.

```yaml
entry_type: pattern
# Examples: retry-with-backoff, file-path-security, error-response-format
```

---

## Knowledge Graph

Knowledge entries form a navigable graph through explicit link relationships in the frontmatter. No external system needed — the graph is fully derivable from metadata.

### Navigation Directions

| Relationship | Direction | Purpose |
|-------------|-----------|---------|
| `extends` | ↑ Upward | Navigate to foundational concepts |
| `references` | ↔ Lateral | Navigate to related knowledge |
| `used_by` | ← Inbound | Trace from directives/tools that depend on this |
| **Backlinks** (derived) | ← Inbound | Auto-generated from other entries' `references` and `extends` |

### Example Graph Cluster

```yaml
# Entry: jwt-token-validation
references:
  - jwt-overview
  - cryptographic-algorithms
  - "https://tools.ietf.org/html/rfc7519"
extends:
  - authentication-basics
  - cryptographic-signatures
used_by:
  - api-authentication
  - service-authorization
```

The three `*-metadata-reference` entries cross-reference each other, forming a navigable cluster:

```
directive-metadata-reference ←→ tool-metadata-reference
         ↕                              ↕
      knowledge-metadata-reference
```

---

## Body Conventions

### Structure

```markdown
# Entry Title

One-line summary.

## Section Heading

Content organized with tables, code blocks, and rules.

### Subsection

More detail.
```

### Content Guidelines

| Element | When to Use |
|---------|------------|
| **Tables** | Field specifications, comparison data, quick-reference |
| **Code blocks** | Examples, templates, syntax |
| **Bullet lists** | Rules, guidelines, enumeration |
| **Headings** | Logical sections (## for major, ### for sub) |

### What Makes Good Knowledge

- **Dense and scannable** — tables and code over prose
- **One topic per entry** — split if exceeds ~2000 words
- **Actionable** — include code samples, examples, templates
- **Plain language** — agents read this as context; avoid unnecessary jargon
- **Concrete** — specific rules over vague guidelines

---

## Size Guidelines

| Entry Type | Target Lines | Notes |
|-----------|-------------|-------|
| `reference` | 200–600 | Comprehensive specs, complete field definitions |
| `learning` | 50–200 | Focused insight with context |
| `pattern` | 100–400 | Approach + examples + when-to-use |

If an entry exceeds ~600 lines, consider splitting into multiple focused entries linked via `references`.

---

## Loading Knowledge

### Via `rye_load`

Knowledge is loaded and injected into agent context:

```python
rye_load(item_type="knowledge", item_id="rye/core/terminology")
# Returns the markdown body as context
```

### Via Thread Hooks

Automatically loaded when a thread starts:

```xml
<hooks>
  <hook>
    <when>thread_started</when>
    <execute item_type="knowledge">rye/core/terminology</execute>
  </hook>
</hooks>
```

### Via Directive Process Steps

```xml
<step name="load_context">
  Load the format specification for reference.
  `rye_load(item_type="knowledge", item_id="rye/authoring/directive-format")`
</step>
```

---

## File Resolution

Knowledge entries resolve by `item_id` to file path:

```
item_id: "rye/core/terminology"
  → .ai/knowledge/rye/core/terminology.md

item_id: "security/jwt-validation"
  → .ai/knowledge/security/jwt-validation.md

item_id: "rye/authoring/directive-format"
  → .ai/knowledge/rye/authoring/directive-format.md
```

The `category` determines the directory path within `.ai/knowledge/`. Knowledge can also be YAML files (`.yaml`/`.yml` extension) instead of markdown.

---

## Signature

Same format as directives and tools:

```
<!-- rye:signed:TIMESTAMP:HASH:SIGNATURE:KEYID -->
```

- Line 1 of the file (before frontmatter)
- Added by `rye_sign` — never written manually
- Unsigned placeholder: `<!-- rye:signed:TIMESTAMP:placeholder:unsigned:unsigned -->`

---

## Validation Rules

1. **Required fields:** `name`, `title`, `category`, `version`, `author`, `created_at`
2. `name` must be kebab-case alphanumeric
3. `category` must match the file path relative to `.ai/knowledge/` directory
4. `version` must be semantic version (`X.Y.Z`)
5. `created_at` must be ISO 8601 format
6. `references` entries must be knowledge IDs (bare strings) or full URLs (`http://`/`https://`) or relative doc paths
7. `extends` entries must be knowledge IDs
8. `used_by` entries must be directive or tool IDs
9. `tags` should be lowercase kebab-case
10. `entry_type` should be one of: `reference`, `learning`, `pattern`

---

## Complete Example

````markdown
<!-- rye:signed:2026-02-18T00:00:00Z:abc123:sig456:keyid789 -->

```yaml
name: jwt-token-validation
title: JWT Token Validation Patterns
entry_type: pattern
category: security/authentication
version: "2.1.0"
author: security-team
created_at: 2025-10-01T08:00:00Z
tags:
  - jwt
  - tokens
  - validation
  - security
references:
  - jwt-overview
  - cryptographic-algorithms
  - "https://tools.ietf.org/html/rfc7519"
extends:
  - authentication-basics
  - cryptographic-signatures
used_by:
  - api-authentication
  - service-authorization
```

# JWT Token Validation Patterns

Best practices for validating JWT tokens in production systems.

## Key Rules

- Token signature verified with public key
- Expiration time always validated
- Algorithm whitelist enforced
- Claims validated according to spec
- None algorithm never accepted

## Common Mistakes

| Mistake | Impact | Fix |
|---------|--------|-----|
| Trusting unverified tokens | Auth bypass | Always verify signature |
| Accepting None algorithm | Auth bypass | Whitelist algorithms |
| Not checking expiration | Stale sessions | Validate `exp` claim |
| Weak key management | Key compromise | Use HSM or vault |

## Python Example

```python
import jwt

payload = jwt.decode(
    token,
    public_key,
    algorithms=['RS256'],
    audience='api:prod',
    issuer='https://auth.example.com'
)
```
````

---

## Best Practices

### Writing
- **Focused scope** — one topic per entry; split if exceeds ~2000 words
- **Kebab-case names** — `data-driven-architecture`, not `data_driven_architecture`
- **3–5 tags** — include both domain and content type
- **Use `references`** — link to related knowledge to build the graph
- **Include examples** — code samples, tables, diagrams make knowledge actionable
- **Version on content changes** — bump version when body changes, not metadata tweaks
- **Plain language** — agents read this as context; avoid unnecessary jargon

### Graph Organization
- Use `extends` to declare dependencies on foundational knowledge
- Use `references` to link to related knowledge or external sources
- Use `used_by` to document which directives/tools apply this knowledge
- Backlinks are automatically derived — no need to maintain manually
- Cross-reference sibling entries to form navigable clusters

### Metadata
- Keep `tags` focused (3–5 items)
- Use kebab-case for all names
- Always include `created_at`
- Set `entry_type` explicitly
- Category must match directory structure

### Creating via Directive

```python
rye_execute(
    item_type="directive",
    item_id="rye/core/create_knowledge",
    parameters={
        "name": "jwt-validation",
        "title": "JWT Validation Patterns",
        "category": "security/authentication",
        "content": "Best practices for validating JWT tokens...",
        "tags": "jwt, tokens, security"
    }
)
```
