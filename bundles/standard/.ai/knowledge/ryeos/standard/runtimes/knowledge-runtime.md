<!-- ryeos:signed:2026-05-22T07:21:27Z:90042447db75bf2b9d93cd4a6ef4ff2b600cf27f716d12d340005c4cdc069d0a:NDKOGoYQrfksatsYERrPLxrbzb441W4gmN9ihn9OkYB9U7+3ioqrfJoIXlvCoiPPVAoH1f2EY+ei3BBHdv8ABA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, knowledge, context]
version: "1.0.0"
description: Knowledge runtime reference.
---

# Runtime: knowledge-runtime

Invariant: knowledge-runtime composes knowledge entries into bounded context payloads for directives and explicit knowledge operations.

It serves the knowledge kind's `compose` and `compose_positions` operations, applies token budgets and exclusions, and writes projection/chain data as required by the operation side-effect declaration.
