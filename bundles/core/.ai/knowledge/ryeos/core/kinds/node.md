<!-- ryeos:signed:2026-05-31T08:15:56Z:d0efff1602f944a0f6514319b9f2dad621766380c8dd8770a14af4b977eef36f:Q23MI+uJL3ELT1CkTey4Sb9HuuBgaf6hgdqlalOK5yNPq9OkWpWRkXmIM5zpKoaEceXpehSvGOrmLe2CUxqwCg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, node, bootstrap]
version: "1.0.0"
description: Node kind reference.
---

# Kind: node

Invariant: `node` items describe per-node sections and daemon bootstrap records; they are not directly executable.

- Directory: `node/`
- Formats: signed YAML
- Composer: identity
- Execution: none
- Sections: `verbs`, `aliases`, `routes`, `bundles`, `auth`, `identity`, `vault`, and `engine/kinds`

The bootstrap loader enforces the path/section invariant separately from the generic kind contract so errors point at the offending node section.
