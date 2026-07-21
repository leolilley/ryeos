<!-- ryeos:signed:2026-07-21T00:24:56Z:4ede43df85b84b63ed214b7af8ca79d2c3cdd716d702a555773231295a4ed6be:1x+aFX0n5OvfpiWWsjGA9XKzr9Jlt2SVpKF25+F2LQtKPDqXV7kCWTLsHiAd6MC99KqmbI0jsV1hgyQpBYxAAg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: native-resume-snapshot-pinning
title: Native Resume Snapshot Pinning
description: Deferred extensions to snapshot-pinned native resume semantics
entry_type: design
version: "1.0.0"
```

# Native Resume Snapshot Pinning

## Status

Deferred follow-up notes for native resume. Current native resume support pins
LocalPath execution to a snapshot captured at spawn time and resumes from
persisted launch metadata/checkpoints.

## Already implemented baseline

- Native resume metadata is persisted for threads that declare resume support.
- LocalPath resume is pinned to the original project snapshot when needed.
- Reconciliation can re-spawn resumable work under the same thread identity.

## Deferred follow-up work

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
