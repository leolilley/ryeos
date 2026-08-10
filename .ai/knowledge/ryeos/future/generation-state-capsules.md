<!-- ryeos:signed:2026-08-10T03:16:08Z:4f75a2e58e34732016d40e5e635bf5d0fea00b3f379f0816bc23fde819bc2b6b:RqMUJ5J+rE3vjBBVxZU+rQfLW38QWB//hW0Fovx3+BVYGg/YQ+4ytOn3/TlxmV5+z5EkLkU25JPdbWP9vlsOBg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, inference, checkpoint, capsule, tinygrad, search, sealed]
version: "0.2.0"
status: deferred
description: >
  Checkpoint, park, resume, prefix reuse, and fork for qualified local
  generation, with provider-owned tensor semantics over a meaning-blind RyeOS
  capsule/index/retention substrate.
---

# Generation-state capsules

## Current boundary

RyeOS has a recorded local Tinygrad route, external large-content storage,
admitted persistent sessions, execution realizations, durable stages, immutable
effect indexes, and graph continuation machinery. It does **not** yet have a
qualified sealed local route or a generation-state capsule.

This work begins only after positive sealed qualification. A retained terminal
answer proves replay of the completed effect; it does not prove that an
in-flight KV/RNG state can resume bit-identically.

## Ownership split

The generic capsule substrate is meaning-blind. It may commit only to:

- owning contract/ref and admitted execution realization;
- an opaque canonical coordinate digest and payload-schema digest;
- payload CAS objects, CAS blobs, and distinct large-object hashes;
- parent checkpoint and fork lineage;
- an opaque bounded continuation-state digest;
- an owning-contract-declared safe boundary;
- tenant/privacy scope, leases, budgets, and retention state.

The provider-generation contract owns:

- model/runtime state schema and compatibility;
- request prefix and logical token position;
- token sequence, KV tensor layout, and sampler/RNG state;
- the exact derivation of coordinate and payload digests;
- semantic validation of park/resume/fork; and
- whether a boundary is safe for the admitted runtime.

None of those provider nouns enter generic state, CAS traversal, operational
indexes, executor code, or the `worker` kind.

## Capsule behavior

### Prefix reuse

A prompt-end prefix is the degenerate checkpoint: no partial output, RNG at its
initial generation state, and a provider-owned coordinate committing to the
qualified realization and exact prefix. The daemon index treats it like any
other opaque capsule. Cross-principal sharing is forbidden unless explicit node
policy creates a broader trusted domain; lookup timing must not become a prompt
membership oracle.

### Park and resume

At an admitted token boundary the worker stages the complete opaque payload;
the daemon independently verifies its declared closure, atomically publishes
the capsule/index entry, and roots the parked capsule through the owning thread.
Exact resume requires the same qualified realization and provider coordinate.
Missing/incompatible content fails closed. A caller may separately request a
fresh generation, but an exact resume never silently restarts.

### Fork

A fork seals its parent capsule and a new provider-owned continuation
coordinate. Bounded fanout/depth are admitted before publication. Rejected
speculative branches are derived performance state, not primary execution
evidence, and age out before parked/leased capsules.

## Atomicity and retention

Publication extends the existing durable stage across CAS objects, blobs, and
large objects. A capsule/index entry becomes visible only after the complete
closure is retained. Active worker/thread leases and parked-thread roots block
retirement. Per-thread and per-principal count/byte ceilings plus fanout/depth
budgets are checked before content becomes durable authority.

Crash recovery must cover every boundary: stage before index, index before root,
root before response, active lease loss, and retirement racing a reader. An
exact duplicate folds; divergent content under one coordinate fails closed.

## Implementation increments

0. **Qualification prerequisite.** Prove the target local route sealed under a
   retained realization; otherwise this package remains blocked by design.
1. **Generic capsule contract.** Current-schema object, opaque coordinate and
   payload digests, typed closure edges, lineage, tenant scope, and strict
   bounds. No provider vocabulary in state.
2. **Operational index and staging.** Immutable answer semantics, atomic
   CAS/blob/large publication, GC roots, leases, and crash matrix.
3. **Provider adapter.** Tinygrad derives/validates prompt-prefix coordinates
   and opaque payloads; daemon never parses KV/tokens/sampler state.
4. **Prefix acceptance.** Fresh process reuses a retained prompt-end capsule
   with zero prefill work and exact realization/privacy proof.
5. **Park/resume.** Interrupt and recovery at declared boundaries; exact
   resumed output matches uninterrupted generation.
6. **Fork and retention.** Bounded lineage, speculative lanes, field
   projection of identity/lineage only, and budget/eviction acceptance.

## Triggers to revisit

- sealed local qualification passes on a target node;
- ARC search needs prefix reuse, mid-turn park/resume, or token-level fork;
- retained KV sizes establish realistic segment and storage budgets; or
- deployment windows need in-turn parking rather than turn-boundary replay.
