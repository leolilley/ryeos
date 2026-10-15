<!-- ryeos:signed:2026-10-15T23:05:10Z:185f375deaef6eb33393d62e68898e29a104b2fd33ed9e91ba753d6e4182f667:z8spAIN3EudDYfrkUF8fGzzzWFcQ63pARtM9xEI16dC/sd//lJtK8bryCc4Bg8pU2FX+jvKikwewLnSzHI/hBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, threads, lifecycle]
version: "1.0.0"
description: Thread query service reference.
---

# Services: threads

Invariant: thread query services expose persisted execution state without directly controlling subprocesses.

Services: `threads/children` and `threads/chain` (shipped by the standard bundle), plus `threads/list` and `threads/get` (shipped by the core bundle so the core `/threads` routes work on a core-only node). Thread cancellation is route-backed by a core service descriptor because the public HTTP cancellation route is a daemon control-plane surface.

Normal child lineage is projected from `upstream_thread_id` and runtime spawn
events. Trace branches are different: `trace.branch` deliberately creates a
child thread with no `upstream_thread_id` and records the relation as
`edge_recorded { relation: "trace_branch" }`. Use `trace.inspect` or replay
the branch provenance event when the question is "what trace point did this
branch come from?", not `threads/children`.
