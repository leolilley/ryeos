<!-- ryeos:signed:2026-08-12T07:28:15Z:837402dabe5f07e8c1b86d6ca5cf4b120ea08c25ffac6db5bd1def994198de1d:dxrkDFw5Pa/MxL5wxX/t+7LlrRfYNGOpzWixikJ7/7ev/LA5t5FDSVGzW3SVG7eM69sesH6LGpIw+3vQQl3bBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [future, determinism, replay, evidence, effects]
version: "0.2.1"
status: implementable
description: >
  Effect-class contract (sealed/recorded/live) making chain re-execution mean
  "derive the same result or produce a typed divergence proof."
---

# Determinism classes: the effect-class contract

RyeOS already contains multiple instances of one pattern: the hook dispatch
ledger and the provider and graph-node effect stores replay recorded outcomes
byte-identically. They are the same idea — a nondeterministic effect recorded
as admitted evidence at first execution and served from the record thereafter.
This note names the general contract.

## The three classes

Every effect an execution performs belongs to exactly one declared class:

- **sealed** — a pure function of admitted content: the graph walk, expression
  evaluation, composition, digest computation. Re-derivation requires no
  record; divergence is a substrate bug.
- **recorded** — nondeterministic at first execution, captured as admitted
  evidence at the boundary, replay serves the record: provider calls, hook
  child dispatches, tool subprocess results, simulator interactions.
- **live** — genuinely non-replayable and declared as such: wall clock reads,
  external mutation, interactive input. Permitted only where declared;
  evidence carries the class marker so no consumer mistakes it for
  reproducible fact.

A class describes the *complete* observable behavior of its boundary, not
just the returned value. `recorded` therefore demands **result
completeness**: replay serves the record and executes nothing, so
everything the effect does must live inside the recorded result. A boundary
that also writes — workspace files, knowledge appends, any mutation outside
the result envelope — is `live` no matter how deterministic its return
value looks; recording it would make replay silently skip the writes.

## Declaration

Effect classes are declared where authority already is: kind schemas and
runtime/tool descriptors gain an `execution.effects` declaration in the same
family as `execution.hooks` — per effect boundary, the class and (for
`recorded`) the record identity. Undeclared effects are refused at admission,
the same fail-closed posture as unknown hook events. The finalizer captures
the effect declaration into the sealed program like everything else that
determines behavior.

## Replay semantics

With classes declared, "re-run this chain" acquires exact meaning:

- sealed effects re-derive; a mismatch is a typed substrate-integrity finding;
- recorded effects serve the record; re-execution against a live provider is a
  *different run*, never a replay;
- live effects replay as their recorded observations with the live marker.

A divergence produces a typed proof, not a shrug:
`{effect ref, occurrence identity, recorded digest, derived digest, class}` —
enough to name which boundary the world moved at.

## Prerequisites — SATISFIED 2026-08-07

1. **Engine/toolchain identity.** Originally cited against
   `content-addressed-managed-runtime-workers`; that attribution was
   inverted. The thing that names tool/toolchain bytes is **external content
   realization** — declared trees captured into CAS, sealed into the
   effective digest, executed from read-only realization mounts, inherited
   by every descendant. Landed, activated on the graph kind, and proven
   against a whole realized tool tree. A divergence proof can now distinguish "the
   runtime changed" from "the tool misbehaved" for everything declared;
   what remains ambient is named as ambient. Workers *consume* this
   identity; they were never its source.
2. **Effective-program activation** — complete (seed v2, realization in the
   digest, epoch cut over).

Status accordingly: no longer deferred. The first consumer is designed:
`knowledge:ryeos/future/durable-node-effect-records` — cross-run replay of
graph node results as recorded-class effect records, keyed by the
now-truthful node cache identity.

## What may be pulled forward independently

- **Class annotation of existing evidence.** Hook-ledger entries and recorded
  simulator responses are already recorded-class; stamping the class
  marker on that evidence now means history is pre-classified when replay
  lands. Cheap, additive, no semantics change.
- The `execution.effects` schema design — now shaped by its first real
  consumer (the effect-record store), not by the workers package.

## Evaluation payoff

A banked execution corpus becomes a regression instrument: re-derive retained
runs under a changed program or engine and receive either identical evidence
spines or divergence proofs naming exactly where behavior moved. Combined with
execution families, that is the difference between "the new version seems
better" and an exact statement of which evaluation changed, at what cost, and
at which step.

## Triggers to revisit

- content-addressed workers land (hard prerequisite satisfied);
- anyone asks "did the engine upgrade change solve outcomes?" and the answer
  requires manual archaeology;
- the second recorded-class mechanism gets built ad hoc (the pattern is
  already at two; a third without the contract is the smell).
