<!-- ryeos:signed:2026-07-14T01:54:46Z:e50c989e1a174a362a03211d402e1eabe57fc10488c2fdd2efaef30a28417d55:lhuO0H6ukzCjx0qoHyjDbQfiZiYTtjlokhf/Zm0JTFrbvYCJBqg+Kvxac4Zk/b1BS2eYcsT9hm/UO4ZfOKlPAA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

See [Execution Sandbox](../node/execution-sandbox.md) for snapshot and restart
semantics.
