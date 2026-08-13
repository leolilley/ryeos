<!-- ryeos:signed:2026-08-13T03:35:01Z:e49a5508295598745eb438bca961633b1d2c6742b895972781e4fcf27887c3d5:lx7l/dmz2AvWSnJexbfhsqF3VPVrsnuaprZJh2Eh4499TL0ZB6ba8UC41DxNEDuVXEyzQCZJtHiCfMhB6EsWDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, determinism, inference, tinygrad, sealed, replay]
version: "0.2.1"
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
`knowledge:local-inference/activation`.

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
  (whole-runtime tree realization is already proven), plus the inference
  server as a managed runtime on the
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

## What this enables for search and evaluation

- **Hosted-to-local movement becomes an identity change.** A workflow may keep
  the same sealed program while replacing a remote provider route with exact
  weights, kernels, and runtime realizations. The resulting difference is
  explicit in the admitted identity rather than hidden behind an API choice.
- **Search becomes a native execution shape.** Fork + prefix-KV make wide
  deliberate search and speculative evaluation cheaper, while the search tree
  itself remains auditable evidence.
- **The banked corpus becomes a flywheel.** Sealed solve traces are
  training data with exact provenance; tinygrad trains as well as it
  infers. Solve → bank → distill → new weights = new realization = new
  digest → evaluation re-run → typed divergence report as the model-iteration
  scorecard. The same execution instrument remains the controlled-experiment
  harness for self-improvement.
- **Certification by recomputation.** A blind evaluation plus sealed inference
  can prove that the digest decomposes into named realizations that exclude
  withheld inputs and that the result re-derives bit-for-bit on a clean node.

## What this means for ryeos

"The graph is the program" currently carries an asterisk: cognition
bottoms out in an unownable remote effect. This removes the asterisk —
program includes the model — and the substrate's claim becomes
whole-agent reproducibility: re-derive an entire agent run bit-for-bit or
receive a typed proof naming which tranche moved. The execution instrument is
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
   runs, repeated evaluation; parallelism nondeterminism honestly classed
   (`sealed` single-device or deterministic-reduction, `recorded` otherwise).

## Non-goals

- No multi-backend hedging: tinygrad is the decision, not a candidate.
- No cross-device sealed claims, ever — scoped identity or nothing.
- No semantic caching, no cross-digest reuse: unchanged from every other
  record surface.

## Triggers to revisit

- a representative measurement of turns replayed, spend saved, and first
  divergence is in hand;
- an available CPU/device target can retain and close its full compiled
  artifact/numerics set;
- an offline or independently reproducible deployment becomes a scheduled
  requirement.
