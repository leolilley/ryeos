<!-- ryeos:signed:2026-07-09T01:21:44Z:f127b211a03cd022bb89b26de2b5dacbefd391e4c9a5c9f13dacfb022cf56916:RDcWWZcOEwt1V1wv1J6Xt2PQr2CRBYvGkow/iS6Dko3+dvy48hr5rivZgphePOakPvOlo1sdOIzp0fQFqCtjAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- `node/status` returns daemon/node status.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.
