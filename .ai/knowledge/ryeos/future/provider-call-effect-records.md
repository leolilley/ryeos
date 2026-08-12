<!-- ryeos:signed:2026-08-12T07:28:15Z:3dbf0f4eb7ddbdbc159997a135c941db8cc368be534d49acc2d94cd96fe90747:YSKkVbZxrRo5Sb6q5a8Kh4Uu93iB57Jvpl7az3pL0TGr9UaVTlKNI19+oLRSbBqcwU3NdDG8wNKgAI6VkgW1Bw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, determinism, replay, provider, directive, evidence, certification]
version: "0.2.1"
status: deferred
description: >
  Deferred measurement, certification, retention, and export work over the
  landed provider-call effect-record boundary.
---

# Provider-call effect-record follow-ons

## Current foundation

Provider-call recording and replay are implemented. This note no longer owns
their base implementation.

For a signed directive that admits a durable provider effect, RyeOS now:

- prepares the exact request and authoritative transport coordinate before
  contact;
- binds remote route/config/accounting authority or the exact admitted local
  worker capsule and execution realization;
- reserves/issues/settles through daemon accounting;
- normalizes the canonical terminal answer and first-observation evidence;
- stores a strict provider-call record in CAS;
- inserts or folds an immutable replay-index answer;
- confirms per-attempt publication proof and repairs both cross-store crash
  edges on restart; and
- emits a runtime-unforgeable chain observation after proof confirmation, then
  serves later hits before reservation/contact with a separate replay
  observation pointing at the same immutable record.

Corrupt or contradictory indexed evidence is an integrity failure, not a live
miss. One identity has one answer; an exact duplicate folds and divergence
fails closed. Remote provider evidence is `recorded`. The standard local worker
is also currently `recorded`, with a daemon-owned local observation and an
admitted execution realization; it is not upgraded to sealed by configuration.

## Standing semantics

### Identity

The provider coordinate commits to every behavior- or authority-selecting
input available at the boundary: effective program, request projection/body
digest, model/route/config, provider authority and accounting contract, and for
local execution the exact worker capsule/realization. Credential values never
enter durable identity.

There is no semantic cache and no cross-coordinate reuse. Equal-looking text
under a moved route, policy, authority, tool schema, realization, or effective
definition is a different effect.

### Observation

Remote observation is runtime-produced and daemon-validated against the issued
attempt. Local observation is daemon-owned and durably retained before a
terminal is exposed. Admitted and observed execution realizations are distinct;
the current local recorded route leaves the observed field absent rather than
fabricating it from the admitted hash.

### First bank versus replay

A successful first execution may publish `inserted` or fold an identical
concurrent answer, but it is still a bank operation. Only a later source of
`effect_record` proves replay. A moved identity starts a new bank generation.
Record existence, report existence, or deterministic source code alone never
proves that a particular run replayed.

The execution field exposes this distinction directly. Each daemon-authored
observation is keyed by chain/thread, logical turn/attempt, effect coordinate,
record, and outcome. An executed `inserted|folded` observation and a later
`replay/not_applicable` observation remain separate selectable facts, including
when crash recovery gives them the same logical turn/attempt. Both relate to
one generic directive-turn entity and one provider-record entity. Runtime code
cannot append this reserved event kind, and malformed durable events degrade
the field rather than failing the whole execution document.

### Streaming

The final canonical semantic answer is evidence; provider chunk cadence is
transport texture. Replay reconstructs the semantic stream/terminal contract
without claiming the original provider emitted those replayed chunks live.

## Deferred work

### 1. End-to-end replay measurement

Run one completed cost-bearing execution twice under an unchanged effective
program and retain:

- provider calls executed, banked, folded, and replayed;
- provider spend/contact avoided;
- first divergent provider coordinate, if any;
- wall-clock change separated from graph-node replay; and
- agreement between graph-owned proof rows, provider turn evidence, and the
  execution field.

This is the next acceptance slice. It must distinguish outer graph-effect
replay from inner provider-turn replay and must not require a project tool to
read receipts or threads sideways.

### 2. Certification retention

Ordinary replay retention is an operational cache policy. Certification needs
an explicit evidence lane that guarantees the capsule, provider record, first
observation, accounting/publication proof, realization closure, and relevant
chain events remain reachable through a named certification root. Eviction from
the ordinary cache stays honest loss; certification is an operator/project
decision with a bounded quota, not an implicit forever-store.

### 3. Verification/export profile

Portable evidence must include the provider record and its transitive closure
inside the execution export format. A verifier recomputes the coordinate and
record hash, checks observation/accounting/publication provenance, validates
the containing capsule/effective program and chain, and reports what is absent.
Verification-only import comes before any cross-node continuation.

### 4. Sealed local qualification

The same record/index machinery can serve as a derivation cache after a local
route earns sealed qualification. Qualification is owned by
`knowledge:ryeos/future/sealed-local-inference`: observed artifact promotion,
closed sampler/numerics policy, and a two-process byte proof. The effect-record
store does not infer or grant that class.

## Non-goals

- semantic or approximate prompt caching;
- cross-authority, cross-route, or cross-realization reuse;
- treating a replayed answer as fresh provider speech;
- a runtime receipt/thread-listing API for campaign reporting; or
- keeping all provider evidence forever without an explicit retained root.

## Triggers to revisit

- a representative acceptance run produces its first measured bank/replay set;
- a solve must be certified or exported beyond the producing node;
- ordinary record eviction prevents a required audit; or
- sealed local qualification begins.
