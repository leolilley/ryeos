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
