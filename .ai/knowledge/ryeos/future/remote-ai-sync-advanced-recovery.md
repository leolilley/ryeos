<!-- rye:signed:2026-05-21T07:07:49Z:14f22c1ad5433e51cd471c86f52a1976e9b5d7cf2f7aea335151c79e911c43f6:Vl5c4jU_ElggxU_vg9_R1Kr83nKDUl5AaLN_SnntFMqqQiGhk9ScEFYlFVf_lj10Yxzi4GLdtjDIv8cUAnZJCg:4b987fd4e40303ac -->
```yaml
category: ryeos/future
name: remote-ai-sync-advanced-recovery
title: Remote AI Sync Advanced Recovery Design
entry_type: pattern
version: "1.0.0"
author: amp
created_at: 2026-05-21T00:00:00Z
description: Future design for cross-process locking, deployment journaling, and crash recovery for remote AI project sync
tags:
  - remote-sync
  - crash-recovery
  - deployment-journal
  - locking
```

# Remote AI Sync Advanced Recovery Design

## Purpose

This note captures the post-v1 hardening path for `remote sync-project-ai` / `project.apply-snapshot`.

V1 is intentionally single-daemon oriented: it uses an in-process per-project mutex, writes a deployed ref after a successful managed-root swap, and relies on best-effort rollback during the apply call. That is adequate for the current Docker/Railway deployment model, but a future multi-process or shared-volume deployment needs stronger coordination and crash recovery.

## Goals

- Serialize applies across daemon processes and containers that share a project filesystem.
- Make each deployment restart-recoverable after process crash, container restart, or host reboot.
- Preserve enough journaled state to either finish the deployment or restore the previous live roots.
- Keep refs honest: a deployed ref is advanced only after the filesystem is known to match the new snapshot.
- Support operator-visible status for in-progress, failed, recovered, and rolled-back deployments.

## Non-goals

- Do not broaden v1's sync scope. AI-only sync remains project-only and must still reject `user_manifest_hash`.
- Do not include `.ai/node/routes` or `.ai/services` without a separate route/service exposure review.
- Do not make app-source deployment part of this path.
- Do not treat the journal as a full history UI; deployment records can be compacted after retention requirements are met.

## Cross-process locking

Use a filesystem lock file under the live project, for example:

```text
<project>/.ai/state/deploy-locks/project-ai-sync.lock
```

The lock must be acquired before reading the deployed ref, validating the expected deployed hash, mutating managed roots, or advancing the deployed ref. It should also be acquired by `push_head` before writing the caller's staged HEAD for that same canonical project path, matching the v1 in-process ordering:

1. canonicalize remote project path
2. acquire project deployment lock
3. acquire CAS/state write barrier
4. read/write refs and/or mutate managed roots

Implementation notes:

- Prefer an OS advisory lock (`flock` on Unix) over lock-by-directory when available.
- Store lock metadata next to the lock file: pid, hostname/container id, daemon principal, project hash, started_at, operation id.
- Do not rely on stale-lock deletion as the main safety mechanism. Advisory locks are released by the kernel on process death; metadata is for diagnostics.
- Keep the lock on the same filesystem as the project so it coordinates all processes touching that project tree.

## Deployment journal

Before changing live roots, create a deployment record in a durable journal directory:

```text
<project>/.ai/state/deployments/<deployment_id>.json
```

Suggested schema:

```json
{
  "schema": 1,
  "kind": "project_ai_deployment",
  "deployment_id": "20260521T170000Z-<nonce>",
  "project_path": "/data/projects/example",
  "project_hash": "...",
  "principal_key": "...",
  "snapshot_hash": "new snapshot",
  "previous_deployed_hash": "old snapshot or null",
  "expected_deployed_hash": "operator expectation or null",
  "state": "prepared",
  "created_at": "...",
  "updated_at": "...",
  "managed_roots": [
    {
      "rel_root": ".ai/directives",
      "dest": "/data/projects/example/.ai/directives",
      "staged": "/data/projects/example/.ai/state/deployments/<id>/staging/.ai/directives",
      "backup": "/data/projects/example/.ai/state/deployments/<id>/backup/.ai/directives",
      "action": "replace",
      "state": "pending"
    }
  ]
}
```

Keep staging and backups inside the deployment record directory, not in random project-root temp names:

```text
<project>/.ai/state/deployments/<deployment_id>/staging/...
<project>/.ai/state/deployments/<deployment_id>/backup/...
```

State transitions should be monotonic and fsync-backed enough for the target platform:

