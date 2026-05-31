<!-- ryeos:signed:2026-05-31T08:15:56Z:d3a17a64a5e35ecc0b2e28e2fc0566378cf729f16ed45548b7fa6a405064690f:Y4+IihFFE5BLxGt/qe96O4A5ezN2h6L4+AIZJ0wA+fxwz6ESo7I8mVPbFGSiyHWyVnZpBLu1NilTDoh61MSxAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/execution
tags: [execution, provenance, callbacks, pushed-head, typestate]
version: "1.0.0"
description: >
  ExecutionProvenance typestate model: the role/source matrix, ownership
  rules, construction sites, lifecycle gates, and current resume caveat.
---

# Execution Provenance

Invariant: every execution carries exactly one `ExecutionProvenance`
value, and its variant encodes which engine, workspace, project source,
and snapshot lifecycle that execution is allowed to own.

## The four legal shapes

`crates/daemon/ryeos-app/src/execution_provenance.rs:41-84` defines the complete
role × source matrix:

| Variant | Source | Role | Owns snapshot lineage? |
|---|---|---|---|
| `RootLiveFs` | Live filesystem | Root | No CAS snapshot exists |
| `RootPushedHead` | Pushed HEAD checkout | Root | Yes: snapshot hash + temp-dir lifeline |
| `BorrowedChildLiveFs` | Live filesystem | Callback child | No |
| `BorrowedChildPushedHead` | Pushed HEAD checkout | Callback child | No: borrows parent's lifeline only |

Pushed roots carry a non-optional `Arc<TempDirGuard>` and a
`snapshot_hash`. Borrowed children do not have a snapshot-hash field,
so callback children cannot accidentally pin, fold back, or advance the
parent's HEAD lineage.

## Construction rules

Root provenance is constructed at execution entry points: HTTP
`/execute`, SSE launch, scheduler ticks, and resume reconciliation.
Callback provenance is never reconstructed from loose fields; it is
derived with `clone_for_borrowed_child()` at callback-token minting and
runtime dispatch boundaries (`execution_provenance.rs:137-175`).

`root_pushed_head()` is the only constructor that performs a runtime
invariant check (`execution_provenance.rs:97-135`). It panics if the
checkout lifeline is disarmed or if `workspace_lifeline.path()` does not
equal `effective_path`. Missing lifeline and missing snapshot hash are
not runtime cases anymore: the variant cannot be constructed without
those fields.

## Lifecycle gates

The runner asks `provenance.is_borrowed_child()` before snapshot pinning
and post-execution foldback (`crates/engine/ryeos-executor/src/execution/runner.rs:775-785`).
Root pushed-head executions own pin/foldback. Borrowed children inherit
the parent's working directory and must not touch snapshot lifecycle.

CAS context preparation is variant-matched in
`crates/engine/ryeos-executor/src/execution/runner.rs:241-317`. Root LiveFs ingests the
live tree, Root PushedHead tracks the lifeline and reads the pre-manifest,
and borrowed variants validate the borrowed directory and return no
manifest/snapshot.

## Resume caveat

Resume currently reconstructs provenance as `root_live_fs` with the
daemon engine. This is an explicit degradation path until resume can
rebuild per-snapshot overlay engines and lifelines. Do not infer pushed
state on resume from stale metadata.
