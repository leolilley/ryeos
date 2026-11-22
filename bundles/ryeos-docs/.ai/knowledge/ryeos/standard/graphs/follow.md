<!-- ryeos:signed:2026-11-22T00:19:58Z:5f9a24fc7fd09f7b4d281beafba28a6899d7a3f46319855ee2c179e0da24466c:0fe2QwiE/Q87o0zg79Tm+65SU0ks5Q76/d3kYQQoIf8fogAv/hY0HjYuc4JWS9xul78mRLU8+VPgxjHvEmZQCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
node. It must declare `as` and set `parallel: true`. `collect` and
`collect_threads` are optional; when present, they must name distinct state keys
and differ from `as`. A cohort follow cannot declare `assign`, `retry`,
`cache_result`, or `detach`. If present, `max_concurrency` must be between 1 and
256.

`action.item_id`, all of `action.params`, and `facets` are recursively
interpolated per item. The item is available under the declared `as` name;
normal `state`, `inputs`, and execution context remain available. Use
`${run.graph_run_id}` for the current durable graph-run identity, for example
to stamp every child with a cohort facet.

An empty `over` array launches no children and succeeds immediately. Its result
is an empty array; each declared collection contributes an empty array to the
candidate state, and normal success routing is evaluated.

## Cohort result and state semantics

- Results are collected in input order, regardless of child completion order.
  `collect: reviews` adds that aligned array to the node-local candidate as
  `state.reviews`. The node's `next` expressions see that candidate and receive
  the same ordered array as `result`.
- `collect_threads: review_threads` adds a second aligned array containing the
  exact daemon-authored terminal thread id for each followed child chain. That
  id names the precise chain accepted by thread services. A child that reaches a
  terminal failure still contributes its id; failure before child birth fails
  the handoff instead of inventing a nullable identity. The id collection never
  changes the authored child return values.
- A failed child occupies its original slot as `null`; indices never collapse
  or reorder. Cohort follow has no per-item `assign` surface.
- With no failures, the candidate (including both collections) commits and normal
  `next` routing is used.
- With a failure and `on_error: <node>`, the candidate is discarded before the
  graph routes to that node. With the effective `config.on_error: fail` policy,
  it is discarded and the graph fails. With `config.on_error: continue`, the
  aligned partial results and thread ids commit atomically, the failure is
  recorded as suppressed, normal `next` routing is used, and the eventual graph result is
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
          cohort_run: "${run.graph_run_id}"
      facets:
        cohort: "${run.graph_run_id}"
        subject: "${job.subject}"
      collect: reviews
      collect_threads: review_threads
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

## Following a worker execution

When the followed item is a `worker_execution:` item (a bounded turn of an
external worker), the followed value is the child's **outcome record** for
every terminal status, never the raw session projection:

```yaml
child_thread_id: T-...          # the followed child thread
status: completed               # the child's terminal status
outcome_kind: completed         # closed classification, below
result: {...}                   # the worker's own result payload, or null
cost: {input_tokens: .., output_tokens: .., total_usd: ".."}   # or null
```

`outcome_kind` is one of `completed`, `deadline` (the duration budget ran
out), `budget` (another budget dimension ran out), `approval_required`,
`outcome_unknown`, `worker_failure`, `cancelled` and `failed`. `result` is the
durable payload of the worker profile's result notification for the completed
turn (for a profile that reports text and usage, `result.result.text` and
`result.result.usage`); it is `null` whenever no turn completed. `cost` is the
shared runtime cost shape, present when the worker's result reports usage, so a
graph can subtract it from a budget held in state.

A worker turn that retains its private candidate (`required_terminal_publication:
retain_result`) needs a parent launched with retained child results. Ordinary
pinned children inherit a discarding copy-on-write realization, which such a
worker refuses, so the graph is launched with the retained-child-results
execution policy (`--retain-child-results` on the CLI,
`ExecutionPolicy::retain_child_results` in code). Each followed worker child then gets its own private
copy-on-write generation, retained for the owner and never published to the
project head.

