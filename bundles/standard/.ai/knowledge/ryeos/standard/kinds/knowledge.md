---
category: ryeos/standard/kinds
tags: [kind, knowledge, context]
version: "1.0.0"
description: Knowledge kind reference.
---

# Kind: knowledge

Invariant: knowledge items are context records with operation-based execution for composing token-budgeted context.

- Directory: `knowledge/`
- Formats: markdown frontmatter or YAML
- Composer: identity
- Operations: `compose` and `compose_positions`
- Runtime: runtime-registry delegate to `runtime:knowledge-runtime`

Directives use `compose_positions` as a launch augmentation to render `system`, `before`, and `after` context blocks within per-position budgets.
