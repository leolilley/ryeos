# ryeos:signed:2026-05-20T11:23:00Z:cb03c7a46629040cddf0c6fad99b8b93e019db592916fc70bb249f1ef3c25bb7:WPvhAGbPTIDtkqiOhkz5PaQNyn58Mb8ksCO6CUq0KFjYeG2j6peLicNjDwlU7WhtHF9xmVmrnvWzNmoAk2ojBw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea

---
category: ryeos/core/services
tags: [service, maintenance, gc, cas, compact, sweep]
version: "2.0.0"
description: >
  The two-phase garbage collector — compact (retention-based DAG
  pruning with topological rewrite) and sweep (mark-and-sweep of
  unreachable objects). Covers the retention policy, flock-based
  locking, JSONL event logging, and the full GC pipeline.
---

# Service: maintenance/gc

Invariant: maintenance GC reclaims unreachable CAS state according to
daemon policy, with dry-run and compact modes for safe operation.

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
    manual_pushes: usize,   // default: 10 — "push" and "manual" sources
    auto_snapshots: usize,  // default: 30 — "fold_back" and other auto sources
}
```

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

- **Lock file**: `{state_root}/gc.lock` — persistent lock anchor
- **State sidecar**: `{state_root}/gc.state.json` — records PID, node
  ID, phase, and start time
- **Mechanism**: `libc::flock()` with `LOCK_EX | LOCK_NB` (exclusive,
  non-blocking). Fails immediately if another GC run is in progress.
- **Drop semantics**: removes `gc.state.json` first (before unlocking),
  then explicitly calls `LOCK_UN`. The lock file persists on disk.

## Event Logging

GC results are logged to `{state_root}/logs/gc.jsonl` — one JSON object
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

# Compact + sweep
ryeos maintenance gc --compact

# Sweep only (no snapshot pruning)
ryeos maintenance gc
```
