---
tags: [fundamentals, directives, workflows, prompts]
version: "2.0.0"
description: >
  How directives work — YAML frontmatter, XML process body,
  inheritance (extends), permissions, limits, and context blocks.
---

# Directives

Directives are the primary LLM-facing item in Rye OS. A directive is a
markdown file with a YAML frontmatter header (metadata) and an XML process
body (instructions for the LLM to follow).

## File Format

```markdown
---
description: "Deploy the project to staging"
version: "1.0.0"
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
---

<process>
  <step name="validate">
    <instruction>
      Validate that {input:environment} is one of: staging, production.
    </instruction>
  </step>

  <step name="deploy">
    <instruction>
      Deploy the project:
      `rye_execute(item_id="tool:my/project/deploy", parameters={"env": "{input:environment}"})`
    </instruction>
  </step>

  <step name="confirm">
    <render>
    Deployed to {input:environment} successfully.
    </render>
  </step>
</process>
```

The `name` and `category` fields are NOT in the frontmatter — they are
derived automatically from the file path:
- `name` comes from the filename (e.g., `deploy.md` → name: `deploy`)
- `category` comes from the parent directory (e.g., `my-project/deploy.md` → category: `my-project`)

## Body: XML Process Tags

The body uses structured XML tags to give the LLM clear, parseable instructions:

- `<process>` — top-level container for all steps
- `<step name="...">` — named execution step, optional `condition` attribute
- `<instruction>` — tells the LLM what to do (followed silently)
- `<render>` — text output verbatim to the user (not interpreted by the LLM)
- `<Identity>` — establishes the LLM's persona (placed before `<process>`)

Rules:
- Output `<render>` blocks verbatim — do not summarize or rephrase
- Follow `<instruction>` blocks silently — do not narrate the thinking
- Steps run in order unless a `condition` is specified
- Use `{input:name}` for input interpolation, `{env:VAR}` for environment variables

## Frontmatter Fields

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
Context blocks inject knowledge into the LLM prompt:

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
