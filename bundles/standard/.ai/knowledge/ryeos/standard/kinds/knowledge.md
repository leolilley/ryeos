<!-- ryeos:signed:2026-07-15T07:49:22Z:d808d585ca27bae245a711e04cdeb9c91f90cb0533c24e394e9791f2171a0afa:dtW0LpqUxXoZLPDYyl/n4HEoyQbBYPjysmMvhRltJ90BsnaXd/WIIGDRV7Sxp54mdLdnS3YLe4L8hvuQgSCbDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- Method wire: kind-schema selection of `protocol:ryeos/core/method_runtime`

Directives use `compose_positions` through the daemon-owned
`compose_context_positions` launch augmentation to render `system`, `before`,
and `after` context blocks within per-position budgets. It is intentionally not
available as a generic `call.method`.
Direct runtime-item launch is rejected: knowledge-runtime consumes the method
wire, not the ordinary `runtime` launch envelope.
