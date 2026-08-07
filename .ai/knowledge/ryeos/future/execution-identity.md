<!-- ryeos:signed:2026-08-07T09:52:46Z:545fae12a37dcac8029e6e8b5db0549cc4b1bb1623c758cd70813629de64c0e6:c5kbRHO3TAr4zPdCCjNU8+9w0grdQ4/7wFir/nme24GQYjGSZzo93bWmBsh4ahfT1hPwuhas7YezgHlM4dagCA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, identity, hardware, inference, tinygrad, evidence]
version: "0.1.0"
status: draft
description: >
  A named, content-addressed identity for the execution substrate — device,
  kernel stack, numerics — as a coordinate beside the program digest, so
  sealed claims are honestly scoped and degrade to recorded across hardware
  instead of becoming invalid.
---

# Execution identity

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
- `recorded` evidence is keyed by program alone and is portable by
  construction;
- `sealed` claims are scoped to (program, execution identity):
  re-derivation is provable exactly where both match;
- on a foreign execution identity, sealed evidence **degrades to
  recorded** — still replayable, no longer re-derivable there. Never
  invalid, just less provable.

This also names why remote providers cap at `recorded`: a provider
exposes no honest execution identity, so the stronger scope cannot be
claimed. The class ceiling was an execution-identity fact all along.

## The object

A content-addressed `execution_identity` with named tranches:

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

The digest over the canonical object is the identity. Nodes probe and
publish their identities at boot as signed documents; a launch selects
one; the capsule seals the selection; records carry it in an optional
field — absent exactly when the boundary has none to claim (remote
providers), which keeps the field itself evidence-bearing.

## Divergence gains a hardware tranche

A divergence report can now say which coordinate moved:

- same program, same execution identity, different bits — a
  substrate-integrity finding, the alarm sealed-class exists to raise;
- same program, different execution identity — expected non-transfer,
  reported as scope, never as a finding;
- different program — the existing tranche decomposition, unchanged.

## Why now, before local inference

The vocabulary pays immediately: absorbing the interpreter residue closes
the last named ambient in the realization story, and giving records the
optional field early means the store's shape never migrates when local
arrives — tinygrad lands as new tranche *values*, not new machinery.
Portable execution (the standing candidate B) becomes a matching problem:
nodes advertise identities, capsules carry requirements, sealed work
re-derives where identities match and recorded-replays where they do not.

## Increments

0. Wire object + canonical digest in ryeos-state, with the same
   current-schema-only decode discipline as every other evidence object.
1. Node probe at boot: CPU/interpreter tranches first (no GPU required),
   published as a signed node document; capsule field seals the
   selection.
2. Optional `execution_identity` on effect records (node and provider),
   absent-means-unclaimable semantics.
3. Divergence reporting: the scope-versus-finding distinction above,
   surfaced wherever records are compared.
4. tinygrad tranches (device, kernel set, BEAM cache, numerics) with the
   sealed-local runtime — values into an already-landed shape.
5. Portable-execution matching (with candidate B, later): advertise,
   require, schedule.

## Triggers to revisit

- Local inference work begins (tranche values arrive);
- the first cross-node capsule migration is attempted;
- anyone proposes trusting a device claim further than self-attestation
  reaches — that conversation is TEE territory and deserves its own note.
