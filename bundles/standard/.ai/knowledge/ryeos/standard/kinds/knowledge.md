<!-- ryeos:signed:2026-07-14T10:12:30Z:83ece88929e572229703a85518c44e66bb3ed1e144e1ccc62902f39b56102e8b:fr4gfNb6uw5lMt4WkYNKRRtjTXfnazYjyWoczvnPi+pPu/JhLCfZ3gHf5QjjHZjW8IGDAbr6p1oC1a6ZfArAAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- Generic methods: `compose`, `query`, `graph`, and `validate`
- Private launch augmentation operation: `compose_positions`
- Runtime implementation: runtime-registry selection of `runtime:knowledge-runtime`
- Method wire: kind-schema selection of `protocol:ryeos/core/method_runtime_v1`

Directives use `compose_positions` through the daemon-owned
`compose_context_positions` launch augmentation to render `system`, `before`,
and `after` context blocks within per-position budgets. It is intentionally not
available as a generic `call.method`.
Direct runtime-item launch is rejected: knowledge-runtime consumes the method
wire, not the ordinary `runtime_v1` launch envelope.
