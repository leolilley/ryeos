---
category: ryeos/core/services
tags: [service, objects, cas]
version: "1.0.0"
description: CAS object service reference.
---

# Services: objects

Invariant: object services expose explicit CAS get/put/has operations and fail closed when requested hashes are absent.

- `objects/has` checks whether a hash exists.
- `objects/get` returns object bytes/metadata for a hash.
- `objects/put` writes supplied content into CAS.

Remote transfer and pushed-head execution depend on these services for content-addressed synchronization.
