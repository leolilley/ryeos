<!-- ryeos:signed:2026-06-22T04:23:11Z:af1ce9bbe60072e94495f649a8c2b96b77ce43161f3ca8213334fe076b98aee1:ssVK0TeHWDPo2duXQ2UXDl9RIykU7td2bTFGzYzvxa5/x4MRZHeRKN9+4TXOIOamGwt77FNAJvz3XL0tY1FIAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/graphs
tags: [graph, authoring, dag, workflow]
version: "1.0.0"
description: How to author graph workflow YAML.
---

# Graph Authoring

Invariant: a graph is signed YAML describing explicit nodes, edges, conditions, and the capabilities it needs for daemon-mediated action callbacks.

## Authoring checklist

- Declare `category`, `version`, and a clear description.
- Declare `requires.capabilities.declared` (a flat list of caps) for every daemon action callback the graph may perform.
- Keep node ids stable; persisted state and events refer to them.
- Use conditional edges for branching and foreach blocks for fan-out.
- Prefer explicit error edges/hooks over relying on runtime defaults.
- Keep side-effecting nodes isolated so resume/retry behavior is understandable.

The graph kind delegates to `runtime:graph-runtime` through the runtime registry.
