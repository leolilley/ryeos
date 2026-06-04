# Advanced native resume path

## Status

Deferred. Current native resume support pins LocalPath execution to a snapshot
captured at spawn time and resumes from persisted launch metadata/checkpoints.

## Already implemented baseline

- Native resume metadata is persisted for threads that declare resume support.
- LocalPath resume is pinned to the original project snapshot when needed.
- Reconciliation can re-spawn resumable work under the same thread identity.

## Deferred work

Advanced resume work may include:

1. richer checkpoint versioning and migration;
2. explicit resume policy per runtime/protocol;
3. operator-visible diagnostics for skipped/non-resumable threads;
4. stronger handling for changed bundle/runtime binaries between spawn and
   resume;
5. cancellation/resume interaction rules;
6. cross-node resume once source snapshots and execution leases are modeled as
   first-class RyeOS objects.

Do not expand this until a runtime needs stronger semantics than the current
snapshot-pinned local resume path.
