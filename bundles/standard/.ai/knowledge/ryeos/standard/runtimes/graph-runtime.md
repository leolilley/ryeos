<!-- ryeos:signed:2026-05-22T19:55:06Z:4627e2b6edfa237dc6a65b02fc09736d33b626d785cdf6b7144139499e444b87:yEIJlDrkg2gS/0LtWtoQ8mgEmzLZJSePmfDeycDU3hXHeNNRKiITx4zr3Ug9gIpV374miL8x2Q9PaNtepALOCQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/runtimes
tags: [runtime, graph, dag, callbacks]
version: "1.0.0"
description: Graph runtime reference.
---

# Runtime: graph-runtime

Invariant: graph-runtime executes graph DAG/state-machine records and delegates node actions back through the daemon callback channel.

It handles node ordering, conditional edges, foreach expansion, state persistence, transcript logging, and callback dispatch. Callback children borrow parent execution provenance and must not own pushed-head snapshot lifecycle.
