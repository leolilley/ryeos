# GC Advanced Path: Beyond Write Barriers

**Date**: 2026-04-23
**Status**: Future reference — not for current implementation.
**Prerequisite**: GC-DESIGN.md (maintenance mode GC) must be
implemented and proven insufficient before any of this applies.

---

## When to evolve

The current GC design uses a global write barrier: daemon pauses
all durable CAS writes, runs GC, resumes. This is correct and
simple. Only evolve beyond it when one of these conditions is met:

| Condition                       | Symptom                                                    |
| ------------------------------- | ---------------------------------------------------------- |
| GC pause > 5-10s                | Users/agents experience noticeable write latency during GC |
| CAS store > millions of objects | BFS reachability traversal itself becomes the bottleneck   |
| Multiple writers introduced     | Single-writer invariant relaxed (CRDTs, multi-node merge)  |
| Continuous GC needed            | Store grows fast enough that periodic GC can't keep up     |

If none of these are true, the write barrier is the right answer.
Don't over-engineer.

---

## Evolution 1: Mark Cache (external)

**Trigger**: BFS in `collect_reachable()` takes >10s.

**Design**: Store the mark cache OUTSIDE CAS, at
`state_root/cache/gc-mark-cache.json`. Never in CAS — it's
operational metadata, not truth.

```json
{
  "version": 1,
  "created_at": "2026-04-23T...",
  "root_digest": "<sha256 of sorted signed head target_hashes>",
  "reachability_schema": 1,
  "reachable_objects": ["hash1", "hash2", ...],
  "reachable_blobs": ["hash3", "hash4", ...]
}
```

**Cache key**: SHA256 of sorted `target_hash` values from all
signed head refs (chains + projects). If any head moves, the
digest changes, cache invalidates.

**Schema version**: Include `reachability_schema` so the cache
auto-invalidates if `extract_child_hashes()` logic changes
(e.g., new object kind added).

**Usage in GC**:

1. Compute current root_digest from signed heads
2. If cache exists and root_digest matches and reachability_schema
   matches → use cached reachable set for sweep
3. Otherwise → full BFS → write new cache

**Why not in CAS**: The mark cache stored in CAS creates a
self-referential loop — the cache object is itself reachable (or
not), which changes the reachable set. The old code worked around
this by filtering the cache's own hash from root comparison, but
it's architecturally wrong. GC metadata belongs in ephemeral
storage.

---

## Evolution 2: Incremental Sweep

**Trigger**: Sweep disk walk takes too long (millions of files
in objects/ and blobs/).

**Design**: Track "last sweep watermark" — a timestamp or
generation number. Only scan files modified/created since the
last sweep.

```json
// state_root/cache/gc-sweep-watermark.json
{
  "version": 1,
  "last_sweep_at": "2026-04-23T...",
  "last_sweep_generation": 42
}
```

On each sweep, only walk shard directories for files newer than
the watermark. Combined with mark cache, this makes GC
O(new objects) instead of O(total objects).

**Limitation**: Cannot detect corruption (deleted or modified
files) — only finds new garbage. Periodic full sweep still
needed for integrity.

---

## Evolution 3: Fully Concurrent GC (no write barrier)

**Trigger**: Write barrier pause is unacceptable. Active agent
executions can't tolerate any write latency.

**Why old epochs don't work**: Mtime-based epoch protection has
a fundamental hole in CAS-as-truth. When a ref update makes an
existing object newly reachable, no new file is created — the
object already exists in the store. The epoch mtime grace window
doesn't see it. The sweep can delete a live object.

Example race:

```
T=0  GC mark phase: object X is unreachable (not in reachable set)
T=1  Writer: update signed ref to point to chain_state that
     references snapshot that references object X
     (X already exists in CAS — no new file write, no mtime change)
T=2  GC sweep phase: X has old mtime, not in reachable set → DELETE
T=3  Reader: follows signed ref → chain_state → snapshot → X → MISSING
```

This race is inherent to mtime-based schemes in a content-addressed
store where objects are deduplicated.

### Correct design: Logical generations

Replace mtime-based epochs with logical generation numbers. Every
CAS write (object store or ref update) increments a global
generation counter.

