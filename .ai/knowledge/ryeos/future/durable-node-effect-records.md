<!-- ryeos:signed:2026-08-07T03:26:07Z:634e8ccbc259c406fca50da93ff4302bb994876a9b93efd13cf559a0e3dd3a80:theVdF6g1QgSvOm7OXJ51Mvv5hshzA2tWsfDJRzQFqRvfqu+sCGcxoVbwsLt3xA/LPn5eWZM3ZgsQOfiW1NWAg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, replay, cache, evidence, graph]
version: "0.1.0"
status: implementable
description: >
  Cross-run replay of graph node results as recorded-class effect records:
  the first implementation of the determinism-classes contract, built on the
  now-truthful node cache identity.
---

# Durable node effect records

The graph runtime already caches node results within one execution, keyed by
`(effective_definition_digest, graph_id, node, canonical_action)`
(`walker.rs::compute_cache_key`), consulted on every dispatch, published only
after the advancing checkpoint is authoritative. Its own comment claims the
digest "prevents changed executable behavior from reusing an entry."

**External content realization made that claim true** (2026-08-07): the
digest now names the tool bytes a node's action executes, so equal keys mean
equal executable behavior for everything declared. The two reasons the cache
was scoped to a single execution are therefore in different states:

- *staleness* — dead. A tool-library edit moves the digest; a stale entry
  cannot be keyed.
- *forgery* — solvable with landed machinery. Entries become CAS objects
  under the pinned node-state authority, which no project or sandbox write
  can reach.

What remains is the honest gate: **eligibility**. Digest equality covers
declared bytes only. A node whose action calls a provider, reads the network
or the clock, or touches ambient content must not replay across runs on key
equality alone. That gate is exactly the determinism-classes contract
(`knowledge:ryeos/future/determinism-classes`) arriving as mechanism — this
design is that contract's first implementation, not a new cache.

## Decision

A durable node effect record is a **recorded-class effect record** for one
graph node action: a CAS object under node-state authority holding the exact
result the daemon observed, bound to the cache key. Cross-run "cache hit" is
redefined as **record replay** under the effect-class contract:

- `sealed` actions may re-derive or replay — divergence is a substrate bug;
- `recorded` actions replay the record; executing live instead is a
  *different run*, never a replay;
- `live` actions never replay across runs (in-run reuse keeps today's
  session-replay semantics unchanged).

Durability is opt-in per node, authored and signed:

```yaml
nodes:
  probe_grid:
    effects: recorded          # sealed | recorded | live (default: live)
    action: { item_id: "tool:arc/probe", ... }
```

Default `live` keeps every existing graph's semantics byte-identical until an
author declares otherwise. The declaration composes and seals like everything
else that determines behavior.

## Mechanism

1. **Placement: daemon-side, at child dispatch.** The runtime already
   computes the cache key and already dispatches children through the daemon
   callback. It attaches the key and declared class to the dispatch; the
   daemon consults the record store and either returns the record (marked
   `replayed_from: <record_hash>`, zero cost, no spawn) or executes,
   settles, and publishes the record. Authority never enters the sandbox;
   the runtime cannot forge what it cannot write.
2. **Object**: `graph_node_effect_record` — `{schema, kind, cache_key,
   effective_definition_digest, graph_id, node, action_digest, class,
   result, produced_by_thread, receipt_identity}`. Content-addressed;
   a state-store index maps `cache_key → record hash` (the admitted-capsule
   indexing pattern).
3. **Publication fence**: unchanged in spirit from today's in-run rule —
   a record becomes durable only after the producing run's checkpoint is
   authoritative *and* the child's settlement is acknowledged. The existing
   terminal-node hold generalizes: no settlement acknowledgement, no record.
4. **Evidence**: a replayed node's receipt carries `replayed_from` instead of
   a false `cache_hit`; accounting bills nothing and records provenance.
   Divergence checking gets its instrument for free: re-executing a `sealed`
   node and comparing against its record is the typed divergence proof from
   the determinism-classes note.
5. **Retention**: the materialization-cache pattern — quota + LRU sweep
   under the state authority, no reachability edges in v1. Evicting a
   `recorded` record is honest loss: the next run executes live and is a
   different run, and evidence referencing the record hash detects the
   absence rather than silently absorbing it. Banked evidence that must pin
   its records durably (ARC certification) adds closure edges in a later
   increment.

## Non-goals (v1)

- No cross-digest reuse, ever — the key is the contract.
- No LLM-response caching; provider calls inside directives are a different
  boundary with its own record design (the hook ledger already covers hook
  children).
- No distributed record sharing; single node, like every cache here.

## Increments

0. Stamp effect-class markers on existing recorded evidence (pull-forward
   item from determinism-classes; additive).
1. **DONE (`ec264c155`)** — `effects: sealed|recorded|live` on graph nodes:
   one shared enum across both strict decoders, default `live` off the wire
   (checkpoints byte-stable), foreach/follow/detach refuse durable classes,
   declaration seals into the digest via `config`.
2. **DONE** — object `1056f2926`, index `d6dc668ca` (operational schema
   v2, publish/lookup/touch/list/delete, both GC gatherers feeding
   `AdditionalCasRoots`). Placement reasoning preserved below:
   - `StateDb` is a rebuildable projection — an index there cannot root CAS
     objects (circular with GC across a rebuild).
   - `OperationalDb` has the root-feed precedent (`Mirrored` entries →
     `AdditionalCasRoots`) but a strict exact-schema gate: a new table is a
     version bump and an operational reset on every node.
   - `runtime_db` is additive and holds the hook ledger, but it carries no
     GC-root authority and its loose schema is a signal that nothing
     identity-bearing lives there — a first consideration of it was
     convenience, not fit.
   - **Correct home: `OperationalDb`** — the durable operational ledger
     whose job already includes CAS-entry tracking and GC root feeds.
     `OPERATIONAL_SCHEMA_VERSION` 1→2 with
     `effect_records(cache_key PRIMARY KEY, record_hash, produced_at,
     last_replayed_at)` as its own root source beside `Mirrored` (the
     `cas_entries` states model sync lifecycle and are not overloaded).
     Both gatherers — `ryeos-api/src/maintenance.rs` (~340) and
     `ryeos-app/src/offline_gc.rs::inspect_operational_gc_roots` —
     enumerate its hashes into `AdditionalCasRoots`. The version bump
     resets operational state on install per the clean-cut law; losing the
     ledger degrades to live re-execution, the retention semantics already
     accepted above.
3. Daemon dispatch interception: replay-or-execute-and-record, settlement
   fence, `replayed_from` receipts (executor + runtime callback protocol).
4. Retention sweep (pattern exists) + `field/runs` projection of replay
   provenance.
5. Measure on ARC: re-solve a solved game; count replayed nodes and saved
   cost/latency. That number decides how hard to push increment 6
   (cross-digest *analysis* — never reuse — for divergence attribution).

## Measurement gate

Implement through increment 3, then measure before polishing: an ARC
re-solve of an already-solved game under an unchanged digest should replay
its probe/measurement nodes. If the replay rate on real re-solves is not
material, this stays a determinism instrument (still worth increment 2–3 for
divergence proofs) rather than a performance feature — and the honest
conclusion is recorded here either way.
