<!-- ryeos:signed:2026-06-08T10:54:41Z:29394740d401f585efc51de1c65361acdb675b31b5ad10c7602aeb010e621e21:xnlRV3MljwjsalZx/v7GdUANHLVJkSyH/+bQ7WW5IyBK2APsyYDVjRv07zbng148gKUe8BSpPIfougkfF95rBQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
---
tags: [future, portable-execution, execution-graph, architecture]
version: "0.1.0"
status: deferred
description: >
  Deferred advanced path for portable execution graph projection and identity.
---

# Portable execution graph: deferred advanced path

This note captures the implementation boundary around RyeOS's portable
execution graph work. It is not a live API contract and should not be treated as
documentation for an implemented projection endpoint.

## Current implementation boundary

The current graph runtime hardens the identity bridge between an authored graph
definition and realized runtime consequences. It does not yet build a complete
portable execution graph projection.

Current stable bridge:

- `definition_ref`: conceptual authored definition ref, currently
  `graph:{graph_id}`.
- `definition_hash`: SHA-256 of the signature-stripped authored YAML bytes.
- `graph_run_id`: invocation/run instance identity.
- `node_ref`: `{definition_ref}#node:{node_name}` on node-scoped runtime
  events.
- `node_result_hash`: canonical JSON hash of a successful action node result.
- `graph_node_receipt` artifacts: daemon-compatible artifact wrappers whose
  `metadata` contains the node receipt payload and optional `node_result_hash`.
- Runtime events and node receipts: realized consequences linked back to
  definition identity.

Current regression proof:

- The graph runtime unit tests pin the receipt/event payload shape at the
  callback boundary.
- The daemon graph action E2E proves successful graph execution persists
  `graph_node_receipt` artifacts and runtime events carrying `definition_ref`,
  `definition_hash`, `graph_run_id`, and `node_ref`.
- The same E2E proves denied callback dispatch persists an error receipt and
  failure-path runtime events. The failed tool dispatch event uses
  `tool_call_result.status = "dispatch_failed"`; the graph step completion event
  uses `graph_step_completed.status = "error"`.
- `ryeos_app::graph_execution_projection::build_graph_execution_trace` now
  provides an internal, pure read-model primitive that groups persisted graph
  runtime events and `graph_node_receipt` artifacts by `node_ref`.
- This is still evidence for the identity bridge, not a public portable
  execution graph projection API.

`definition_hash` is exact document identity after signature-line stripping. It
is not semantic YAML canonicalization and is not by itself a trust,
authorization, safety, or policy decision.

## Deferred advanced model

A future portable execution graph projection may distinguish four layers.

### 1. Portable capability

The signed authored item that can be invoked:

- graph/workflow definitions;
- tools and command definitions;
- runtime descriptors;
- immutable content identity;
- signer/trust metadata;
- declared input, output, environment, and authority requirements.

### 2. Invocation instance

The specific execution run:

- thread id;
- graph run id or future execution run id;
- caller and authority context;
- runtime descriptor/version;
- input identity;
- workspace/source provenance.

### 3. Realized consequence

The facts produced by execution:

- runtime events;
- node receipts;
- artifacts;
- snapshots and checkpoints;
- output or error identity;
- event braid hashes and signed refs.

### 4. Projection

A derived read model over existing definitions, runtime events, receipts,
artifacts, refs, snapshots, and CAS objects.

The projection should not replace `ThreadEvent`, the event braid, CAS, signed
refs, graph runtime event vocabulary, or static `.ai` topology. It should be an
additive view that connects capability and consequence for inspection,
debugging, export, audit, and eventual replay/verification work.

## Guardrails

Do not implement this note as an API yet.

Specifically deferred:

- no `portable_execution_projection` endpoint/API;
- no `ThreadEvent` shape changes for graph-specific identity;
- no CAS/ref architecture changes;
- no public graph event string renames;
- no `ui.graph.topology` rename;
- no semantic YAML canonicalization for `definition_hash`;
- no trust or authorization semantics derived from hashes alone;
- no universal execution model across all executable kinds until graph runtime
  identity is stable and tested.

## Why this is deferred

The system first needs stable identity breadcrumbs emitted by execution:

```text
definition_ref + definition_hash
  -> graph_run_id
  -> node_ref
  -> runtime event payloads
  -> node receipts
  -> node_result_hash / artifact identity
```

Without those facts, a portable execution graph projection would be forced to
guess from names, text, or incomplete event payloads. The current slice should
therefore make the bridge precise and regression-tested before introducing a
new query surface.

## When to revisit

Consider the advanced projection when one or more of these becomes concrete:

- consumers need cross-run querying/export by `definition_hash` or
  `definition_ref`;
- RyeOS UI needs runtime trace projection rather than static `.ai` topology;
- replay, resume, or audit workflows require an attestable closure from signed
  capability to consequence;
- admission/trust policy needs to reason over capability, invocation, and
  consequence together;
- portable graph execution histories need to move across machines, projects, or
  vendors.

## Relationship to topology

`ui.graph.topology` is a static topology projection over resolved `.ai` items
and their structural/heuristic references. It can show authored workflow nodes
and declared relationships, but it is not runtime history.

The future portable execution graph projection would be a runtime/history view:

```text
authored definition topology
  + execution run identity
  + event braid facts
  + receipts/artifacts/checkpoints
  + trust/provenance overlays
```

Both views are useful, but they answer different questions:

- topology: what exists and what references what?
- execution projection: what ran and what consequence followed?

## Current implementation target

For now, implementation should stay limited to:

- pinning `definition_ref` and `definition_hash` semantics;
- using canonical JSON for `node_result_hash`;
- ensuring node-scoped events carry `node_ref`;
- ensuring runtime events and node receipts carry definition identity;
- publishing action-node error receipts if the existing receipt shape supports
  it;
- maintaining the internal graph execution trace projection as a helper over
  already-persisted events and artifacts, without promoting it to a route/API;
- documenting topology internals as a projection without changing public API.

This is enough to make future projection possible without prematurely adding a
new execution graph API.
