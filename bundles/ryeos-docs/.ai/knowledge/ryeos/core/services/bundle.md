<!-- ryeos:signed:2026-08-17T23:06:00Z:f7e764a19029b237a6ffe61c0a599a379514aa405f362bad951ad309419b042e:9UmExYo5gO6goPssyNDuEJcQSlkbNc1KTX/cHK4RlhXAtKct9UdY6MZZ3PXnokt99dxhOyJTbZelPp/NLkCsBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, bundle, install, export]
version: "1.0.0"
description: Bundle service reference.
---

# Services: bundle

Invariant: bundle services manage installed bundle registrations and bundle transfer without executing arbitrary workflow logic.

- `bundle/install` — install a bundle; requires stopped-node authority and `ryeos.execute.service.bundle/install`.
- `bundle/list` — list installed bundles; unauthenticated capability requirement is none.
- `bundle/remove` — remove an installed bundle; requires stopped-node authority and the remove capability.
- `bundle/export` — daemon-side export of bundle CAS objects for transfer.

Install/remove require a stopped node to avoid mutating the engine registry while the daemon is serving requests.
