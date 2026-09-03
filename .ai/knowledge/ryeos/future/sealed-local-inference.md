<!-- ryeos:signed:2026-08-27T04:21:32Z:4d9c8fe38c5fdbab424b3fba7f2445ba42f4c1ed76889a597ad452fe58a0a12b:uoY1JnjbslyCPKrGqYvEoRSLn2y6lTGmIAXqzdP1cjM2cOI0cMuXP6Tw13Fs59KLY9yujioIvSXo1QPoP9N5BQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, inference, tinygrad, sealed, replay]
version: "0.3.0"
status: scheduled
description: >
  Qualification of an exact tinygrad local-model profile from recorded
  evidence to sealed re-derivation, scoped honestly to program, artifacts,
  numerics, sampler, and execution identity.
---

# Sealed local inference

## Current boundary

The recorded local foundation is implemented. RyeOS can admit exact worker
source and runtime/model/toolchain realizations, deliver them through a
daemon-owned private workspace under disabled isolation or optional enforced
isolation, run a persistent tinygrad worker, retain a daemon observation,
publish a provider-call record, repair crash boundaries, and replay without
model contact.

The route remains `recorded`. No qualification currently proves that its
compiled artifact set, device/numerics, request rendering, sampler, and two
clean processes reproduce the same canonical bytes. An optional sandbox does
not provide that proof and is not a qualification prerequisite.

Remote provider effects remain recorded because their complete execution
identity and model behavior are not under RyeOS control. Local tinygrad can
earn a stronger class only where every output-selecting input and observed
artifact can be closed and independently tested.

## Why tinygrad

Tinygrad is the chosen inference and training substrate because its runtime and
compiler surface is small enough to capture, inspect, and promote as exact
content. This makes sealed qualification feasible; it does not make every
tinygrad execution deterministic by assertion. The qualification evidence is
the authority.

## Qualified identity

A sealed call is scoped to a portable program plus an exact execution identity.
The program/profile commits to:

- model weights and quantization;
- model config, tokenizer, normalization, special tokens, template, tools, and
  stop behavior;
- tinygrad and worker source;
- hermetic interpreter/runtime/compiler closure;
- context, output, batching, sampling, seed, and trace policy; and
- the closed compiled-artifact/selection plan.

The execution realization additionally commits to:

- device class/topology and codegen-relevant features;
- driver/compiler target facts;
- dtype, accumulation, fast-math, TF32-like, and reduction policy;
- exact admitted isolation provenance and runtime resource contract where they
  can affect behavior; and
- the node attestation that made those facts admission authority.

Host paths, pool slots, process IDs, and import locations remain diagnostic.
Changing the program/profile is a new program. Changing only the qualified
target is a new execution scope. A sealed result remains replayable elsewhere
but can be re-derived only where both scopes match.

## Qualification flow

1. Run a recorded discovery execution through the exact target profile.
2. Retain the observed compiler inputs/outputs, kernels, selection cache,
   numerical facts, sampler policy, and bounded diagnostics.
3. Review and promote that observed set into a closed read-only pre-admission
   realization. The discovery run remains recorded.
4. Start two fresh processes under the same admitted program and execution
   identity.
5. Run a bounded acceptance corpus covering greedy and seeded generation,
   tokenizer/template/tool routing, stops, context edges, and selected numeric
   golden points.
6. Require byte-identical canonical terminal answers and the required
   diagnostic agreement.
7. Publish a node-signed qualification linking the program, execution
   realization, closed artifact set, acceptance evidence, and qualification
   policy.

Provider admission derives its maximum class from the current matching
qualification. An authored `effects: sealed` string cannot upgrade a recorded
route. A new JIT artifact, moved numeric fact, missing evidence, or changed
target refuses sealed execution rather than silently compiling or downgrading
under the old coordinate.

Bubblewrap may contribute stronger containment provenance when installed, but
the qualification asks whether the admitted computation re-derives. Normal
disabled-isolation execution can qualify when its complete behavior-bearing
program and execution scope pass the same proof. It must still report that OS
confinement was not enforced.

## Records, traces, and divergence

Provider-call records remain useful after qualification: they become a
derivation cache and retain first-execution evidence. Replaying a record is not
the same as re-deriving it.

Trace evidence may include canonical request/answer, exposed reasoning,
token IDs, selected logits, kernel/compiler refs, timings, and resource facts.
Hidden frontier chain-of-thought is outside this contract. Large traces remain
content-addressed artifacts referenced by bounded events.

Divergence reports distinguish:

- different program/profile;
- same program but different execution/qualification scope;
- same scope with different canonical bytes;
- missing/corrupt qualification evidence; and
- replay without re-derivation.

Only same-scope byte divergence is a sealed substrate-integrity finding.

## Generation state and search

Recorded prefix, park/resume, and fork capsules may exist before sealed
qualification as integrity-checked continuation state. Their results remain
recorded and make no uninterrupted-equivalence claim.

After qualification, an interrupted-versus-uninterrupted proof may upgrade a
matching capsule contract to exact resume/fork. The generic state substrate
remains opaque; the tinygrad provider owns tokens, KV layout, RNG/sampler state,
compatibility, and semantic validation.

Sealed state enables reproducible prefix reuse, token-boundary search, bounded
forks, and independent recomputation of selected branches. It does not turn a
branch score or model rationale into correctness authority.

## Distillation and training

Observable solve trajectories, exposed reasoning, tool actions, outcomes, and
local token traces may be admitted into provenance-complete corpora. Tinygrad
training consumes exact base-model, dataset, program, recipe, and execution
realizations and publishes new immutable weights for separate evaluation and
explicit promotion.

Recorded training is useful and may precede sealed inference. Sealed training
is a later claim requiring deterministic single-device execution or a proved
reduction policy. Model iteration is:

```text
solve -> retain -> admit corpus -> distill/train -> new weight realization
      -> held-out evaluation -> typed comparison -> explicit promotion
```

## Remaining increments

1. Serious model/profile and target activation under recorded execution.
2. Bounded observed-artifact and diagnostic-trace capture.
3. Closed artifact promotion and qualification object.
4. Two-process byte proof and divergence projection.
5. Portable target matching and verification-only export.
6. Exact generation-state qualification where recorded capsule measurements
   justify it.
7. Sealed training only after recorded training exposes concrete value and a
   defensible deterministic policy.

## Non-goals

- no automatic determinism claim from tinygrad, temperature zero, locality, or
  OS isolation;
- no cross-device sealed claim;
- no semantic or cross-coordinate cache reuse;
- no hidden chain-of-thought capture; and
- no requirement that useful local inference, traces, solve work, or recorded
  training wait for sealed qualification.
