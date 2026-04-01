```yaml
id: garbage-collection
title: "CAS Garbage Collection"
description: "Automatic and manual garbage collection for the content-addressed store — prune caches, compact history, mark-and-sweep unreachable objects"
category: internals
tags: [gc, garbage-collection, cas, storage, compaction, mark-sweep, epochs]
version: "1.0.0"
```

# CAS Garbage Collection

CAS accumulates objects indefinitely from remote pushes and graph executions. GC frees storage by pruning derived caches, compacting snapshot history, and sweeping unreachable objects. The same GC engine powers the Rye tool (local + remote), the server auto-GC, and the server API endpoints.

## Architecture

| Component          | File                                                        |
| ------------------ | ----------------------------------------------------------- |
| Core engine        | `ryeos/rye/cas/gc.py`                                       |
| Type definitions   | `ryeos/rye/cas/gc_types.py`                                 |
| Epoch support      | `ryeos/rye/cas/gc_epochs.py`                                |
| Lock support       | `ryeos/rye/cas/gc_lock.py`                                  |
| Incremental state  | `ryeos/rye/cas/gc_incremental.py`                           |
| Rye tool           | `ryeos/bundles/core/ryeos_core/.ai/tools/rye/core/gc/gc.py` |
| Server integration | `ryeos-node/ryeos_node/server.py`                           |

## 3-Phase Pipeline

### Phase 1: Cache & Execution Pruning

- Prunes materialized `cache/snapshots/` directories (fully derived, safe to delete unconditionally)
- Prunes user-space cache (`cache/user/`)
- Prunes excess execution records per `(project_path, graph_id)`: keeps running + last N success + last N failure
- Configurable via `cache_max_age_hours` (default 24) and `max_executions_per_graph` (default 10)

### Phase 2: History Compaction

Rewrites the ProjectSnapshot DAG chain to make old objects unreachable. Walks the first-parent chain from HEAD and classifies which snapshots to retain:

- **HEAD** (always)
- **Last N manual pushes** (source == "push", default 3)
- **1 daily checkpoint** per calendar day for last N days (default 7)
- **Weekly checkpoints** (optional)
- **Pinned snapshots** (always, exact hash preserved)

Builds a new chain oldest→newest with rewritten parent links. Uses CAS compare-and-swap to advance HEAD atomically. Skips compaction if the chain is too deep (>50K) or history is incomplete.

### Phase 3: Mark-and-Sweep

- Collects root hashes from all live refs (project HEADs, user-space HEAD, pins, execution records, running markers, in-flight epochs)
- Two root collectors: `_collect_roots_server()` for per-user server layout, `_collect_roots_local()` for project CAS layout (`cas_root/refs/`)
- Iterative BFS traversal from roots — follows all hash references in objects to build the reachable set
- Sweep deletes objects and blobs not in the reachable set, respecting a grace window (default 3600s, aggressive: 300s)
- Grace window protects objects created after mark started

## Writer Epochs

Push and execute endpoints register epochs before creating CAS objects via `register_epoch()`. Epochs complete after ref advance via `complete_epoch()`.

- Stored in `user_root/inflight/` as JSON files
- Epoch root hashes are added to the reachable set during mark phase
- Stale epochs cleaned after configurable timeout (default 30 min)
- Prevents sweep from deleting objects being written by concurrent operations

## Pin Support

- `pin_snapshot(user_root, cas_root, project_hash, snapshot_hash, pin_id)` creates durable refs at `refs/pins/<project>/<pin-id>/head`
- `unpin_snapshot(user_root, project_hash, pin_id)` removes pins
- Pinned snapshots survive compaction — their original hash is preserved (never rewritten)
- Pin refs are treated as GC roots during mark phase

## Distributed Lock

Per-user lock at `user_root/gc.lock.json`:

