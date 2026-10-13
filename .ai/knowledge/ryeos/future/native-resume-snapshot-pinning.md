<!-- ryeos:signed:2026-10-06T01:00:58Z:eb80bf8b46db87a667ccefcc948e5f536a80441b9581025d4e6d7d5a1cf6b70f:lpM0Uj5Yp6/yRMoL7xfSDYPPxSvquQ7kcSEQ3eL4RDbFYs0r7qiH7mypBxRHDi6kMuQFKsIva7jP8asl6QphCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: native-resume-snapshot-pinning
title: Native Resume Snapshot Pinning
description: Deferred extensions to snapshot-pinned native resume semantics
entry_type: design
version: "1.0.1"
```

# Native Resume Snapshot Pinning

This note concerns native runtime-specific resume extensions. Existing hosted
worker checkpoint transfer and explicit cold placement have their own contracts;
the deferred cross-node item below does not defer or generalize those supported
paths. Follow the [platform direction](platform-direction-and-delivery.md) for
overall priorities and the selected runtime contract for actual support.

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
6. native-runtime-specific cross-node resume when that runtime supplies the
   exact snapshot, resource, target-admission and continuation contracts; reuse
   supported hosted mechanisms where appropriate without promising arbitrary
   live-process migration.

Do not expand this until a runtime needs stronger semantics than the current
snapshot-pinned local resume path.
