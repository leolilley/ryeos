---
category: ryeos/standard/services
tags: [service, threads, lifecycle]
version: "1.0.0"
description: Thread query service reference.
---

# Services: threads

Invariant: thread query services expose persisted execution state without directly controlling subprocesses.

Services: `threads/list`, `threads/get`, `threads/children`, and `threads/chain`. Thread cancellation is route-backed by a core service descriptor because the public HTTP cancellation route is a daemon control-plane surface.
