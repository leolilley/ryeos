---
category: ryeos/core
tags: [fundamentals, knowledge, composition, context]
version: "1.0.0"
description: >
  How knowledge entries are composed into LLM context blocks —
  the compose operation, token budgets, position-based injection,
  and exclude filters.
---

# Knowledge Composition

Knowledge entries are not directly executed — they are **composed**
into context blocks that get injected into LLM prompts. The compose
operation assembles relevant knowledge within a token budget.

## Two Operations

The knowledge kind supports two named operations:

### `compose` (default)
Assemble knowledge entries into a single context block.

**Parameters:**
| Parameter        | Type     | Default | Description                    |
|------------------|----------|---------|--------------------------------|
| `token_budget`   | integer  | 4000    | Max tokens for the composed block |
| `exclude_refs`   | string[] | []      | Knowledge refs to exclude      |

**Process:**
1. Collect candidate knowledge entries from the context specification
2. Exclude any refs in `exclude_refs`
3. Sort by relevance (position order, then ref order)
4. Accumulate entries until `token_budget` is reached
5. Return the composed text block

### `compose_positions`
Compose knowledge at specific prompt positions with per-position
budgets.

**Parameters:**
| Parameter             | Type                | Description                        |
|-----------------------|---------------------|------------------------------------|
| `roots_by_position`   | map<string, refs[]> | Knowledge refs grouped by position |
| `per_position_budget` | map<string, int>    | Token budget per position          |

**Process:**
1. For each position (e.g., `system`, `user`, `after_body`):
   - Collect the refs at that position
   - Compose within that position's budget
2. Return a map of position → composed text

## Context Injection in Directives

Directives specify knowledge context in frontmatter:

```yaml
context:
  - position: system
    ref: "knowledge:ryeos/core/signing"
  - position: system
    content: "Inline text injected at system position"
  - position: user
    ref: "knowledge:project/api-docs"
```

During directive launch:
1. The engine pre-resolves all `ref:` entries via `compose_context_positions`
2. Knowledge content is fetched and assembled
3. The composed blocks are placed at their declared positions in the prompt
4. The assembled prompt is sent to the LLM

## Token Budget

Token budgets prevent context overflow. When composing:
- Each knowledge entry's content is token-counted
- Entries are added in order until the budget is exhausted
- Partial entries are truncated with an ellipsis marker

The directive's `limits.tokens` is the hard cap. Context blocks must
fit within this budget alongside the prompt body and conversation
history.

## Knowledge Entry Format

Knowledge entries can be:

### Markdown (`.md`)
```markdown
---
category: ryeos/core
tags: [signing, trust]
version: "1.0.0"
description: "Brief description for catalog"
---

# Title

Content body...
```

Frontmatter is optional. The body is the composed content.

### YAML (`.yaml`)
```yaml
category: ryeos/core
tags: [signing, trust]
version: "1.0.0"
description: "Brief description for catalog"
data:
  key: value
  nested:
    - item1
    - item2
```

YAML knowledge entries provide structured data rather than prose.
The entire document is available as composed context.

## Knowledge in Extends Chains

Context blocks merge through directive extends chains:
- Parent context entries come first
- Child context entries are appended after
- Merge strategy: `dict_merge_string_seq_root_last`

This means a child directive always inherits its parent's knowledge
context, and can add more entries on top.