On success the whole record is the value `assign` and `next` see:

```yaml
review:
  node_type: action
  action: {item_id: "worker_execution:example/review", params: {}}
  follow: true
  assign:
    summary: "${result.result.result.text}"
  on_error: failed
  next: {type: unconditional, to: done}
```

On failure the record is still produced. Under a node-level `on_error` target,
or under `continue`, it is written to graph state as `state._outcome` before
routing, so the recovery node can branch on `state._outcome.outcome_kind` and
read a partial `state._outcome.result`; the next failed worker node overwrites
it. A followed child that failed with a structured runtime failure and has no
worker record of its own (a directive that ended on a typed failure) gets a
generic record: `outcome_kind`, `failure_code` (the failure's own code, which the
graph runtime does not interpret), `child_thread_id`, `status` and `result`, so
a recovery node can branch on `state._outcome.failure_code`. Under `fail` the
graph terminates as before. In a cohort follow, a failed
worker child's slot holds its record (not `null`), so
`${state.reviews[0].outcome_kind}` is readable after a `continue` policy.

The parent also receives a `child_thread_settled` event on its thread for each
followed child, once, when the child settles: `child_thread_id` (the id the
follow's `child_thread_spawned` named), `follow_key`, `node`, `cohort_index`,
`status`, `outcome_kind` (always present), `result_summary` (at most 2 KiB of
the result text, cut on a UTF-8 boundary), `truncated` and `cost`. It is
observation only and never changes follow semantics.

## Following under a run deadline

A graph launched with `run_timeout` can reserve time for its own fallback by
authoring `limits: {deadline: {reserve_seconds: N}}` at the top of the graph
file. A followed child is stopped when that reserve window opens, so the graph
resumes with at least `N` seconds left. A child stopped by the deadline reports
`outcome_kind: deadline` (the same outcome record a failed worker child
reports), which a node-level `on_error` target reads as
`state._outcome.outcome_kind` before running its fallback inside the reserve.

## What follow does

1. The parent (a checkpoint-resumable / native-resume execution) hits a
   `follow:` node and asks the daemon to admit and spawn the child.
2. The daemon reserves a durable **follow waiter**, mints the child as a FRESH
   ROOT (its own chain, no upstream braid), creates the parent's resume
   successor (settling the parent `continued`), then launches the child detached.
3. When the child chain terminates, the daemon stores the child's terminal
   envelope on the waiter and drives the parent's resume successor, which folds
   the result and continues the parent graph.

A follow node is one node-step split across two process lifetimes: the suspend
writes a pending-follow checkpoint (pointing at the follow node, carrying no
child identity) and emits no receipt or `graph_step_completed`; those are emitted
only on resume once the child result is folded in. A failed handoff settles a
terminal error, never `continued` with no child behind it. See
`../runtimes/graph-runtime.md` for how this fits the commit fence.

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
  results. A failed worker child's outcome record is the one exception: it is
  exposed as `state._outcome` (see above).
- Read a followed worker's cost by the follow value's names
  (`${result.cost.input_tokens}`, `output_tokens`, `total_usd`), not the worker
  profile's result pointers: a profile's `cost_micro_usd` is converted into
  `total_usd` and is not a field of `cost`. A required path the value lacks
  fails the node's `assign`.
- A worker whose reported cost is unknown (the Claude Code adapter reports
  `usage.cost_micro_usd: null` with `usage.cost_status: "unknown"` when its
  stream ended without a `result` event) produces a null `cost`, not a zero
  one: a turn that may have spent is never shown as free. A rollup over several
  followed workers adds only the members that reported a cost, so a total can
  be lower than the true spend when a member's cost is null. Test
  `${result.cost}` for null where that matters.
- `on_error` also catches the node's own expression errors (an `assign` or
  `next` path that does not resolve), not only a failed child. Then no outcome
  record was written and `state._outcome` is absent, so a recovery node that may
  see both reads it as an optional path (`${state._outcome.outcome_kind ??
  "none"}`) or checks `exists(...)` first.
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
