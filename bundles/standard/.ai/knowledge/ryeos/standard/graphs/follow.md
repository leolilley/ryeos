---
category: ryeos/standard/graphs
tags: [graph, follow, authoring, lineage, budget]
version: "1.0.0"
description: >
  Authoring graph `follow:` nodes — how a parent graph launches a detached child
  execution, suspends until the child chain terminates, and resumes with the
  child's result; plus the follow lineage a client can read and the chain-budget
  policy.
---

# Graph Follow

A graph node with `follow: true` launches a **detached child execution** and
suspends the parent until the child's whole continuation chain reaches terminal.
The parent is then resumed from its checkpoint with the child's result injected.
This is how one graph delegates a whole sub-execution to another and consumes its
outcome, rather than making a single leaf action callback.

## What follow does

1. The parent (a checkpoint-resumable / native-resume execution) hits a
   `follow:` node and asks the daemon to admit and spawn the child.
2. The daemon reserves a durable **follow waiter**, mints the child as a FRESH
   ROOT (its own chain, no upstream braid), creates the parent's resume
   successor (settling the parent `continued`), then launches the child detached.
3. When the child chain terminates, the daemon stores the child's terminal
   envelope on the waiter and drives the parent's resume successor, which folds
   the result and continues the parent graph.

## Authoring checklist

- A follow child kind must be a **managed runtime** kind (e.g. another graph or a
  directive) — a leaf tool/service kind cannot be followed.
- The parent must have `execute` authority over the child ref; the child is
  bounded by the PARENT's effective caps and hard limits, launched at parent
  depth + 1.
- Keep the follow node's error routing explicit: a failed child chain resumes the
  parent with a visible in-band failure envelope, so route it with an error edge
  rather than relying on a default.
- `retry` on a `follow: true` node is a validation error — retrying a follow
  needs a fresh follow lifecycle per attempt; route a failed follow with an edge
  to a fresh follow node instead.
- Follow nesting (A follows B follows C…) is bounded server-side at admission
  (max depth 8, walked from the follow-waiter lineage — never a caller-supplied
  depth).

## Follow lineage a client can read

Follow lineage surfaces as a `follow` fact on a thread projection (alongside the
kind-derived `execution` facts), so a client can tell a suspended follow parent
from an ordinary segment-cut `continued` thread, and can name the child chain a
parent is waiting on:

- `role: suspended_parent` — this thread issued the follow and is suspended
  awaiting its child chain (carries the live waiter `phase`, the `follow_node`,
  and the child chain identity).
- `role: resume_successor` — this thread is the parent's resume successor that
  consumes the child result.

The live fact is sourced from the follow waiter; after the waiter is cleared, a
resume successor is still recognized from the projected `graph_follow_resume`
continuation edge (CAS is truth). The cross-chain parent → followed-child link is
NOT recorded in the projection today — it lives only in the operational waiter
while the follow is in flight (see `queries::ContinuationReasonMarker` for the
two edge kinds).

## Chain budget — decision (per-launch inheritance is sufficient)

**Decision: a follow chain relies on per-launch limit inheritance; there is no
separate chain-level cumulative spend budget, and none is planned.** Rationale:

- **Each child is individually clamped.** A follow child launches under the
  parent's hard limits (turns / wall / spend) at parent depth + 1, exactly like a
  normal callback-dispatched child. No single child can exceed the parent's
  ceiling, and children cannot recurse unboundedly (depth + 1 clamp plus the
  max-follow-nesting-depth admission bound).
- **The count of sequential follows is graph-bounded.** A graph has a finite node
  set and a `max_steps` ceiling; each `follow:` node issues at most one child per
  visit, and visits are bounded by `max_steps`. So a parent + N sequential
  children is bounded by `max_steps × per-child ceiling`.
- **The follow-resume edge is not an autonomous segment-cut.** A parent's
  autonomous machine-continuation depth is separately bounded, and a
  `graph_follow_resume` resume edge RESETS that autonomous count (it is structural
  progress, not a runaway self-continuation), so follow never inflates the
  autonomous-run cap.

**Residual risk (accepted, documented).** The limits bound each child, not the
SUM across a chain: a graph with a high `max_steps` that sequentially follows
many expensive children can aggregate spend above any single child's ceiling.
Bound that at the graph level (`max_steps` + per-node cost budgeting) if it bites.

**Lever if it ever bites (spec, not built).** Thread a cumulative spend
accumulator through the waiter/resume path and re-clamp each child launch against
the remaining chain budget. This adds durable per-chain state and a new
mid-chain budget-exhaustion failure mode (which would strand a suspended parent),
so it is deliberately NOT implemented speculatively — the per-launch bound plus
graph `max_steps` is the intended safety envelope today.
