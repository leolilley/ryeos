<!-- ryeos:signed:2026-07-16T02:18:48Z:b70ba7144224effad8994bb75c220065914fba4a44a10f918f6eee8a2440590b:zM+/gaXJO6szmBtsn4q+RczIKq7mqqa9YdmvGdWwcJt3Bk6NTT0j8nBa6S/bihcVDGv2yk22re7xiupZlZd1Cg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, health, identity, status]
version: "1.1.0"
description: Health, identity, and status service reference.
---

# Services: health, identity, status

Invariant: health and public identity surfaces are safe unauthenticated reads; status reports daemon state without mutating execution state.

- `health/status` backs health checks.
- `identity/public_key` returns the node public identity document.
- `node/status` returns daemon/node status, including the immutable sandbox
  snapshot's mode, schema version, source path, and policy digest.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.

See [Execution Isolation](../node/execution-isolation.md) for snapshot and restart
semantics.
