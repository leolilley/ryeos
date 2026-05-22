---
category: ryeos/standard/runtimes
tags: [runtime, knowledge, context]
version: "1.0.0"
description: Knowledge runtime reference.
---

# Runtime: knowledge-runtime

Invariant: knowledge-runtime composes knowledge entries into bounded context payloads for directives and explicit knowledge operations.

It serves the knowledge kind's `compose` and `compose_positions` operations, applies token budgets and exclusions, and writes projection/chain data as required by the operation side-effect declaration.
