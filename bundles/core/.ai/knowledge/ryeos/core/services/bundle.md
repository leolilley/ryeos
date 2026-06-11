<!-- ryeos:signed:2026-06-11T05:13:18Z:fbadb31125d8201ce9bef465cc5dfd6e17bc1eb8b96a31ac2e591b93c6406bba:ADE+LC07KpTihxfqCoEegE+TQgvRLyiOIW9eD3yl0sUN6bxGVLqHIVLqMQghwhuFHJ3tjC/A4LJC032a5kpXBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, bundle, install, export]
version: "1.0.0"
description: Bundle service reference.
---

# Services: bundle

Invariant: bundle services manage installed bundle registrations and bundle transfer without executing arbitrary workflow logic.

- `bundle/install` — install a bundle; offline-only; requires `ryeos.execute.service.bundle/install`.
- `bundle/list` — list installed bundles; unauthenticated capability requirement is none.
- `bundle/remove` — remove an installed bundle; offline-only; requires remove capability.
- `bundle/export` — daemon-side export of bundle CAS objects for transfer.

Install/remove are offline to avoid mutating the engine registry while the daemon is serving requests.
