---
category: ryeos/core/services
tags: [service, maintenance, gc, cas]
version: "1.0.0"
description: Maintenance GC service reference.
---

# Service: maintenance/gc

Invariant: maintenance GC reclaims unreachable CAS state according to daemon policy, with dry-run and compact modes for safe operation.

Run GC as a maintenance task, not during request-critical paths.
