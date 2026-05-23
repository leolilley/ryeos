<!-- ryeos:signed:2026-05-22T19:55:06Z:b38d7ebb70174ce820e8b64ea68228f84615f5d2a897b5835618d38d9cf4b912:d5k98G0M1qQ10yAeTC87Cv6ScjfbUe9OLnqevBEHsAMhT8AWvsKVFuZW2VRoighduHPxKIxAyuuVYw0MPpIvAw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
