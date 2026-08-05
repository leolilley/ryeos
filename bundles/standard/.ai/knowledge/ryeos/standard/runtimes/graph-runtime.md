<!-- ryeos:signed:2026-08-05T07:04:40Z:b7654ec6cf80cb7c1ae486fd9f9892b106dd5b5a3b49fd0b148fb444dd8de291:iZIfLJSUjxeF+WxQwjnoNTb1Bk5vL0kO3mcqeHcPXdoWh1TKvvbVnQtwiqiestTzaNQt8Wh+0Y2Wzot9nGFWDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/runtimes
tags: [runtime, graph, dag, callbacks, execution-model, checkpoint, durability, continuation, fence]
version: "1.1.0"
description: >
  Graph runtime execution model and durability contract — the single-interpreter
  walker, the one commit fence, checkpoint-as-commit-record, segment continuation,
  and cooperative control.
---

# Runtime: graph-runtime

Invariant: graph-runtime executes graph DAG/state-machine records and delegates
node actions back through the daemon callback channel. It handles node ordering,
conditional edges, foreach expansion, state persistence, transcript logging, and
callback dispatch. Callback children borrow parent execution provenance and must
not own pushed-head snapshot lifecycle.

The rest of this document is the **execution-model contract**: the guarantees an
author, an operator, and a reviewer can rely on about *how* a graph advances,
what survives a crash, and where the one durable line is. Other graph docs
(`../graphs/graphs.md`, `../graphs/follow.md`, `../graphs/retry-and-hooks.md`)
reference "the node dispatch fence" and "the checkpoint fence"; this doc defines
them.

## The walker: one interpreter, one node at a time

A graph run is advanced by a single sequential **walker**. It is a *frontier
interpreter*, not an orchestrator: on each iteration it looks at the current
node, runs its body, and produces exactly **one** `StepOutcome` (advance to a
next node, take a gate branch, complete a foreach, suspend on a follow, schedule
a retry, or terminate). There is no external scheduler driving the graph and no
node-level parallelism in the walk itself — the graph *is* the program, and the
walker is the only thing stepping it. (Parallelism exists *inside* a node — a
parallel `foreach` or a cohort `follow` — bounded by `max_concurrency`; it never
makes two authored nodes advance at once.)

The process running the walker is disposable. Everything below exists so that
losing it — to a crash, a segment cut, or a follow suspension — costs at most one
redone node and never corrupts resumable state.

## The single mutation point (the commit fence)

Every walker branch funnels its one `StepOutcome` through a single function,
`commit_step`. It is the **only** place allowed to:

- emit transition-commit lifecycle events (`graph_step_started`,
  `graph_branch_taken`, `graph_step_completed`);
- write the node **receipt** (the signed provenance record of what the node
  produced);
- write the **checkpoint** (the durable resume cursor);
- emit `graph_completed` and finalize the thread on a terminal.

The walker's main loop never writes a checkpoint, a receipt, or a
transition-commit event anywhere else. Node execution *may* publish **live**
progress before the node settles — a `graph_foreach_started` marker, a
`graph_node_retry` event — but live progress is observability only; it advances
nothing.

### The fence order

For an advancing action node, `commit_step` emits effects in this fixed order:

```
graph_step_started → tool_call_start → (dispatch) → tool_call_result
  → state mutation → receipt → graph_step_completed → checkpoint
```

The **checkpoint is written last**, and it is the only effect that points at the
*next* node. Read it as a database commit: everything before the checkpoint is
the story of what happened at this node; the checkpoint is the single sentence
"…and therefore the run may move on."

### What a crash means

Because the checkpoint is last, **a crash anywhere before it leaves the previous
checkpoint authoritative** — the one still pointing at the *current* node — so
the whole node re-runs on resume. Two consequences follow, and both are
contract, not accident:

- **Effects before the checkpoint are at-least-once.** Events, the receipt, and
  advancing-step hooks may be re-emitted on replay. They are idempotent
  observability, not authority. Only the checkpoint advances resumable state.
- **There is no separate recovery path.** Recovery *is* resume, and resume is
  just startup with a checkpoint injected. A crash is indistinguishable from a
  clean pause — see segment continuation below.

## Result caching is replay authority

An opt-in `cache_result` entry lets a later visit to the same node reuse a prior
result instead of re-dispatching. That makes the cache **replay authority**, so
it is fenced accordingly:

- A cache entry is published **only after** the advancing checkpoint is durable
  (i.e. the step actually committed to `Advance`).
