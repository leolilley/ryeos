<!-- ryeos:signed:2026-08-11T02:28:39Z:e5b415827bec9d106a97374bf728237c82526d80e8f6c5936b9ffd69ca2722ae:52WedYzTND8hxRix0dLr1dBkhHF+WtQ3Rl3EP5tQFvw4I8ZZ0DfqGGqLY2vB9cqDkcmkukOSgH1/tw2ifqy6BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/kinds
tags: [kind, graph, workflow, dag]
version: "1.0.0"
description: Graph kind reference.
---

# Kind: graph

Invariant: graphs are YAML workflow state machines whose complete finalized
composed value is validated before it is sealed and delegated to the graph
runtime.

- Directory: `graphs/`
- Format: `.yaml`, `.yml` via `parser:ryeos/core/yaml/yaml`
- Composer: `handler:ryeos/core/extends-chain`
- Effective validator: `handler:ryeos/core/graph-effective-validator`
- Execution: runtime-registry delegate to `runtime:graph-runtime`
- Resolution: extends-chain step

The graph kind keeps `resolve_extends_chain`. `version` and `category` are
root-required. `config` is merged shallowly from deepest ancestor to root:
omitted keys inherit and every declared key replaces its complete inherited
value. `requires.capabilities` narrows at every direct edge. This prevents an
implicit node-by-node hybrid while still allowing a child to inherit complete
topology and replace selected graph settings.

Before a token, capsule, or runtime exists, the effective validator proves
start/edge consistency, expression and retry validity, captured hook-plan
parity, and equality between declared capabilities and `effective_caps`. The
runtime recomputes `effective_definition_digest` and executes
`resolution.composed.composed`; `root.raw_content` is source evidence only.