```rust
pub struct GenerationCounter {
    current: AtomicU64,
}

impl GenerationCounter {
    /// Increment and return the new generation.
    pub fn advance(&self) -> u64;

    /// Read the current generation without advancing.
    pub fn current(&self) -> u64;
}
```

#### New object staging

New objects are written with their generation number recorded:

```
state_root/objects/generations/<generation>.manifest
  → list of (hash, namespace) pairs written in this generation
```

Or simpler: objects written after GC mark started are in the
"safe set" — never swept in this GC cycle.

#### GC protocol (tri-color marking adapted for CAS)

```
1. Record mark_start_generation = current_generation
2. Mark phase: BFS from signed heads → build reachable set
   (runs concurrently with writes)
3. Re-mark phase: BFS again from any refs that changed DURING
   mark (generation > mark_start_generation)
   Repeat until no new refs changed (convergence)
4. Sweep phase: delete objects that are:
   - NOT in reachable set
   - AND were created before mark_start_generation
   (objects created during or after mark are safe — they might
   be reachable from refs we haven't seen yet)
```

The key insight: **never sweep objects newer than mark_start_generation**.
This replaces the mtime-based epoch grace period with a logically
correct boundary.

#### Why this is safe

- Objects created before mark_start_generation: if they're
  reachable, the mark phase found them. If a ref update during
  mark made them newly reachable, the re-mark phase catches it.
- Objects created during/after mark: protected by generation
  cutoff. They'll be evaluated in the next GC cycle.
- Ref updates during mark: the re-mark phase converges because
  the set of refs is finite and monotonically increasing (no
  ref deletions in the single-writer model).

#### Complexity cost

- Global generation counter (trivial)
- Generation manifest or per-object generation tracking
- Re-mark convergence loop
- More complex correctness reasoning
- Testing concurrent mark + write scenarios

**Recommendation**: Only implement this if the write barrier
pause exceeds 5-10 seconds AND reducing the store size or
increasing GC frequency can't bring it down.

---

## Evolution 4: Background Continuous GC

**Trigger**: Store growth rate exceeds what periodic GC can
handle. Objects accumulate faster than GC can collect.

**Design**: Daemon runs GC continuously in a background task.
Uses the fully concurrent GC protocol (Evolution 3). GC is
always running, sweeping objects from previous generations while
new objects are written.

```rust
// In daemon
tokio::spawn(async move {
    loop {
        let result = concurrent_gc_cycle(&cas_root, &refs_root).await;
        log_gc_event(&result);
        // Adaptive sleep based on how much garbage was found
        let sleep = if result.freed_bytes > 0 {
            Duration::from_secs(60)
        } else {
            Duration::from_secs(300)
        };
        tokio::time::sleep(sleep).await;
    }
});
```

This is the most complex model. Requires Evolution 3 as a
prerequisite. Only consider if RyeOS is running long-lived
agent chains that generate significant CAS churn.

---

## Evolution 5: Chain History Compaction

**Trigger**: chain_state objects accumulate significantly.
Each chain_state.prev_chain_state_hash forms a linear history.
If chains run for thousands of state transitions, the history
grows unbounded.

**Design**: Same algorithm as project snapshot compaction
(memoized recursive DAG rewrite), applied to the chain_state
history chain. Keep the last N chain_states, rewrite
prev_chain_state_hash pointers to skip removed states.

**Why deferred**: Chain states are small JSON objects. Project
snapshots reference full source manifests (potentially thousands
of items). The growth pressure is on project snapshots, not
chain states. Revisit if chain histories reach >10k states.

---

## Decision record

| Evolution         | Complexity | Prerequisite  | When                         |
| ----------------- | ---------- | ------------- | ---------------------------- |
| Mark cache        | S          | None          | BFS > 10s                    |
| Incremental sweep | S          | Mark cache    | Millions of objects          |
| Concurrent GC     | L          | None          | Write barrier > 5-10s        |
| Background GC     | L          | Concurrent GC | Growth rate > GC rate        |
| Chain compaction  | M          | None          | Chain histories > 10k states |
