<!-- ryeos:signed:2026-07-13T04:02:46Z:852d7f75a2fdc3003b5f0eb8723088178faf534a1f1833334c2df91a56b7a35f:9tMF/SV8L9YQWxuHB2+mIrOTakQswSf9g8qD4NhP6lQxaNKq0/mqwVLtBg/4Kwt7KbTa/+jMBX5rQbE4FVzhAQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/graphs
tags: [graph, follow, authoring, lineage, budget]
version: "1.0.0"
description: >
  Authoring graph `follow:` nodes — single-child follow and bounded cohort
  follow, suspension and durable resume, result folding, capabilities, and
  follow lineage.
---

# Graph Follow

A graph node with `follow: true` launches a **detached child execution** and
suspends the parent until the child's whole continuation chain reaches terminal.
The parent is then resumed from its checkpoint with the child's result injected.
This is how one graph delegates a whole sub-execution to another and consumes its
outcome, rather than making a single leaf action callback.

## Classic follow versus cohort follow

Classic follow launches one managed-runtime child from one action:

```yaml
review:
  node_type: action
  action:
    item_id: "directive:example/review"
    params: {subject: "${state.subject}"}
  follow: true
  on_error: failed
  next: {type: unconditional, to: done}
```

Adding `over` selects **cohort follow**: the action is interpolated and followed
once per array item, and the parent resumes only after the whole cohort is hard
terminal. Cohort follow is an action node (`node_type: action`), not a foreach
node. It must declare `as` and set `parallel: true`. `collect` is optional; when
present, it must differ from `as`. A cohort follow cannot declare `assign`,
`retry`, `cache_result`, or `detach`. If present, `max_concurrency` must
be between 1 and 256.

`action.item_id`, all of `action.params`, and `facets` are recursively
interpolated per item. The item is available under the declared `as` name;
normal `state`, `inputs`, and execution context remain available. Use
`${_run.graph_run_id}` for the current durable graph-run identity, for example
to stamp every child with a cohort facet.

An empty `over` array launches no children and succeeds immediately. Its result
is an empty array; `collect`, when declared, contributes that empty array to the
candidate state, and normal success routing is evaluated.

## Cohort result and state semantics

- Results are collected in input order, regardless of child completion order.
  `collect: reviews` adds that aligned array to the node-local candidate as
  `state.reviews`. The node's `next` expressions see that candidate and receive
  the same ordered array as `result`.
- A failed child occupies its original slot as `null`; indices never collapse
  or reorder. Cohort follow has no per-item `assign` surface.
- With no failures, the candidate (including `collect`) commits and normal
  `next` routing is used.
- With a failure and `on_error: <node>`, the candidate is discarded before the
  graph routes to that node. With the effective `config.on_error: fail` policy,
  it is discarded and the graph fails. With `config.on_error: continue`, the
  aligned partial results commit, the failure is recorded as suppressed, normal
  `next` routing is used, and the eventual graph result is
  `completed_with_errors`.

## Complete cohort example

The parent must declare authority for every item a template may select. Namespace
wildcards use a literal slash before `*`: `ryeos.execute.tool.example/*` covers
`tool:example/explore` and deeper descendants but not `tool:example` or
`tool:example-other`; the equivalent knowledge grant is
`ryeos.execute.knowledge.example/*`.

```yaml
version: "1.0.0"
category: example
requires:
  capabilities:
    declared:
      - ryeos.execute.directive.example/review
      - ryeos.execute.tool.example/*
      - ryeos.execute.knowledge.example/*
config:
  start: review
  max_steps: 100
  on_error: continue
  config_schema:
    type: object
    required: [jobs]
    properties:
      jobs: {type: array}
  nodes:
    review:
      node_type: action
      over: "${inputs.jobs}"
      as: job
      parallel: true
      max_concurrency: 4
      follow: true
      action:
        item_id: "directive:example/review"
        params:
          subject: "${job.subject}"
          context_ref: "knowledge:example/${job.context}"
          cohort_run: "${_run.graph_run_id}"
      facets:
        cohort: "${_run.graph_run_id}"
        subject: "${job.subject}"
      collect: reviews
      next: {type: unconditional, to: done}

    done:
      node_type: return
      output: "${state.reviews}"
```

This example deliberately uses `config.on_error: continue`: failed slots remain
`null`, `state.reviews` commits, and the run finishes
`completed_with_errors`. Replace it with `fail`, or add a node-level `on_error`
target, when partial results must not enter durable graph state.

`max_concurrency` is a launch window, not a batch partition: at most that many
child chains are launched-and-live, and each hard-terminal child admits the next
queued item. Omit it for the runtime default. Every child launch is also bounded
by the parent's effective capabilities and inherited hard limits; a wildcard in
the graph declaration cannot grant authority the parent itself does not have.
Before suspension, the runtime also accounts the complete rendered child-spec
cohort (`item_ref`, parameters, and facets) under one rye-expr/1 JSON result
budget. An aggregate overflow fails the node before any daemon handoff.

Cancellation is lineage-aware: cancelling or killing the parent cascades to the
launched cohort children. Suspension and cohort progress are durable. After a
daemon restart or interrupted resume, the runtime reuses the checkpointed cohort
and terminal child results rather than launching completed items again, then
continues the parent when all slots are terminal.

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
  parent with a visible in-band failure envelope. Choose the graph's `fail` or
  `continue` policy, or a node-level `on_error` target, deliberately; a redirect
  does not receive the discarded follow candidate or its collected partial
  results.
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
- **The count of follows is graph- and expression-bounded.** A classic follow
  issues one child per node visit. A cohort follow issues one child per item in
  its bounded `over` result. Node visits are bounded by `max_steps`; cohort
  cardinality is bounded by the rye-expr/1 result limits, while
  `max_concurrency` bounds live launches rather than total cohort size.
- **The follow-resume edge is not an autonomous segment-cut.** A parent's
  autonomous machine-continuation depth is separately bounded, and a
  `graph_follow_resume` resume edge RESETS that autonomous count (it is structural
  progress, not a runaway self-continuation), so follow never inflates the
  autonomous-run cap.

**Residual risk (accepted, documented).** The limits bound each child, not the
SUM across a chain or cohort: a graph that follows many expensive children can
aggregate spend above any single child's ceiling. Keep `max_steps`, cohort input
size, and `max_concurrency` no larger than the workflow requires.

**Lever if it ever bites (spec, not built).** Thread a cumulative spend
accumulator through the waiter/resume path and re-clamp each child launch against
the remaining chain budget. This adds durable per-chain state and a new
mid-chain budget-exhaustion failure mode (which would strand a suspended parent),
so it is deliberately NOT implemented speculatively — the per-launch bound plus
graph `max_steps` is the intended safety envelope today.
