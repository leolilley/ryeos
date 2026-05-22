<!-- ryeos:signed:2026-05-22T07:21:27Z:4627e2b6edfa237dc6a65b02fc09736d33b626d785cdf6b7144139499e444b87:LnEjmZW8R6OOXEtiTfjHVfQterUFErevdZCbQ+4MHb1IQBK54hmPfEUCu43B2KVqbsvFpot8z5XgLQxEbNOqCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, graph, dag, callbacks]
version: "1.0.0"
description: Graph runtime reference.
---

# Runtime: graph-runtime

Invariant: graph-runtime executes graph DAG/state-machine records and delegates node actions back through the daemon callback channel.

It handles node ordering, conditional edges, foreach expansion, state persistence, transcript logging, and callback dispatch. Callback children borrow parent execution provenance and must not own pushed-head snapshot lifecycle.
