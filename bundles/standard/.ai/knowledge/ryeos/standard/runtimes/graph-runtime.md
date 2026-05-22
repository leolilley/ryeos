---
category: ryeos/standard/runtimes
tags: [runtime, graph, dag, callbacks]
version: "1.0.0"
description: Graph runtime reference.
---

# Runtime: graph-runtime

Invariant: graph-runtime executes graph DAG/state-machine records and delegates node actions back through the daemon callback channel.

It handles node ordering, conditional edges, foreach expansion, state persistence, transcript logging, and callback dispatch. Callback children borrow parent execution provenance and must not own pushed-head snapshot lifecycle.