```diagram
╭──────────╮  materialize  ╭──────────╮  swap roots  ╭─────────╮
│ prepared │──────────────▶│ staged   │─────────────▶│ swapped │
╰──────────╯               ╰──────────╯              ╰────┬────╯
                                                           │ advance deployed ref
                                                           ▼
╭────────────╮  rollback ok  ╭────────────╮         ╭──────────╮
│ recovering │──────────────▶│ rolled_back│         │ committed│
╰────────────╯               ╰────────────╯         ╰──────────╯
      ▲                            ▲                      │
      │ crash/error                 │ error before ref      │ cleanup after retention
      ╰────────────────────────────╯                      ▼
                                                       ╭─────────╮
                                                       │ cleaned │
                                                       ╰─────────╯
```

## Crash recovery algorithm

Run recovery at daemon startup and before each new `project.apply-snapshot` for the target project:

1. Acquire the cross-process project lock.
2. Scan deployment records that are not `committed`, `rolled_back`, or `cleaned`.
3. For each record, read the deployed ref.
4. If deployed ref already equals `snapshot_hash`, treat the deployment as committed and remove or retain backups according to retention policy.
5. If deployed ref equals `previous_deployed_hash` or is absent when previous was null, roll back any installed roots from backups and mark `rolled_back`.
6. If deployed ref is some unrelated hash, stop and report manual intervention required; another deployment changed the project after the crash.
7. Never advance the deployed ref during recovery unless the filesystem has been verified to match the target snapshot.

Filesystem verification can start simple and become stronger over time:

- v1+: verify all manifest files exist under managed roots and content hashes match CAS blobs.
- Later: write a materialized-root manifest/checksum file in the journal and compare roots against it.
- Optional: validate file modes match `ItemSource.mode & 0o777`.

## Apply flow with journaling

The future `project.apply-snapshot` flow should become:

1. Canonicalize project path and compute project hash.
2. Acquire cross-process lock.
3. Acquire write barrier.
4. Recover incomplete deployments for the project.
5. Verify caller staged HEAD equals requested snapshot.
6. Validate snapshot scope, absence of user manifest, manifest paths, item objects, blob hashes, and file modes.
7. Create journal record in `prepared` state.
8. Materialize all files to journal staging and mark `staged`.
9. Rename existing managed roots into journal backups and staged roots into live destinations, recording each root state.
10. Verify live roots match the manifest and mark `swapped`.
11. Advance deployed ref with CAS from `previous_deployed_hash` to `snapshot_hash`.
12. Mark `committed`.
13. Cleanup staging immediately; retain backups for a bounded window or until an explicit `deployment cleanup` maintenance pass.

If any step before the deployed-ref advance fails, rollback from backups while still holding the lock and mark the record `rolled_back` or `failed_rollback`.

If the deployed-ref advance fails after roots are swapped, prefer rolling roots back to `previous_deployed_hash` and marking `rolled_back`. If rollback fails, mark `failed_rollback` and surface the deployment id in the error.

## Status and operator tooling

Extend `project.status` / `remote project-status` to include deployment journal state:

- current deployed snapshot hash and scope
- latest deployment id
- latest deployment state
- in-progress operation age
- failed recovery/rollback marker
- retained backup count and approximate size

Useful future commands:

- `remote project-deployments --project ...` — list records
- `remote recover-project-ai --project ...` — run recovery explicitly
- `remote rollback-project-ai --project ... --to <snapshot>` — rollback using retained backups or CAS rematerialization
- `remote cleanup-project-deployments --project ... --older-than ...` — remove committed backups/staging

## Security invariants to preserve

- All manifest paths still pass `validate_project_manifest_paths(..., AiOnly)`.
- Exact managed-root paths are not accepted as files.
- Existing live managed roots or their parents must not be symlinks.
- Staging and backup paths must be under the canonical project path and never derived from unvalidated manifest paths except through the validated relative path join.
- File modes remain clamped to `0o777`; special bits are never restored.
- The deployed ref is the source of current deployment state, but only after the filesystem has been verified.

## Incremental implementation plan

1. Add a small cross-process lock helper and use it in `project.apply-snapshot` and `push_head`.
2. Move staging/backups under a per-deployment journal directory.
3. Write journal records with state transitions but keep v1's rollback behavior.
4. Add startup/before-apply recovery for incomplete records.
5. Add status surfacing and cleanup tooling.
6. Add tests that simulate crashes by stopping after each journal state and invoking recovery.
