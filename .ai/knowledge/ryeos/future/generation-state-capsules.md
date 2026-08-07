<!-- ryeos:signed:2026-08-07T10:40:48Z:a453f9fa66d3262832cfa4329f68e99fe7ae18d9b44b42285efdadf24a1bd832:LJz+U48AjvcwPltUlvvve3W9ra5UcfMIz3IdZbFJa3AlMqge62fBrUe7QdxFCDb4Wcfg3fA2l9qLlhl/3K4ODw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, inference, checkpoint, capsule, tinygrad, search, sealed]
version: "0.1.0"
status: draft
description: >
  Checkpoint, park, resume, and fork for in-flight local generation: KV
  cache, RNG state, and token position as a content-addressed capsule —
  with the prefix cache falling out as the degenerate checkpoint.
---

# Generation-state capsules

Under sealed local inference a turn stops being atomic: generation state
is tensors — KV cache, RNG state, tokens so far — and tensors checkpoint.
This note designs the capsule that carries that state, and the three
operations it enables: park a canceled turn instead of discarding it,
resume bit-identically, and fork at a token with sealed provenance per
branch. The graph runtime already has park-tree, continuation capsules,
and `segment_steps`; this is the same machinery reaching inside the
model.

## The capsule

A content-addressed object:

- **scope** — the program digest and the **execution identity**
  (`knowledge:ryeos/future/execution-identity`), both required. A
  checkpoint is sealed-class state: its bytes are meaningful only where
  weights, kernels, and numerics match. On a foreign identity a capsule
  is not resumable and never pretends to be — the graceful path is the
  turn's provider-call record (replay the finished turn) or live
  recomputation from the last durable boundary.
- **position** — the token offset, the RNG state at that offset, and the
  digest of the token sequence up to it. History-as-counter, as
  everywhere: the sequence digest is the identity of "where in the
  generation," so two checkpoints at the same offset of different
  generations can never be confused.
- **state payload** — the KV tensors, stored through the semantically
  blind large-object store
  (`knowledge:ryeos/future/weights-tier-realizations`): contiguous,
  mmap-ready, streaming-verified at write. The capsule holds hashes,
  never tensors.
- **lineage** — the parent capsule hash when this checkpoint continues or
  forks another. The search tree is literally the capsule lineage graph.

## The unification: prefix cache is the degenerate checkpoint

A KV-prefix cache entry is a checkpoint with no partial output: position
at the end of a shared prompt prefix, RNG untouched. Same object, same
store, same keying by (execution identity, weights, kernels, sequence
digest). There is no separate prefix-cache subsystem to build — sharing
prompt-prefix computation across runs and callers is capsule reuse, and
every property below (retention, foreign-identity refusal, lineage)
applies to it for free.

## Operations

- **Park.** An interrupt at a checkpoint boundary seals the capsule and
  parks the turn exactly as a graph parks at a continuation boundary. A
  crash re-pays tokens since the last checkpoint, not the turn.
- **Resume.** Admission verifies scope (program digest and execution
  identity match), restores KV and RNG, and continues. Bit-identity with
  the uninterrupted run is the sealed-class contract, and a divergence
  is a substrate-integrity finding.
- **Fork.** A new generation admitted *from* a capsule with a divergent
  continuation — different seed, injected tokens, different sampler
  config. The child seals the parent hash; branch provenance is
  structural. Best-of-N and tree search are fork loops, and rejected
  branches remain re-derivable evidence.

## Boundaries and economics

KV runs to gigabytes at long context, so checkpoints land at declared
boundaries, never per token: a `segment_tokens` cadence in the sealed
declaration, plus the natural free boundaries — tool-call pauses and
turn ends, where generation already stops. Retention is the cheapest
lane in the system, because capsules are pure derived state: under a
matching identity they are re-derivable, so eviction is never loss of
evidence, only loss of time. Two protections stand above LRU: a parked
thread's newest capsule is rooted by its thread row (parking must
survive the sweep), and an active generation's capsule chain is leased.
Speculative search branches age out first by construction.

## What this means

- **Cancellation is costless** — canceled work is parked work.
- **Search is native** — the solver forks reasoning at token granularity
  with provenance, which is the ARC campaign's deep lever.
- **Deploy windows shrink** — a clear window needs solves parkable, and
  parkable now reaches inside a turn instead of waiting for one to end.
- **Chat products** get cross-caller prompt-prefix sharing as capsule
  reuse under a shared execution identity.

## Increments

0. Capsule object + canonical digest in ryeos-state (scope, position,
   payload hashes, lineage), decode discipline as everywhere.
1. Prefix-as-checkpoint in the tinygrad runtime: write a capsule at
   prompt-end, look one up before prefill — the cheapest win and the
   proof of the store path.
2. Park/resume at declared boundaries, wired to the existing interrupt
   and continuation surfaces.
3. Fork, with lineage sealed and a bounded fan-out declared like every
   other budget.
4. Retention lanes: thread-rooted parked capsules, leased active chains,
   LRU for the speculative rest.

## Triggers to revisit

- The tinygrad runtime lands (increment 1 becomes buildable);
- the first real solver wants mid-turn fork budgets;
- KV size at target context lengths makes `segment_tokens` defaults or
  the large-object budget wrong in practice.
