---
category: ryeos/core/services
tags: [service, health, identity, status]
version: "1.0.0"
description: Health, identity, and status service reference.
---

# Services: health, identity, status

Invariant: health and public identity surfaces are safe unauthenticated reads; status reports daemon state without mutating execution state.

- `health/status` backs health checks.
- `identity/public_key` returns the node public identity document.
- `system/status` returns daemon/system status.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.
