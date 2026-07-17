<!-- ryeos:signed:2026-07-15T23:50:35Z:c3f327866b152bd1820414f9cd119684700dba1499b7afc65c14d5290aeb4bd5:LYzFX6eyT6Flr/LxUJgUqGXYN12rR+JwYSD9pRbt+d4o0v8o9bXN7J3qkx1b9AZSkWuwoL/+ECQWVPk+JqSTAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/services
tags: [service, maintenance, gc, cas, compact, sweep]
version: "2.2.0"
description: >
  The two-phase garbage collector — compact (retention-based DAG
  pruning with topological rewrite) and sweep (mark-and-sweep of
  unreachable objects). Covers the retention policy, flock-based
  locking, JSONL event logging, and the full GC pipeline.
---

# Service: maintenance/gc

Invariant: maintenance GC reclaims unreachable CAS state according to
signed invocation data, with dry-run and compact modes for safe operation.

Run GC as a maintenance task, not during request-critical paths.

## Pipeline

GC runs in two phases. Compact runs first (opt-in), then sweep always
runs:

```
Phase 1: COMPACT (opt-in, --compact flag)
  - Requires a signer (to update project head refs)
  - Per-project retention-based DAG pruning
  - Topological rewrite of surviving snapshots

Phase 2: SWEEP (always)
  - No signer needed
  - Mark: collect all reachable objects from signed heads
  - Sweep: delete unreachable objects + blobs from sharded dirs
  - Clean empty shard directories bottom-up
```

Compact runs before sweep because compaction orphans snapshots by
removing them from the DAG. Sweep then collects those newly-unreachable
objects.

## Operational History Retention

Operational cleanup is data-driven. The service has no built-in age or count;
omitting a parameter disables that cleanup bound, including for `deep` GC:

| Parameter | Effect when present |
| --- | --- |
| `schedule_fire_max_age_days` | Drop terminal schedule-fire groups older than this age |
| `schedule_fire_max_count` | Keep at most this many terminal fire groups per schedule |
| `sync_job_retention_days` | Drop older terminal sync jobs and their attempt rows |
| `seat_lease_grace_seconds` | Settle running seat sessions this long after lease expiry |
| `durable_cas_upload_max_age_seconds` | Retire abandoned durable multi-request CAS upload stages older than this signed age |

The signed maintenance schedule authors the node's values explicitly. Terminal
execution-chain history is different: eligibility comes only from the policy
captured on that chain root, and `deep` merely asks the daemon to evaluate those
captured policies and generic recovery pins.

Durable upload stages are GC roots until they are explicitly retired. There is
no compiled-in upload timeout: omission preserves them indefinitely. A recurring
schedule authors an age rather than a fixed timestamp so each invocation derives
a fresh canonical cutoff while holding the exclusive CAS mutation guard.

## Offline Full Thread-History Retirement

Normal maintenance GC never deletes the chain-head namespace as a bulk
operation. When an operator explicitly chooses to discard the entire local
thread-history epoch, use the bootstrap-local command while the daemon is
stopped:

```bash
# Inspect every participating store without deleting history.
ryeos node gc --discard-thread-history --dry-run

# Retire all thread history and publish empty current projections.
ryeos node gc --discard-thread-history --confirm-discard-thread-history
```

This clears authoritative thread-chain heads and pending transitions, daemon
execution rows, per-thread runtime files, scheduler fire journals/rows, and all
superseded thread-projection databases. It preserves node identity and trust,
node configuration, installed bundles, vault data, signed schedule definitions,
project and bundle-event heads, and stable operational admission/sync state.
Independently retained trace/log/cache data remains governed by the normal
maintenance GC parameters; this recovery command does not silently broaden its
deletion scope to those stores.

The command publishes a durable discard marker before its first destructive
step. Ordinary startup refuses while that marker exists; rerunning the same
confirmed command resumes the idempotent cleanup. Physical CAS reclamation is
separate: add `--sweep-cas` to the confirmed run, or allow normal maintenance GC
to reclaim the now-unreachable objects later.

