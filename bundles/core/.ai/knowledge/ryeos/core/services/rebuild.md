---
category: ryeos/core/services
tags: [service, rebuild, projection]
version: "1.0.0"
description: Rebuild service reference.
---

# Service: rebuild

Invariant: rebuild reconstructs daemon projection state from CAS and signed registrations, and is an offline maintenance operation.

Use it after state corruption or migration when the append-only sources remain authoritative.
