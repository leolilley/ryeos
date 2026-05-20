---
category: ryeos/core/services
tags: [service, remote, pushed-head, transfer]
version: "1.0.0"
description: Remote service reference.
---

# Services: remote

Invariant: remote services are daemon-only operations for cross-node configuration, transfer, execution, and thread/vault inspection.

The remote set includes configure/list/status, push/pull, execute, authorize, thread queries, bundle install, and vault proxy operations. Capability names are per-endpoint and are intentionally stricter for admin operations such as `remote.execute`.
