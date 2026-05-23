<!-- ryeos:signed:2026-05-22T19:55:06Z:90042447db75bf2b9d93cd4a6ef4ff2b600cf27f716d12d340005c4cdc069d0a:KAynrjNpRVX1D0oG1aKhxgE/O/Tsh2i9uw6ECYxC8RYunXVUyvrE3VVx6X9D5akLnPSCDp6jW3+fhPXr/zkNAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/runtimes
tags: [runtime, knowledge, context]
version: "1.0.0"
description: Knowledge runtime reference.
---

# Runtime: knowledge-runtime

Invariant: knowledge-runtime composes knowledge entries into bounded context payloads for directives and explicit knowledge operations.

It serves the knowledge kind's `compose` and `compose_positions` operations, applies token budgets and exclusions, and writes projection/chain data as required by the operation side-effect declaration.
