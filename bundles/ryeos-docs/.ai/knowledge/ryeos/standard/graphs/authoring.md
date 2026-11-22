<!-- ryeos:signed:2026-11-13T04:43:16Z:bf633c6b7ffabbd12085e2f4d5f6a3f6d527baa99f33327dabbafd3643a47353:lur6/PG005XqCBp8gtXLdM8CI5yLJ5v0LO3ige9PnDYvSIHM9dS9wpg8BZ/6A/vU1NVMNlIysf8PtyTDHQY6Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- Declare `requires.capabilities.declared` (a flat list of caps) for every daemon action callback the graph may perform. The caller launching the graph must hold every declared cap (`caller_capability_not_covered` otherwise); declaring never grants authority.
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
