<!-- ryeos:signed:2026-05-23T12:11:51Z:b38d7ebb70174ce820e8b64ea68228f84615f5d2a897b5835618d38d9cf4b912:+kAgYks/3LC+1UgBXmMFKz9wLBCcnZLeNxXiK2mnvCnO/N62xMTk6ns3XPPp/+SBGuqAXkOT2Vf0+DVh6+L3BQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/kinds
tags: [kind, graph, workflow, dag]
version: "1.0.0"
description: Graph kind reference.
---

# Kind: graph

Invariant: graphs are YAML workflow state machines that delegate execution to the graph runtime and lift graph permissions into callback policy facts.

- Directory: `graphs/`
- Format: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/graph-permissions`
- Execution: runtime-registry delegate to `runtime:graph-runtime`
- Resolution: extends-chain step

Graph runtime actions dispatch back through the daemon, so graph permissions must be reflected in `effective_caps` before callback tokens are minted.
