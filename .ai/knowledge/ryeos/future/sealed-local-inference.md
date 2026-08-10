<!-- ryeos:signed:2026-08-10T03:16:08Z:39e6620092c9488fcb0ca6caddf8787c21dfab13413a1049b1c990bf2646d90c:0iDwlTSBzfUIhaLGlF9GhOa+xoi26tTSLn8+ArQVko3M14hyPOJSDthClRoWaGywThtWMj5si3G4LfqbTln0Bw==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, inference, tinygrad, sealed, replay, arc]
version: "0.2.0"
status: deferred
description: >
  The destination for LLM execution in ryeos: local inference on tinygrad as
  sealed-class computation — weights, kernels, and sampler state as admitted
  content — completing "the graph is the program" down through the model.
---

# Sealed local inference

## Current boundary

The recorded local foundation has landed. RyeOS can import and bind exact
runtime/model/toolchain content, admit a signed `worker`, run Tinygrad/Qwen
through an isolated persistent session, retain a daemon observation, publish a
provider-call record, repair crash boundaries, and replay after restart without
model contact. Node execution identity, admitted execution realization, large
content, and provider-effect evidence are current contracts; see
`knowledge:ryeos/core/execution/local-model-workers`.

The route is intentionally **recorded**, not sealed. No observed realization or
sealed qualification currently proves that the compiled artifact set,
numerics, sampler, and two clean processes reproduce the same bytes. This note
owns that promotion boundary and nothing in the landed worker kind implicitly
crosses it.

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
- **device** — named in the execution identity, a content-addressed
  coordinate *beside* the program digest
  (`knowledge:ryeos/future/execution-identity`). Float non-associativity
  means bit-stability does not transfer across hardware; `sealed` is
  scoped to (program, execution identity), and on foreign hardware sealed
  evidence degrades to recorded — still replayable, no longer
  re-derivable there — never a false "sealed everywhere."

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
the proving ground, while portable execution eventually becomes "export the
complete authenticated capsule closure, re-derive where the execution identity
matches, recorded-replay where it doesn't." That independently complete export
is deferred in `portable-execution-graph-advanced-path.md`; a retained local
capsule alone is not yet a portable artifact. Chat-product reuse is the same
record store plus cross-caller KV-prefix sharing.

## Remaining implementation, in dependency order

The former prerequisites — provider-call records, semantically blind large
content, execution identity/realizations, and a realized Tinygrad worker — are
landed foundation. The remaining work is narrower and evidence-gated:

1. **Qualification contract.** Add a node-signed sealed qualification linking
   the exact admitted realization, target identity/attestation, deterministic
   request and sampler policy, and retained compiled-artifact/numerics plan.
   Provider admission derives its class ceiling from this object; an authored
   `effects: sealed` declaration cannot upgrade a recorded route.
2. **Observed-artifact promotion.** A discovery run records the exact kernels,
   compiler products, selection cache, and numerics facts it observed. It stays
   recorded. Promotion creates a new closed admitted realization; any new JIT
   artifact during a qualified run is an integrity failure.
3. **Two-process byte proof.** Run a bounded acceptance corpus in two clean
   processes under the same realization and require byte-identical canonical
   terminal answers. Changed target/realization moves identity rather than
   serving old proof.
4. **Divergence projection.** Distinguish different program, different
   realization/target scope, and same-scope byte divergence. Only the last is a
   substrate-integrity finding.
5. **Generation-state capsules.** Only after positive qualification, implement
   `knowledge:ryeos/future/generation-state-capsules`; a recorded terminal
   replay does not prove an in-flight tensor checkpoint resumable.
6. **Sealed training runs** (later) — data manifests as realizations, seeded
   runs, campaign-as-eval; parallelism nondeterminism honestly classed
   (`sealed` single-device or deterministic-reduction, `recorded` otherwise).

## Non-goals

- No multi-backend hedging: tinygrad is the decision, not a candidate.
- No cross-device sealed claims, ever — scoped identity or nothing.
- No semantic caching, no cross-digest reuse: unchanged from every other
  record surface.

## Triggers to revisit

- the ARC measurement (turns replayed, spend saved, and first divergence) is in
  hand;
- an available CPU/device target can retain and close its full compiled
  artifact/numerics set;
- ARC's offline deadline is scheduled — the forcing function for the
  whole ladder.
