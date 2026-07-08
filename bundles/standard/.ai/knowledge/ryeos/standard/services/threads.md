<!-- ryeos:signed:2026-07-08T03:50:30Z:2df2af758dffc4d7e47f7fa96d0c028a1f580e2c929c6a325767c9a111952aee:wTouJk5Kw4cu2ML8C0atk/IbxTD5ZtjcVIdIKjp1fQKlr104CryqCfaAtU081SpXK2y+/RG8MnSSSgc7vz8QAw==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/standard/services
tags: [service, threads, lifecycle]
version: "1.0.0"
description: Thread query service reference.
---

# Services: threads

Invariant: thread query services expose persisted execution state without directly controlling subprocesses.

Services: `threads/list`, `threads/get`, `threads/children`, and `threads/chain`. Thread cancellation is route-backed by a core service descriptor because the public HTTP cancellation route is a daemon control-plane surface.

Normal child lineage is projected from `upstream_thread_id` and runtime spawn
events. Trace branches are different: `trace.branch` deliberately creates a
child thread with no `upstream_thread_id` and records the relation as
`edge_recorded { relation: "trace_branch" }`. Use `trace.inspect` or replay
the branch provenance event when the question is "what trace point did this
branch come from?", not `threads/children`.
