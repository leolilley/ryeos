<!-- ryeos:signed:2026-05-23T09:45:40Z:bd9cd21a5cfd252bdd94178f48538c05f4c99be33e973f89745882a1c2a1676e:TpnM6KSyczDYaLT03JwTTbaqLfQeCN2KgwsENKKMzG+IfZaHMw+7GRbs/oHqiZqu23b3HDyWDKhRCrlFsSQQCA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
category: ryeos/standard/graphs
tags: [graph, authoring, dag, workflow]
version: "1.0.0"
description: How to author graph workflow YAML.
---

# Graph Authoring

Invariant: a graph is signed YAML describing explicit nodes, edges, conditions, and permissions for daemon-mediated action callbacks.

## Authoring checklist

- Declare `category`, `version`, and a clear description.
- Declare `permissions` for every daemon callback the graph may perform.
- Keep node ids stable; persisted state and events refer to them.
- Use conditional edges for branching and foreach blocks for fan-out.
- Prefer explicit error edges/hooks over relying on runtime defaults.
- Keep side-effecting nodes isolated so resume/retry behavior is understandable.

The graph kind delegates to `runtime:graph-runtime` through the runtime registry.
