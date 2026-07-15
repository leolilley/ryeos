<!-- ryeos:signed:2026-07-14T23:18:43Z:af33c8d652281da34b4fa042dcdbf11f790b62eb25df3fa093ba75d53a830f1f:lUlblOz5wWFINE2AXnSLy3XFY45StHMTXiY9NJjGTWvCURL+e5/qDgSmmLEpXfwUL/F3kASTCmzXYLIH3co3Ag==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, objects, cas]
version: "1.0.0"
description: CAS object service reference.
---

# Services: objects

Invariant: object services expose explicit CAS get/put/has operations and fail closed when requested hashes are absent.

- `objects/has` checks object and blob namespaces independently. Requests and
  responses preserve the required CAS kind even when both namespaces contain
  the same digest.
- `objects/get` returns object bytes/metadata for a hash.
- `objects/put` writes supplied content into CAS.

Remote transfer and pushed-head execution depend on these services for content-addressed synchronization.
