<!-- ryeos:signed:2026-08-06T03:37:09Z:9431a5125ac9f8395f1e77186ad6073527549cde23f86fea4ad3d0091960e5f6:UwG5nEsbmtXwRQg0ZV7qOPVjhCqM1YI0dwGQzzUH+k48p6iapjnIKlLgcAZgHm5DadA9I1MngWOUhjy/xmuqAg==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
---
tags: [future, determinism, replay, evidence, effects]
version: "0.1.0"
status: deferred
description: >
  Effect-class contract (sealed/recorded/live) making chain re-execution mean
  "derive the same result or produce a typed divergence proof."
---

# Determinism classes: the effect-class contract

RyeOS already contains two instances of a pattern it has never named: the hook
dispatch ledger replays recorded child responses byte-identically, and ARC's
offline mode replays recorded simulator interactions deterministically. Both
are the same idea — a nondeterministic effect recorded as admitted evidence at
first execution and served from the record thereafter. This note names the
general contract.

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

## Prerequisites (why this is deferred)

1. **Engine/toolchain identity** (`content-addressed-managed-runtime-workers`)
   must land first. A divergence proof against an unpinned engine cannot
   distinguish "engine changed" from "tool misbehaved"; proofs that cannot
   make that distinction lie, and lying proofs are worse than none.
2. The effective-program activation must be complete (it is the identity the
   record binds to).

## What may be pulled forward without the prerequisites

- **Class annotation of existing evidence.** Hook-ledger entries and ARC
  offline simulator responses are already recorded-class; stamping the class
  marker on that evidence now means history is pre-classified when replay
  lands. Cheap, additive, no semantics change.
- The `execution.effects` schema design, folded into the workers package so
  the worker contract is shaped by its consumer.

## ARC payoff

The banked-solution corpus becomes a regression instrument: re-derive banked
solves under a changed solver or engine and receive either identical evidence
spines or divergence proofs naming exactly where behavior moved. Combined with
execution families, that is the difference between "the new solver seems
better" and "the new solver wins game X at the same cost and regresses game Y
at step 41."

## Triggers to revisit

- content-addressed workers land (hard prerequisite satisfied);
- anyone asks "did the engine upgrade change solve outcomes?" and the answer
  requires manual archaeology;
- the second recorded-class mechanism gets built ad hoc (the pattern is
  already at two; a third without the contract is the smell).
