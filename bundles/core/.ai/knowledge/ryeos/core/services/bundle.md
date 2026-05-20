---
category: ryeos/core/services
tags: [service, bundle, install, export]
version: "1.0.0"
description: Bundle service reference.
---

# Services: bundle

Invariant: bundle services manage installed bundle registrations and bundle transfer without executing arbitrary workflow logic.

- `bundle/install` — install a bundle; offline-only; requires `ryeos.execute.service.bundle.install`.
- `bundle/list` — list installed bundles; unauthenticated capability requirement is none.
- `bundle/remove` — remove an installed bundle; offline-only; requires remove capability.
- `bundle/export` — daemon-side export of bundle CAS objects for transfer.

Install/remove are offline to avoid mutating the engine registry while the daemon is serving requests.
