<!-- ryeos:signed:2026-08-27T04:21:32Z:f1b5235010616d1eace03abe3492a6e51ba04cec0abc6a4b66c5b84b3effacb3:3N7W76WA06LluX5dlKmsdYCZSXPhCZ1ac/7zk3Dkk16VUA8043U1uQvy8x3kEiDeR4drFDhjtXz0oa0bz0tXAg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, inference, checkpoint, capsule, tinygrad, search, sealed]
version: "0.3.0"
status: scheduled
description: >
  Opaque content-addressed prefix, park/resume, and fork state for admitted
  local generation, with recorded continuation before qualification and exact
  equivalence only after sealed proof.
---

# Generation-state capsules

## Current boundary

RyeOS has recorded tinygrad inference, exact external large content, persistent
sessions, execution realizations, durable staging, immutable effect indexes,
thread continuations, and generic retention. It does not yet publish a
generation-state capsule.

The earlier design blocked all capsules on sealed qualification. That
conflated useful integrity-checked continuation with the stronger claim that a
resumed generation equals an uninterrupted one. The corrected design supports
two explicit classes:

- **recorded capsule:** compatible retained state used for prefix reuse,
  recorded resume, or recorded fork; no equivalence claim;
- **qualified capsule:** a matching sealed provider contract has proved
  interrupted and uninterrupted generation byte-identical.

## Ownership split

The generic capsule substrate is meaning-blind. It owns only:

- owning contract/profile and admitted execution-realization refs;
- opaque canonical coordinate and payload-schema digests;
- payload CAS objects, blobs, and large-object hashes;
- parent checkpoint and fork lineage;
- opaque continuation-state digest and declared safe boundary;
- tenant/project/privacy scope;
- staging, immutable indexes, leases, budgets, retention, and GC roots; and
- recorded or qualified capability testimony.

The local-generation contract owns:

- model/runtime state schema and compatibility;
- exact prompt prefix and logical token position;
- token sequence, KV tensor layout, and sampler/RNG state;
- derivation and validation of coordinates and payloads;
- whether a boundary is safe; and
- the proof required to claim exact resume/fork.

Those provider nouns do not enter generic state, CAS traversal, operational
indexes, executor code, or the `worker` kind.

## Prefix reuse

A prompt-end prefix is a checkpoint with no partial output and the sampler at
its declared initial state. Its provider coordinate commits to the exact
profile, execution realization, canonical prefix tokens, state schema, and
privacy domain.

Recorded prefix reuse may skip prefill for a new recorded request. The provider
validates compatibility and RyeOS records the capsule hit; it does not claim
the resulting output would match a full fresh prefill unless that exact path is
qualified.

Cross-principal sharing is forbidden unless explicit node policy creates a
broader trusted domain. Lookup timing must not become a prompt-membership
oracle.

## Park and resume

At a declared token boundary the worker stages the complete opaque payload.
The daemon verifies its closure, publishes the capsule/index atomically, and
roots it through the owning execution.

- A recorded resume explicitly continues from that state and produces a new
  recorded consequence.
- An exact resume is admitted only under a matching qualified capsule
  contract.
- Missing or incompatible content fails closed.
- Exact resume never silently restarts. A caller may separately request a
  fresh generation under a new invocation/effect coordinate.

## Fork

A fork commits to the parent capsule and a new provider-owned continuation
coordinate. Fanout, depth, storage, compute, and retention budgets are admitted
before publication. Recorded forks remain useful for broad search and outcome-
based selection; qualified forks add reproducible branch derivation.

Rejected speculative branches use a cheaper retention lane than selected
evidence, parked user work, or corpus-rooted traces.

## Atomicity and recovery

Publication extends the existing durable stage across CAS objects, blobs, and
large objects. A capsule/index entry becomes visible only after the complete
closure is retained. Active process/thread leases and parked/corpus evidence
roots block retirement.

Recovery covers stage-before-index, index-before-root, root-before-response,
lease loss, worker death, daemon restart, and retirement racing a reader. An
exact duplicate folds; divergent content under one coordinate is an integrity
failure.

Per-thread, principal, profile, and node byte/count limits plus fork fanout and
depth budgets are checked before publication.

## Qualification

After the local route earns sealed qualification, compare:

1. one uninterrupted generation;
2. one generation parked at a declared boundary and resumed in a fresh
   process; and
3. any qualified prefix/fork paths the contract intends to claim.

All use the same exact program, execution identity, input, sampler, and closed
artifact set. Only byte-identical results permit the qualification to grant
exact capsule semantics. Other capsules remain recorded, not invalid.

## Implementation increments

1. Generic opaque capsule object, closure edges, lineage, scope, and bounds.
2. Staging, immutable index, leases, recovery, retention, and GC integration.
3. Tinygrad provider coordinate/payload validation.
4. Recorded prompt-prefix reuse with measured prefill savings.
5. Recorded park/resume and bounded fork with honest evidence.
6. Sealed interrupted-versus-uninterrupted qualification.
7. Execution-field projection of identity, class, lineage, hit/miss, bytes, and
   saved/repaid work without exposing KV payloads.

## Triggers

- serious-model prefill or restart cost is material;
- a search workload needs bounded generation forks;
- long turns need checkpoint-bounded recovery; or
- sealed qualification is ready to prove exact resume rather than merely
  integrity-checked continuation.
