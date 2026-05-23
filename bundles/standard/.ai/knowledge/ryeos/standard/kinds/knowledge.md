<!-- ryeos:signed:2026-05-22T19:55:06Z:9ffcb5728f7abe0dba34fd257b563307ce2ab30d294bca03efbeb1c9489c8b8d:C+AOSKY5I7nP/i5EE2hevK94etG9r3rH5kpR+gV4f19dZRx314F511228fKlZwBJBwHVSfLYwk7dVz3+vM+rDw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
