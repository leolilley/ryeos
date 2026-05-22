<!-- ryeos:signed:2026-05-22T03:35:36Z:b3362d7cfc0cddad89fdf34aba4a0b6f64c663292d9c183f2e0220b252e12fc6:3W4kwVhUsNla0NUSs8E1cEeeGVuIGG8IGLBu0Z3MfDOhuaQuwvQdUNadB30QOUR9RwbVFcJ5qkoKforYmfGrDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
