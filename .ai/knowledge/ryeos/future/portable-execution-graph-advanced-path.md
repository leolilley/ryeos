<!-- ryeos:signed:2026-08-06T00:58:12Z:739d602ca40eec0d68c6d18bd9a57da10060f3dff7b38d51ee02143b27e4f2eb:iHVyTiuSUvveHICc/m1+Jdkt/0zlTuqcn3ht1/lgW1ALY62FWG0CTtG6K0zc0O0LLRXqx/JOLuFbayI00PknAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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

- `definition_ref`: canonical admitted definition ref.
- `root_raw_content_digest`: SHA-256 of the signature-stripped root source
  bytes; this is source provenance, not effective execution identity.
- `effective_definition_digest`: canonical identity of the complete admitted
  resolution, including ordered ancestors, referenced contributors, composed
  behavior, derived effective hook plan, policy facts, and trust provenance.
- `graph_run_id`: invocation/run instance identity.
- `node_ref`: `{definition_ref}#node:{node_name}` on node-scoped runtime
  events.
- `node_result_hash`: canonical JSON hash of a successful action node result.
- `graph_node_receipt` artifacts: daemon-compatible artifact wrappers whose
  `metadata` contains the node receipt payload and optional `node_result_hash`.
- Runtime events and node receipts: realized consequences linked back to the
  effective definition identity and its root-source provenance.

Current regression proof:

- The graph runtime unit tests pin the receipt/event payload shape at the
  callback boundary.
- The daemon graph action E2E proves successful graph execution persists
  `graph_node_receipt` artifacts and runtime events carrying `definition_ref`,
  `root_raw_content_digest`, `effective_definition_digest`, `graph_run_id`,
  and `node_ref`.
- The same E2E proves denied callback dispatch persists an error receipt and
  failure-path runtime events. The failed tool dispatch event uses
  `tool_call_result.status = "dispatch_failed"`; the graph step completion event
  uses `graph_step_completed.status = "error"`.
- The generic field services project current definitions, project topology,
  run summaries, and braid-bounded execution facts. They join occurrences to
  effective graph nodes only when admitted identity agrees and degrade a
  malformed individual event without discarding the complete field document.
- This is an implemented operator read model, not yet a portable export or
  cross-node verification API.

`root_raw_content_digest` identifies exact root bytes after signature-line
stripping. `effective_definition_digest` identifies admitted behavior. Neither
digest alone grants trust, authorization, safety, or policy authority; those
come from the verified resolution, captured effective program, and sealed
launch evidence.

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
- no conflation of root-source digest with effective-definition identity;
- no trust or authorization semantics derived from hashes alone;
- no universal execution model across all executable kinds until graph runtime
  identity is stable and tested.

## Why this is deferred

The system first needs stable identity breadcrumbs emitted by execution:

```text
definition_ref + root_raw_content_digest + effective_definition_digest
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

- consumers need portable cross-node export by `effective_definition_digest`
  or `definition_ref`;
- RyeOS UI needs projection beyond the current living-field project/run/
  execution views;
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

The implemented foundation should remain anchored on:

- pinning `definition_ref`, `root_raw_content_digest`, and
  `effective_definition_digest` to their distinct semantics;
- using canonical JSON for `node_result_hash`;
- ensuring node-scoped events carry `node_ref`;
- ensuring runtime events and node receipts carry effective definition
  identity and root-source provenance;
- publishing action-node error receipts if the existing receipt shape supports
  it;
- maintaining the generic field services as bounded read models over existing
  chain history, artifacts, and exact admitted definitions;
- keeping any future portable export layered over those facts rather than
  introducing a second execution-history substrate.

This is enough to make future projection possible without prematurely adding a
new execution graph API.
