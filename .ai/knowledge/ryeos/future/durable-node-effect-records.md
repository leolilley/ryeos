<!-- ryeos:signed:2026-08-12T07:28:15Z:f3e973ba9511f9239678afedcea11b2781ac70885ef20817d92cff471fede804:VMJWhMqYg9NFcILOCrcushTo2pUCbZ5Y4yPshSsRBFN3NBMOkjgi2j8J2yeJ/tzWaMfm3427Vk8MxFoqx01GBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, determinism, replay, cache, evidence, graph]
version: "0.1.1"
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
    action: { item_id: "tool:example/probe", ... }
```

Default `live` keeps every existing graph's semantics byte-identical until an
author declares otherwise. The declaration composes and seals like everything
else that determines behavior.

## Author covenants

Two properties the contract requires of a `recorded` node, surfaced by the
first adoption review:

1. **Result completeness.** Replay serves the stored result and executes
   nothing: any write the action performs — workspace output, a knowledge
   append, external mutation — silently does not happen on a replayed
   dispatch. A node is eligible for `recorded` only when its result
   envelope IS its entire observable effect. Anything else is `live`.
2. **Run-stable action payloads.** The action payload is the record's
   identity. A run-scoped value in the params — a thread id, a timestamp, a
   fresh token — makes every key unique: a permanent miss on every run and
   a one-shot record published per dispatch. Retention prunes never-replayed
   rows before replayed ones, so that churn cannot evict a banked record,
   but the misses and the index pressure remain the author's bill. Keep
   run-scoped values out of recorded nodes' params, or leave the node
   `live`.

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
5. **Retention**: the materialization-cache pattern — quota + sweep under
   the state authority, pruning never-replayed rows (publication churn)
   before the least-recently-replayed, no reachability edges in v1. Evicting a
   `recorded` record is honest loss: the next run executes live and is a
   different run, and evidence referencing the record hash detects the
   absence rather than silently absorbing it. Banked evidence that must pin
   its records durably for certification adds closure edges in a later
   increment.

## Non-goals (v1)

- No cross-digest reuse, ever — the key is the contract.
- No LLM-response caching; provider calls inside directives are a different
  boundary with its own record design (the hook ledger already covers hook
  children).
- No distributed record sharing; single node, like every cache here.

## Increments

0. **CLOSED, definitionally** — the hook dispatch ledger is recorded-class
   by construction: the table is the record store for that boundary, its
   rows carry their own seed version, and a replay engine maps
   table → class statically. A redundant marker column in a
   CHECK-constrained ledger would be churn without a consumer. Any
   project-specific recorded simulator adoption belongs to its consumer.
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
3. **DONE (`fc7380d6f`)** — built exactly as mapped, plus replay
   provenance threaded end-to-end immediately (envelope → ActionSuccess →
   ActionOkOutcome → NodeReceipt.replayed_from → persisted receipt)
   rather than deferred: dead wire fields were refused on review. Two
   directive-runtime request literals and four runtime test literals were
   the compiler's catches beyond the map. Original map:
   - **Wire** (`ryeos-runtime/src/callback.rs`): optional
     `effect_replay: Option<EffectReplayRequest { node, class }>` on
     `DispatchActionRequest`. The runtime never names its own cache key —
     a lying runtime could otherwise poison the index. The daemon-minted
     callback capability already carries
     `cap.effective_definition_digest` and `cap.item_ref`; those plus the
     shared `ActionPayload` are the whole derivation input.
   - **Shared derivation** (same file; lillux is a dependency): a
     schema-tagged canonical-JSON seed
     (`ryeos.node_effect_record.key.v1` over digest, root ref, node,
     ActionPayload) → sha256. Identical by construction on both sides
     because both hash the wire type, not a private value. In-run
     `compute_cache_key` stays untouched — different scope, different key,
     no compatibility to manage.
   - **Handler** (`executor/src/execution/runtime_dispatch.rs`, inline
     path only — the `thread == "detached"` branch at ~415 is excluded,
     matching the definition-layer refusal): after capability and hook
     preflight, when `effect_replay` is present and the cap carries a
     digest — derive key, `lookup_effect_record`; on hit, load the object
     (`GraphNodeEffectRecord::from_current_value`, verify stored
     `cache_key` equals the derived key), `touch`, and return
     `record.result` with `replayed_from: <hash>` injected; on miss, run
     the normal inline dispatch, and on a successful envelope store the
     object (CAS write under the pinned authority, then index row — a
     crash between leaves an orphan the sweep collects) via
     `publish_effect_record`. Crash-replay after publish is not a fence
     violation for durable records: replaying the record is the intended
     semantics, unlike the in-run cache whose checkpoint fence remains.
   - **Envelopes**: the graph runtime's strict response parsers
     (`runtimes/graph/src/dispatch.rs` — `SubprocessResultEnvelope`,
     `NativeResultEnvelope`, both `deny_unknown_fields`) gain optional
     `replayed_from`; only replay-requesting dispatches ever see it, so
     no other runtime's parser is touched.
   - **Runtime request + receipt**: `dispatch_action`
     (`runtimes/graph/src/dispatch.rs:130`) takes the node's
     `EffectClass` from its two call sites
     (`walker/execution.rs:336,357`, where the node is in scope), sets
     `effect_replay` for non-live classes, and threads `replayed_from`
     from the envelope into the node receipt beside `cache_hit`.
   - **Record loader**: a `state_store` helper following
     `admitted_launch_capsule`'s load pattern.
4. **DONE** — retention prunes the least-recently-replayed rows beyond a
   10k cap inside maintenance GC, before root gathering, so un-rooted
   record objects are sweepable in the same pass; replay provenance
   reaches the UI with zero code, because receipts flow as opaque bounded
   artifact metadata and now carry `replayed_from` only when a record was
   actually served.
5. Measure on a representative completed evaluation: count replayed nodes and
   saved cost/latency. That number decides how hard to push increment 6
   (cross-digest *analysis* — never reuse — for divergence attribution).

## Measurement gate

Implement through increment 3, then measure before polishing: re-executing a
completed evaluation under an unchanged digest should replay its
probe/measurement nodes. If the replay rate on real repeated runs is not
material, this stays a determinism instrument (still worth increment 2–3 for
divergence proofs) rather than a performance feature — and the honest
conclusion is recorded here either way.