- **Terminal-node results are never cached.** Terminal settlement reports
  failures through a warning channel, so a success-looking result is not proof of
  durable authority — not a safe thing to turn into replay authority.
- Cache entries are **execution-local**: they never persist across runs or
  resumes.

The hazard this closes: if a cache entry became visible *before* its checkpoint
committed and the process then crashed, resume would replay the older checkpoint,
re-enter the node, hit the freshly written cache, and sail past — laundering
uncommitted work into authoritative state through the cache side door. The fence
makes that unrepresentable.

## Segment continuation: the run drives itself

A run is bounded two ways:

- **`max_steps`** — the hard ceiling on total node steps across the whole run.
  Exceeding it is a terminal failure (`max_steps_exceeded`).
- **`segment_steps`** — the soft per-process-segment budget. `step` is cumulative
  across the continuation chain; the segment counter is per-segment.

When a segment exhausts `segment_steps` without reaching a terminal node, the
walker **cuts a machine continuation**: it settles the thread `continued` and the
daemon launches a successor that resumes from the checkpoint the last
`commit_step` already wrote. **No new checkpoint is written for the cut** — the
existing one is already the resume point. A long run therefore executes as a
chain of `continued` threads that drive themselves forward with no external
orchestration.

This is why a crash and a planned pause share one code path: both simply leave
the run at its last committed checkpoint. A crash is an *unplanned* segment cut.

## Cooperative control: cancel and kill settle at a boundary

Operator control is **cooperative**, never a hard signal landing mid-node.
Between every node the walker drains queued operator commands and checks a
`SIGTERM`-driven cancel flag (set by the daemon's graceful-shutdown coordinator):

- A `cancel` or `kill` is routed through `commit_step` as a **terminal outcome**,
  with full lifecycle, and settles the thread as a distinct `cancelled` / `killed`
  status (an operator can tell these apart from a `failed` run). It is never a
  torn write.
- When both queue in one drain, **`kill` supersedes `cancel`**.
- A cancel that races a segment-continuation cut is re-checked **before** the
  handoff, so it is not lost to a fresh successor that would launch carrying no
  cancel.
- One exception to "at a boundary": a cancel arriving during a `retry` backoff
  wakes the sleeping walker promptly (it does not wait out the delay), then
  settles at the next node boundary. The already-written retry checkpoint stays
  authoritative.

## Follow suspend/resume (durable join)

A `follow:` node delegates a whole sub-execution to a child and suspends the
parent until the child chain is terminal. In fence terms:

- The suspend writes a **pending-follow checkpoint pointing at the follow node
  itself**, carrying no child identity (the daemon owns that mapping), then hands
  off. A re-entry re-drives the handoff idempotently by follow key, so exactly one
  child is spawned even across a crash between checkpoint and handoff.
- A failed handoff settles a **terminal error** — never `continued` with no child
  behind it. There is no "waiting on a child that does not exist" state.
- The suspend deliberately writes **no receipt and no `graph_step_completed`** —
  the child's result does not exist yet. A follow step is one node-step split
  across two process lifetimes: the receipt and step-completion are emitted only
  on resume, once the child's terminal envelope is spliced in.

See `../graphs/follow.md` for authoring, cohort follow, result folding,
capabilities, and lineage facts.

## Resume is fail-closed

The last successfully written checkpoint is the authoritative resume cursor. It
pins `definition_ref`, `effective_definition_digest`, the admitted launch
capsule where required, and `expression_language: "rye-expr/1"`. The effective
digest commits to the ordered source contributors, composed graph, captured
hook plan, policy facts, source-space/trust, and signers; it is not the root
file hash. On resume these coordinates must match the exact sealed program; the
current node must exist in its composed graph; step, retry, accounting, and any
pending-follow snapshot must be internally consistent. Any drift, an older
schema, an unknown or missing field, or an explicit `null` where a value must be omitted fails closed with
`restart_required_after_expression_language_cutover` rather than being migrated
or silently cold-started.

Two things follow directly:

- **Changing effective behavior changes its digest.** A source edit, ancestor
  change, trust/signer change, or configured-hook-policy change creates a new
  effective program for future launches. An existing run recovers from its
  sealed resolution and never re-resolves live source or hook policy. Current
  signer revocation still blocks recovery.
- **Event replay is not a state-reconstruction fallback.** Receipts, runtime
  events, transcripts, and artifacts are durable observability; they do not
  advance resumable state without a later successful checkpoint write. Only the
  checkpoint can both reconstruct state and prove which signed program produced
  it.

See `../graphs/graphs.md` (State Persistence) for the author-facing summary.
