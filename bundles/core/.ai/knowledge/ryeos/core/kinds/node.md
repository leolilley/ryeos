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
