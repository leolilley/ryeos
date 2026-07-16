<!-- ryeos:signed:2026-07-16T04:18:05Z:0d4aa8023c179c136e820cc953c174e5b96b79b82eddbaeeb47be4324eb40725:+mnVLcnYZefESDEJRiCrAjq01XdN7GBa6Q+gXsYcNX1lHHpF4A1h6CZlRiFaLvytuEYYwxFntPusSksHujOxBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, health, identity, status]
version: "1.2.0"
description: Health, identity, and status service reference.
---

# Services: health, identity, status

Invariant: health and public identity surfaces are safe unauthenticated reads; status reports daemon state without mutating execution state.

- `health/status` backs health checks.
- `identity/public_key` returns the node public identity document.
- `node/status` returns daemon/node status, including the immutable isolation
  snapshot's mode, schema version, source path, policy digest, typed backend
  status, selected bundle/implementation, manifest and signer identities,
  adapter digest/build, declared and effective capabilities, and inspected
  payload digests.
- `system/ingest-ignore` and `system/push-head` support CAS ingestion and remote pushed-head workflows.

See [Execution Isolation](../node/execution-isolation.md) for snapshot and restart
semantics.
