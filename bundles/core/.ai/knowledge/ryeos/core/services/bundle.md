<!-- ryeos:signed:2026-05-22T04:30:07Z:b598afb454d29c1003af2248623e1c905d7bbc63a95bf56789ddb944030911a5:ipcZyGnN5gSCRNrP0MIolg9ZwO8XKQA8QmTC+X812WWAorO9tj3kCsOwcAPjPi6SKc4m72FqArUH73K46KWtCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
