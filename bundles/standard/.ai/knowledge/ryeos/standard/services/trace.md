<!-- ryeos:signed:2026-07-21T00:24:30Z:ef5b881098dbed27abf8b43b6c7b9729257586932e36121d8df900351c1a1f68:sxCzUwoL++DaPqV5cNj1khhqsXqfN05/e29SnJ0O7ldR5gxpgeAwCTwmBPwpnH/moKqTiyIQSxA+dyosBqA6Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, trace, replay, branch, provenance, state-anchor]
version: "1.1.0"
description: Programmable trace service reference.
---

# Services: trace

Invariant: trace services make durable execution history programmable without
rewriting parent history. They operate over signed, hash-linked chain events and
domain-authored state anchors.

Trace services are higher-level views and mutations over the event braid:

- `trace.inspect` is read-only and returns a normalized trace view over durable
  replay.
- `trace.branch` creates a daemon-authored branch relation from a parent event
  and a state-anchor milestone.

## Trace points

A trace point is a durable event ref:

```json
{
  "chain_root_id": "T-root",
  "thread_id": "T-child",
  "chain_seq": 42,
  "thread_seq": 3,
  "event_hash": "64-hex-cas-hash",
  "event_type": "milestone"
}
```

`chain_seq` is global within a chain root. `thread_seq` is local to the thread.
`event_hash` is the CAS hash of the persisted event object. Any mutating trace
service must replay the cited event and compare every field in the ref; caller
payload is never trusted as proof.

## `trace.inspect`

`trace.inspect` accepts exactly one of `thread_id` or `chain_root_id`, plus
optional `after_chain_seq` and `limit`.

- `thread_id` mode inspects one thread's durable events.
- `chain_root_id` mode inspects the chain-wide braid.
- The cursor is `after_chain_seq`.
- The response includes normalized events, raw events, stable event refs,
  extracted state anchors, and `next_cursor`.

`trace.inspect` does not read daemon checkpoint files and does not expose model
or runtime-internal cache state. It is a replay projection over durable indexed
events.

## State anchors

A branchable domain state is represented as a normal durable `milestone` event
using the nested milestone shape:

```json
{
  "event_type": "milestone",
  "payload": {
    "kind": "state_anchor",
    "payload": {
      "schema_version": 1,
      "label": "arc.sim_state",
      "state_digest": "sha256:...",
      "manifest_ref": "cas:...",
      "runtime": {
        "kind": "tool",
        "item_ref": "tool:arc/simulate"
      },
      "metadata": {}
    },
    "node": "optional graph node",
    "step": "optional graph step"
  }
}
```

The domain owns canonicalization. RyeOS treats `state_digest` equality as a
domain claim unless the domain also publishes and verifies the manifest or
object behind `manifest_ref`.

## `trace.branch`

`trace.branch` is daemon-only and requires
`ryeos.execute.service.trace/branch`. It creates a new child thread plus the
initial branch provenance events in one signed chain-head transition.

Request shape:

```json
{
  "parent_event_ref": { "...": "event ref" },
  "state_anchor_ref": { "...": "event ref" },
  "child_thread_id": "optional stable retry key",
  "purpose": "holdout",
  "kind": "directive",
  "item_ref": "directive:...",
  "executor_ref": "native:...",
  "launch_mode": "wait",
  "restore_contract": {},
  "metadata": {}
}
```

`parent_event_ref` and `state_anchor_ref` must be in the same chain. The anchor
ref must point to a `milestone` whose payload kind is `state_anchor`.

For idempotent retries, callers should provide a stable `child_thread_id`.
A duplicate explicit child id returns conflict and must not advance the signed
chain head.

## Branch provenance

Trace branches do not use `upstream_thread_id` and do not appear as ordinary
spawned children. The branch relation is represented by a daemon-authored
`edge_recorded` event:

```json
{
  "relation": "trace_branch",
  "child_thread_id": "T-branch",
  "parent_event_ref": { "...": "event ref" },
  "state_anchor_ref": { "...": "event ref" },
  "purpose": "holdout",
  "restore_contract": {},
  "metadata": {}
}
```

The child snapshot, its `thread_created` event, the `edge_recorded` branch
provenance event, the chain state, and the signed chain head are committed as
one durable chain update. The projection groups the child row and initial event
rows in one SQLite transaction so normal reads do not see a branch child without
its provenance.

## What trace services are not

- Not checkpoint forking. Branchable points are domain-emitted state anchors,
  not daemon checkpoint files.
- Not history rewriting. Parent chains are append-only; a branch is a new thread
  plus signed provenance.
- Not KV-cache reuse. Model-internal cache state is outside the trace contract.
- Not generic evaluation. Domain outcomes should be recorded through
  domain-owned events, such as bundle events, until a specific substrate
  contract exists.
