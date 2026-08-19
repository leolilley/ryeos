<!-- ryeos:signed:2026-08-19T09:42:33Z:1529c615e1230c5fceb5553cf8c3db12086c73440cb0c0aed762c6083740df06:b5oLecx4UZo90lj7wSTfcaTcWv9LmbBwnG4Vd4UTtSaH5KW8jjRCfyqByzJHgZ2bfBR1Z8cgmbgQrBZVJSr5BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/graphs
tags: [graph, authoring, dag, workflow]
version: "1.1.0"
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
- When an action proposes or its node authors `project_observations`, give each
  claim a bounded namespaced `namespace`, a source-owned stable `stable_id`,
  and a bounded meaning-blind `payload`. RyeOS publishes it at the graph commit
  boundary; callers cannot assert the graph source identity. Prefer the
  node-authored form when the accepted action result already contains every
  claimed value; reserve hooks for observers that actually execute.

The graph kind delegates to `runtime:graph-runtime` through the runtime registry.
