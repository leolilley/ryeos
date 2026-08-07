<!-- ryeos:signed:2026-08-07T08:08:24Z:9f8ed63dca88e6a56de7d018df94dc01762b7cbbc5502f774faa0bd78c02d8c8:PdCXtk7VAJb0mpXoKqCe2pqcO6LrOOvlSJWZe4OF4BwhNt/oPf2PJKjShIkkPgW2hpyowOaRZE4B/YzwAlodCw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, inference, tinygrad, sealed, replay, arc]
version: "0.1.0"
status: draft
description: >
  The destination for LLM execution in ryeos: local inference on tinygrad as
  sealed-class computation — weights, kernels, and sampler state as admitted
  content — completing "the graph is the program" down through the model.
---

# Sealed local inference

Remote provider calls cap out at `recorded`: the sample from the
distribution is irreproducible (provider batching, MoE routing, silent
model updates — temp=0 is not bit-stable remotely), so the record is the
only evidence there will ever be. Local inference under full execution
control inverts the class. When the substrate seals every input that
selects the output — weights, kernels, sampler state, device — an LLM call
becomes a pure function of admitted content: **sealed**. Re-derivation must
reproduce it bit-for-bit, and a mismatch is a substrate-integrity finding,
not the world moving. The record store stops being evidence of last resort
and becomes a derivation cache; certification upgrades from replay to proof
by recomputation.

**Substrate decision (Leo, 2026-08-07): the local inference backend is
tinygrad.** Not incidental: tinygrad's static computation graph and
deterministic linearized kernel schedule are what make the sealed claim
implementable — kernels are compiled artifacts that can be content-
addressed, and there is no library-side algorithm roulette to launder
nondeterminism through. The codebase is small enough to realize whole.

## The sealed identity

An inference call's digest decomposes into named tranches, every one
admitted content:

- **weights** — a realization: content-addressed manifest over the
  safetensors, pinned by digest, inherited by descendants, GC'd by
  manifest reachability. A model version is a realization version.
- **kernels** — the compiled kernel set for (graph, device class),
  captured as artifacts; the BEAM search cache is pinned as a realization
  so kernel *selection* is sealed, not just kernel source.
- **runtime** — the tinygrad tree itself under a realization mount
  (precedent: `simulator_runtime`, 1,276 files — realizing a whole runtime
  is proven), plus the inference server as a managed runtime on the
  existing framed-streaming protocol surface.
- **sampler** — RNG seed and sampler config in the action identity, like
  any other param.
- **device** — device class named in the digest. Float non-associativity
  means bit-stability does not transfer across hardware; `sealed` is
  always scoped to an execution identity. A different GPU is a different
  digest is a different run — never a false "sealed everywhere."

What stays `recorded` even locally: effects crossing outside the sealed
set — live retrieval, user interaction — and sampling on any execution
identity not yet sealed. What stays `live`: what is live everywhere.

## Interior structure: the turn stops being atomic

Generation state is tensors — KV cache, RNG state, tokens so far — and
tensors checkpoint. The park/resume machinery the substrate already has
for graphs extends down into the model:

- **Canceled turns are parked computations.** Interrupt → checkpoint
  capsule at a token boundary → resume later, bit-identical to the
  uninterrupted run under the sealed identity. Native-resume inside a
  turn; a crash mid-turn re-pays tokens since the last checkpoint, not
  the turn.
- **Fork at token k.** Branch a generation with different seeds or
  injected continuations — tree search over reasoning with sealed
  provenance per branch, the graph runtime's fanout at token granularity.
  Rejected branches remain re-derivable evidence.
- **KV-prefix reuse as CAS.** The KV cache for a shared prefix is a
  derived artifact keyed by (weights, kernels, prefix tokens): shared
  prefixes share computation across runs and callers, not just storage.

Checkpoint economics are real: KV tensors run to gigabytes at long
context. Checkpoints land at declared segment boundaries (the
`segment_steps` idea, not per-token), with the same retention-lane
honesty as every other cache here.

## What this means for ARC

- **The dev→competition gap collapses to a digest change.** The endgame
  is offline; under this design there is no port — the hosted-model
  solver and the competition solver are the same sealed program with the
  provider route swapped for weights + kernels + runtime realizations.
  The submission artifact is a capsule.
- **Search becomes the solver's native move.** Turn budgets and harvest
  levers are adaptations to hosted economics. Fork + prefix-KV make wide
  deliberate search — best-of-N rule conjectures, MCTS over solver
  strategies — the cheap default, and the search tree itself is
  auditable evidence.
- **The banked corpus becomes a flywheel.** Sealed solve traces are
  training data with exact provenance; tinygrad trains as well as it
  infers. Solve → bank → distill → new weights = new realization = new
  digest → campaign re-run as the eval harness → typed divergence report
  as the model-iteration scorecard. The campaign instrument, unchanged,
  is the controlled-experiment harness for self-improvement.
- **Certification by recomputation.** The blind-run drill plus sealed
  inference is an airtight no-leakage proof: the digest decomposes into
  named realizations, none containing the hidden games, and the solve
  re-derives bit-for-bit on a clean node.

## What this means for ryeos

"The graph is the program" currently carries an asterisk: cognition
bottoms out in an unownable remote effect. This removes the asterisk —
program includes the model — and the substrate's claim becomes
whole-agent reproducibility: re-derive an entire agent run bit-for-bit or
receive a typed proof naming which tranche moved. The three standing
directions turn out to be one architecture: the campaign instrument is
the proving ground, portable execution is "ship the capsule, re-derive
where the execution identity matches, recorded-replay where it doesn't,"
and chat-product reuse is the same record store plus cross-caller
KV-prefix sharing.

## Prerequisites, in dependency order

1. **Provider-call effect records**
   (`knowledge:ryeos/future/provider-call-effect-records`) — the identity
   and store machinery, placement- and class-agnostic by design; local
   arrival changes the class marker on the same keys.
2. **Weights-tier realizations.** Current bounds (32 MiB/file,
   256 MiB/launch) are three orders short of safetensors reality, and
   weights want read-only mmap straight from CAS, not the
   materialize-copy path. This gates everything and deserves design
   before local work begins.
3. **Execution-identity vocabulary** — device class, kernel artifacts,
   tinygrad version as named digest components; the divergence proof
   gains a hardware tranche.
4. **tinygrad as a realized managed runtime** — the inference server on
   the framed-streaming protocol, its tree and its JIT/BEAM caches either
   pinned as realizations or declared live (the ambient-content
   discipline applies to caches exactly as it did to the interpreter).
5. **Generation-state capsules** — checkpoint/fork/resume with segment
   economics.
6. **Sealed training runs** (later) — data manifests as realizations,
   seeded runs, campaign-as-eval; parallelism nondeterminism honestly
   classed (`sealed` single-device or deterministic-reduction, `recorded`
   otherwise).

## Non-goals

- No multi-backend hedging: tinygrad is the decision, not a candidate.
- No cross-device sealed claims, ever — scoped identity or nothing.
- No semantic caching, no cross-digest reuse: unchanged from every other
  record surface.

## Triggers to revisit

- Provider records land and the ARC measurement (turns replayed, spend
  saved) is in hand;
- the weights-tier realization design starts;
- ARC's offline deadline is scheduled — the forcing function for the
  whole ladder.