On an interactive terminal the command renders its typed maintenance phases in
one redrawn line. Head retirement reports the exact verified-head count and is
throttled to terminal refresh speed rather than writing once per deletion.
`--json` and redirected invocations remain stable, plain output and never emit
ANSI cursor controls.

## Phase 1: Compact

Compact operates per-project. For each project in the refs tree:

### Step 1: Walk the DAG

BFS from the project HEAD via `parent_hashes`. Strict: errors on
missing or corrupt objects. Collects all `SnapshotInfo` (hash, source,
parent_hashes, created_at).

### Step 2: Sort by Time

All reachable snapshots are sorted by `created_at` descending (newest
first). Retention is by timestamp, not traversal order.

### Step 3: Apply Retention Policy

```rust
struct RetentionPolicy {
    manual_pushes: usize,   // authored limit for "push" and "manual" sources
    auto_snapshots: usize,  // authored limit for "fold_back" and other auto sources
}
```

`compact: true` requires the complete nested `policy` object. Both fields are
mandatory; RyeOS supplies no default or partial-policy fallback. The signed
scheduled-maintenance declaration authors both values explicitly.

HEAD is always kept regardless of policy. Then iterate newest-first:
count per category, keep up to the policy limit for each. Everything
else goes into the `removed` set.

### Step 4: Topological Rewrite

Topological sort of kept nodes using Kahn's algorithm (roots first):
- For each kept snapshot, resolve `parent_hashes` through removed
  snapshots to surviving ancestors
- If `parent_hashes` changed, write a new CAS object with the updated
  field (content-addressed via canonical JSON + SHA-256)
- Maintain a `hash_remap` map (old hash → new hash)
- If the result has fewer nodes than the kept set, bail with "possible
  cycle in snapshot DAG"

Dry-run skips CAS writes but still counts rewrites.

### Step 5: Update HEAD Ref

If HEAD's hash changed (due to parent rewrite), advance the project
head ref with the node signer.

→ See [DAG Versioning](../../state/dag-versioning.md) for the full
  snapshot DAG model.

## Phase 2: Sweep

Classic mark-and-sweep:

### Mark

`collect_reachable()` walks all root refs (chain roots + project heads)
and transitively resolves all reachable object and blob hashes into a
`HashSet<String>`.

### Sweep

For each sharded namespace (`objects` with `.json` extension, `blobs`
without extension):

1. Iterate the 2-level hex shard layout: `namespace/ab/cd/hash{ext}`
2. For each file, extract the hash (strip extension if needed)
3. If the hash is **not** in the reachable set: delete the file and
   accumulate `freed_bytes`
4. **Bottom-up cleanup**: after processing each leaf directory, attempt
   to remove it if empty. Same for parent directories after all children
   are processed

This prevents accumulation of empty directories from deleted objects.

## Locking

GC uses file-based locking to prevent concurrent runs:

- **Lock file**: `{runtime_state_dir}/gc.lock` — persistent lock anchor
- **State sidecar**: `{runtime_state_dir}/gc.state.json` — records PID, node
  ID, phase, and start time
- **Mechanism**: `libc::flock()` with `LOCK_EX | LOCK_NB` (exclusive,
  non-blocking). Fails immediately if another GC run is in progress.
- **Drop semantics**: removes `gc.state.json` first (before unlocking),
  then explicitly calls `LOCK_UN`. The lock file persists on disk.

## Event Logging

GC results are logged to `{runtime_state_dir}/logs/gc.jsonl` — one JSON object
per line, append-only:

```json
{
  "timestamp": "2026-05-20T...",
  "dry_run": false,
  "compact": true,
  "roots_walked": 3,
  "reachable_objects": 100,
  "reachable_blobs": 20,
  "deleted_objects": 10,
  "deleted_blobs": 5,
  "freed_bytes": 4096,
  "snapshots_compacted": 15,
  "duration_ms": 150
}
```

## CLI Usage

```bash
# Dry run (preview only, no mutations)
ryeos maintenance gc --dry-run

# Compact + sweep (both policy limits are required)
ryeos maintenance gc --compact --policy '{"manual_pushes":10,"auto_snapshots":30}'

# Sweep only (no snapshot pruning)
ryeos maintenance gc
```