- Includes `gc_run_id`, `node_id`, `timestamp`, `expiry`, `generation` counter, `current_phase`
- Lock expires after configurable timeout (prevents deadlocks from crashed GC runs)
- Phase tracking: `idle` → `prune` → `compact` → `mark` → `sweep`

## Server Integration

### Auto-GC

Triggered in `_check_user_quota()` when a user exceeds `max_user_storage_bytes` quota:

- Rate-limited by `gc_auto_cooldown_seconds` (default 600s)
- First attempts emergency cache prune, then full aggressive GC if still over quota
- Controlled by `gc_auto_enabled` (default true)

### POST /gc

Runs GC for the authenticated user.

- **Parameters:** `dry_run`, `aggressive`, `retention_days`, `max_manual_pushes`
- **Returns:** `GCResult` with per-phase breakdowns

### GET /gc/stats

Returns usage info and GC state:

- `usage_bytes`, quota info, `gc_state`, `gc_lock` status
- `recent_events` (last 10 from `gc.jsonl`)
- `inflight_epochs` count

## node.yaml Configuration

```yaml
gc:
  retention_days: 7 # daily checkpoints to keep
  max_manual_pushes: 3 # manual push snapshots to retain
  max_executions_per_graph: 10 # execution records per graph
  cache_max_age_hours: 24 # cache snapshot max age
  auto_gc_enabled: true # enable quota-triggered auto-GC
  auto_gc_cooldown_seconds: 600 # minimum interval between auto-GC runs
  grace_window_seconds: 3600 # sweep grace period for new objects
```

Maps to Settings fields: `gc_retention_days`, `gc_max_manual_pushes`, `gc_max_executions`, `gc_cache_max_age_hours`, `gc_auto_enabled`, `gc_auto_cooldown`, `gc_grace_window`.

## Observability

- Every GC run writes a structured JSON event to `user_root/logs/gc.jsonl`
- Events include per-phase metrics: cache entries deleted, snapshots compacted/retained, objects/blobs swept, bytes freed, duration
- GC state persisted for incremental runs: `last_gc_at`, reachable count, generation counter
- State invalidated when compaction discards snapshots (forces full mark on next run)

## Incremental GC

- After a full mark, the reachable set is stored as a CAS blob
- Subsequent runs can load the previous reachable set and only re-mark from changed/new roots
- Invalidated when compaction discards snapshots or GC state is manually cleared

## Production Results

First production deployment on track-blox:

- **Remote (server):** 1.8 GB / ~198K objects / 237 snapshot chain → 0.7 GB / 5 retained snapshots / 198,231 objects swept, 1.1 GB freed in 53 seconds
- **Local:** 50 MB / 7,463 objects → 16.5 MB / 128 reachable

## Implementation Files

| Component         | File                                                        |
| ----------------- | ----------------------------------------------------------- |
| GC engine         | `ryeos/rye/cas/gc.py`                                       |
| Type definitions  | `ryeos/rye/cas/gc_types.py`                                 |
| Writer epochs     | `ryeos/rye/cas/gc_epochs.py`                                |
| Distributed lock  | `ryeos/rye/cas/gc_lock.py`                                  |
| Incremental state | `ryeos/rye/cas/gc_incremental.py`                           |
| Rye tool          | `ryeos/bundles/core/ryeos_core/.ai/tools/rye/core/gc/gc.py` |
| Server endpoints  | `ryeos-node/ryeos_node/server.py`                           |
| Server config     | `ryeos-node/ryeos_node/config.py`                           |

## GC Invariants

1. **Durable roots** = all current refs: project HEADs, user-space HEAD, pins
2. **Ephemeral roots** = all active writer epochs in `inflight/`
3. **Protected set during sweep** = durable reachable ∪ ephemeral reachable ∪ objects newer than grace window
4. Compaction invalidates incremental state — next mark must be full
5. Pinned snapshots preserve exact original hash — compaction never rewrites pinned objects
6. Current HEAD policy governs compaction — no historical per-snapshot policies
