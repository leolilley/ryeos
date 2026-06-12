<!-- ryeos:signed:2026-06-11T21:03:05Z:9ffcb5728f7abe0dba34fd257b563307ce2ab30d294bca03efbeb1c9489c8b8d:udMEilQB6vnrLpOifjyCy6Dfx4Z/2JuFiUw/5GXoL5sPgoi68h09JoNvoSOxz7fK0Vrt7GhGeycz6HBlSMq3CA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
