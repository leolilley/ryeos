<!-- ryeos:signed:2026-05-22T07:21:24Z:8bb396710100bf466f985bb7c902dd2e34a619862999c9bdd34080862f912ec4:Kf0yzASjeQp1jrmJ2xUUp56dP6ihYPGYjJMoy/j0M33/pCjCWkZxibFKdPbIAELQmYN5lIoMWJ+VObSsmp9WDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/services
tags: [service, fetch, resolution]
version: "1.0.0"
description: Fetch service reference.
---

# Service: fetch

Invariant: `service:fetch` resolves an item through the engine and returns metadata/content without executing it.

It is the daemon-side endpoint behind CLI/MCP fetch operations. It can be used with verification to inspect signed items safely.
