<!-- ryeos:signed:2026-08-10T03:16:08Z:47a70985b2296b417525177fe1975aec72363072a57004aa2de833a331be9ae0:14fixDk7eM29o/4NpYct4zA+t/XpdHNgd9sSAnjd38Dq3YsHnJcdXukBR2RLius4pGqoj8yEymEMv/DBKxTODQ==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, identity, hardware, inference, tinygrad, evidence]
version: "0.2.0"
status: deferred
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

The standard local route currently publishes recorded evidence with its
admitted realization and no observed realization. That absence is meaningful:
the node has not yet qualified the observed artifact/numerics set for sealed
re-derivation.

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
  compiler/driver versions that affect codegen, and the compiled kernel
  set for (graph, device) including the pinned BEAM cache, so kernel
  *selection* is identity, not just kernel source.
- **numerics** — the policy flags that change bits: fast-math, TF32-class
  toggles, deterministic-reduction settings.
- **interpreter** — the ambient `python-interpreter` residue, absorbed:
  the one piece of today's execution substrate that every realized tool
  still leans on becomes a named identity tranche instead of a footnote.

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

0. **Observed artifact capture.** Retain a strict observation of the compiled
   artifact/numerics closure actually used by one local generation. Never
   synthesize it from the admitted hash.
1. **Qualification.** Promote a closed observed set into a node-signed sealed
   qualification for subsequent launches; link it to the exact node identity,
   admitted realization, sampler policy, and two-process byte proof.
2. **Divergence reporting.** Surface program movement, admitted/observed scope
   movement, and same-scope byte divergence as distinct typed outcomes.
3. **Portable matching.** Advertise retained node/realization requirements and
   support verification-only matching before any cross-node continuation.
4. **Stronger attestation** (later). Hardware-backed claims require their own
   trust policy; the current node probe remains self-attested and says so.

## Triggers to revisit

- sealed local qualification begins (observed tranche values arrive);
- the first cross-node capsule migration is attempted;
- anyone proposes trusting a device claim further than self-attestation
  reaches — that conversation is TEE territory and deserves its own note.
