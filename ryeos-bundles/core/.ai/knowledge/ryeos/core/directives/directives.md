---
category: ryeos/core
tags: [fundamentals, directives, workflows, prompts]
version: "1.0.0"
description: >
  How directives work — YAML frontmatter, inheritance (extends),
  permissions, limits, context blocks, and actions.
---

# Directives

Directives are the primary LLM-facing item in Rye OS. A directive is a
markdown file that combines a structured YAML header with a prompt body,
defining a complete workflow for an LLM agent.

## File Format

```markdown
---
name: deploy
description: Deploy the project to staging
extends: "directive:base/workflow"
model:
  tier: high
permissions:
  execute:
    - ryeos.execute.tool.ryeos.file-system.*
limits:
  turns: 10
  tokens: 8000
  spend_usd: 0.50
  duration_seconds: 300
context:
  - position: system
    ref: "knowledge:ryeos/core/signing"
  - position: system
    content: "Project uses pnpm and deploys to AWS."
inputs:
  - name: environment
    type: string
    required: true
outputs:
  - name: result
    type: string
actions:
  execute:
    item_id: "tool:my/project/deploy"
  fetch:
    item_id: "tool:ryeos/core/fetch"
  sign:
    item_id: "tool:ryeos/core/sign"
---

You are deploying the project. Follow these steps:
...
```

## Frontmatter Fields

### Identity
- `name` — unique directive name
- `description` — what this directive does
- `extends` — optional parent directive canonical ref

### Model Selection
- `model.tier` — abstract capability tier: `fast`, `general`, `high`,
  `orchestrator`, `max`, `code`, `code_max`, `cheap`, `free`
- `model.name` — explicit model name (overrides tier)
- `model.context_window` — override context window size

### Permissions
- `permissions.execute` — list of capability strings required.
  Uses dot-namespaced glob patterns:
  - `["ryeos.execute.tool.ryeos.file-system.*"]` — all FS tools
  - `["ryeos.execute.service.fetch"]` — just the fetch service
  - `[]` — no tool execution (read-only directive)

Permissions **narrow** through extends chains — a child can only reduce
the parent's permissions, never expand them.

### Limits
- `limits.turns` — max LLM round-trips
- `limits.tokens` — max total tokens
- `limits.spend_usd` — max spend in USD
- `limits.duration_seconds` — wall-clock timeout

### Context
Context blocks are injected into the LLM prompt:

```yaml
context:
  - position: system
    ref: "knowledge:ryeos/core/signing"      # knowledge entry
  - position: system
    content: "Inline text content"            # literal content
  - position: user
    ref: "knowledge:project/context"          # in user position
```

Context merges through extends chains using
`dict_merge_string_seq_root_last` — the child's context entries
are appended after the parent's.

### Inputs and Outputs
Typed parameters and return values:

```yaml
inputs:
  - name: target
    type: string
    required: true
  - name: dry_run
    type: boolean
    default: false
outputs:
  - name: result
    type: string
```

### Actions
Named operations the directive can invoke:

```yaml
actions:
  execute:
    item_id: "tool:ryeos/core/subprocess/execute"
  fetch:
    item_id: "tool:ryeos/core/fetch"
  sign:
    item_id: "tool:ryeos/core/sign"
```

## Inheritance (Extends)

Directives support single inheritance via `extends`:

```yaml
extends: "directive:base/workflow"
```

The extends-chain composer resolves the full chain (root → ... → child)
and merges fields with declared strategies:

| Field          | Strategy                          |
|----------------|-----------------------------------|
| `body`         | `root_verbatim` — child replaces parent body |
| `permissions`  | `narrow_against_parent_effective` — child ⊆ parent |
| `context`      | `dict_merge_string_seq_root_last` — child appended |

This means:
- A child directive always overrides the prompt body
- A child can never gain more permissions than its parent
- Context accumulates: parent context + child context

## Execution Lifecycle

1. **Resolve** — canonical ref → file path → parsed metadata
2. **Compose** — extends chain resolved, fields merged
3. **Launch** — directive-runtime subprocess spawned with:
   - Composed prompt body
   - Context blocks assembled into system/user positions
   - Input values interpolated
   - Permission caps set
4. **Run** — LLM loop with tool dispatch (up to `limits.turns`)
5. **Complete** — result captured, thread finalized
