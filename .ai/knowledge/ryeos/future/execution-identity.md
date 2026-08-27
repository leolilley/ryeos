<!-- ryeos:signed:2026-08-27T04:21:33Z:efab33de38ae35f4124527ee7e6d4bf5980e992f567078d406993b0cc014f09b:y4Z+pU1Qq3FNXxigVmNcegosEdph/cTWFL8fwmJ9p4OGDZxl0c9myuP3PPzivwZ+tMQxIbrhi7FKMvB5kerqAQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, identity, hardware, inference, tinygrad, evidence]
version: "0.3.0"
status: scheduled
description: >
  A named, content-addressed identity for the execution substrate — device,
  kernel stack, numerics — as a coordinate beside the program digest, so
  sealed claims are honestly scoped and degrade to recorded across hardware
  instead of becoming invalid.
---

# Execution identity

## Current foundation

The missing coordinate is now split at the correct lifecycle boundaries:

- a stable node execution identity and node attestation name the boot substrate;
- an admitted execution realization names the exact behavior-bearing substrate,
  isolation backend/policy, retained program closure, and external realizations
  selected before one launch;
- an observed execution realization is a distinct optional evidence object and
  is not fabricated by echoing the admitted hash; and
- capsules and local provider coordinates retain the admitted realization.

The local-inference fixture currently publishes recorded evidence with its
admitted realization and no observed realization. That absence is meaningful:
the node has not yet qualified the observed artifact/numerics set for sealed
re-derivation. The next serious remote profile uses the same coordinate family;
it does not add a GPU- or model-specific identity system.

The effective definition digest names the *program*: definition, tools,
realizations, parameters. Nothing names what computes it. For recorded-class
evidence that omission is harmless — the record is the evidence, wherever it
was produced. For sealed-class claims it is the whole question: bit-exact
re-derivation holds on one device class, one kernel stack, one numerics
policy, and pretending otherwise would manufacture false substrate-integrity
findings out of float non-associativity. This note names the missing
coordinate.

## The decision: a coordinate, not a digest tranche

Two placements were candidate. Folding device facts into the effective
definition digest makes a cross-machine run a *different program* — no
false sealed claims, but coarse: the same signed program on two nodes
would share nothing, and portable execution would mean re-signing the
world per machine. Instead, **execution identity is a separate
content-addressed coordinate beside the program digest**:

- the program digest stays portable — same program everywhere;
- `recorded` evidence is keyed by its exact effect coordinate: program and
  action identity plus the admitted invocation/realization scope required by
  that transport. A retained record may be verified elsewhere; it is never
  treated as program-only merely because it does not claim re-derivation;
- `sealed` claims are scoped to (program, execution identity):
  re-derivation is provable exactly where both match;
- on a foreign execution identity, sealed evidence **degrades to
  recorded** — still replayable, no longer re-derivable there. Never
  invalid, just less provable.

This also names why remote providers cap at `recorded`: a provider
exposes no honest execution identity, so the stronger scope cannot be
claimed. The class ceiling was an execution-identity fact all along.

## The retained identity family

The landed content-addressed identity family carries named mechanical tranches:

- **device** — class and architecture (GPU model/arch string, CPU ISA and
  the feature flags that reach codegen). Self-attested by the node's own
  probe; no confidential-computing claims in v1, and evidence says so.
- **kernel stack** — the tinygrad tree (already a realization), the
  compiler/driver versions and target facts that affect codegen, and the
  compiled kernel set for (program, device), so kernel *selection* is identity,
  not just kernel source;
- **numerics** — the policy flags that change bits: fast-math, TF32-class
  toggles, deterministic-reduction settings.
- **runtime** — the exact hermetic interpreter, standard library, native
  libraries, worker source, and compiler/runtime closure consumed by the
  admitted local profile. Ambient interpreters are not a valid local-inference
  identity input.

Canonical digests name each object. Nodes probe and attest their stable
identity at boot; launch admission creates the exact realization; capsules
seal it; effect records carry admitted and observed realization fields only
where their observation boundary can honestly claim them. Remote providers do
not acquire a local execution realization merely because RyeOS called them.

## Divergence gains a hardware tranche

A divergence report can now say which coordinate moved:

- same program, same execution identity, different bits — a
  substrate-integrity finding, the alarm sealed-class exists to raise;
- same program, different execution identity — expected non-transfer,
  reported as scope, never as a finding;
- different program — the existing tranche decomposition, unchanged.

## Why the split matters

Boot identity cannot absorb per-program kernels, model bytes, or mutable policy;
launch realization cannot pretend it observed what actually executed; an
observation cannot rewrite the already admitted launch. The three objects keep
those claims separate while making portable execution a matching problem:
nodes advertise identities, capsules carry requirements, sealed work re-derives
where qualification matches, and recorded evidence replays where it does not.

## Remaining increments

0. **Serious target profile.** Name one explicit remote model/device profile
   and match it against a capable node without introducing an automatic
   scheduler or caller-selected mutable hardware policy.
1. **Observed artifact capture.** Retain a strict observation of the compiled
   artifact/numerics closure actually used by one local generation. Never
   synthesize it from the admitted hash.
2. **Qualification.** Promote a closed observed set into a node-signed sealed
   qualification for subsequent launches; link it to the exact node identity,
   admitted realization, sampler policy, and two-process byte proof.
3. **Divergence reporting.** Surface program movement, admitted/observed scope
   movement, and same-scope byte divergence as distinct typed outcomes.
4. **Portable matching.** Advertise retained node/realization requirements and
   support verification-only matching before any cross-node continuation.
5. **Stronger attestation** (later). Hardware-backed claims require their own
   trust policy; the current node probe remains self-attested and says so.

## Triggers to revisit

- the serious remote model/device profile is admitted;
- sealed local qualification begins (observed tranche values arrive);
- the first cross-node capsule migration is attempted;
- anyone proposes trusting a device claim further than self-attestation
  reaches — that conversation is TEE territory and deserves its own note.
