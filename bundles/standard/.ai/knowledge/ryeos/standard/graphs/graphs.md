<!-- ryeos:signed:2026-08-10T04:56:59Z:1873ebffd01c72ac8e6f5b685c3a7de7b244e57eb3f7860d63da9657e42dd7c8:DfwdJpAY4QFN9J9JDlzYdavkp8rNKRs9rljh06Jg714yjX67DzMqJSNXNqJ4N6bi5gBS8A8sSYebyaxTUYo6Bg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [reference, graphs, dag, state-machine]
version: "1.0.0"
description: >
  How state graphs work — YAML DAG definitions, node types,
  conditional edges, foreach, hooks, and state persistence.
---

# State Graphs

State graphs are declarative YAML state machines executed by the
state-graph runtime. They define multi-step workflows with
conditional branching, parallel execution, and persistent state.

## Graph Structure

```yaml
version: "1.0.0"
category: my
config:
  start: fetch
  on_error: fail
  state: {attempts: 0}
  nodes:
    fetch:
      action:
        item_id: "tool:ryeos/core/fetch"
        params: {item_ref: "knowledge:project/context"}
      assign:
        context: "${result}"
        attempts: "${state.attempts + 1}"
      next: {type: unconditional, to: process}
    process:
      action:
        item_id: "tool:my/process"
        params: {input: "${state.context}"}
      next:
        type: conditional
        branches:
          - when: 'result.status == "ok"'
            to: done
          - to: handle_error
    handle_error:
      action: {item_id: "tool:my/error-handler"}
      next: {type: unconditional, to: done}
    done:
      node_type: return
      output: "${state.context}"
```

## Inheritance and the executable graph

Graphs may `extend` another graph. Every child still declares its own
`version` and `category`. The generic extends-chain composer processes the
deepest ancestor first and applies these rules:

- omitted `config` keys inherit;
- a declared `config` key replaces that complete inherited value;
- `nodes` and `hooks` are never recursively or ID-wise merged;
- `requires.capabilities` can only narrow at each direct parent/child edge;
- `hooks: []` explicitly clears the inherited authored hook list.

This shallow rule makes graph reuse predictable: a child may inherit a whole
topology and change `max_steps`, but declaring `nodes` supplies the complete
effective node mapping. After composition, the signed graph validator proves
that `start`, every edge and error target, expressions, retry policy, hooks,
and capability facts are coherent.

The result in `LaunchEnvelope.resolution.composed.composed` is the executable
graph. The runtime never reconstructs behavior from the root file or reopens
ancestor paths. Root and ancestor bytes remain visible provenance.

## Nodes

Each node has:
- **`node_type`** — `action` (default), `foreach`, `gate`, or `return`
- **`action`** — callback action to execute on action/foreach nodes
- **`assign`** — mapping evaluated from the pre-assignment state and `result`
- **`next`** — unconditional target or ordered conditional branches
- **`on_error`** — a recovery target, overriding top-level `fail`/`continue`
- **`cache_result`** — opt-in, execution-local result caching for repeated
  ordinary action nodes; entries never persist across runs or resumes

### Actions

```yaml
action:
  item_id: "tool:my/deploy"          # Execute a tool
  params: { target: "staging" }

action:
  item_id: "directive:my/review"     # Execute a directive
  params: { scope: "full" }
```

An action result may propose authoritative, meaning-blind project observations:

```json
{
  "project_observations": [{
    "namespace": "example.classification",
    "stable_id": "classification:subject-1",
    "payload": {"status": "accepted"}
  }]
}
```

The graph runtime normalizes these requests only after assignment and branch
evaluation succeed; one action may propose at most 256. The daemon supplies the chain and admitted graph
definition/effective digest, derives the durable observation identity, and
refuses ordinary runtime append of the reserved event kind. A byte-identical
retry returns the original event; reusing the same source-scoped stable ID with
different payload or occurrence fails the graph commit. The field therefore
projects one selectable entity across a crash/retry. This boundary is for
accepted project claims, not progress telemetry; use milestones for advisory
status.

### Edge Conditions

`next` branches use the same `rye-expr/1` expression language as templates.
Conditions must produce booleans. An entry with no `when` is the single default
branch; explicit `null`, structured path/operator maps, and duplicate defaults
are invalid.

```yaml
next:
  type: conditional
  branches:
    - when: 'state.build_status == "success" && state.tests_passed'
      to: deploy
    - when: 'state.build_status == "failed"'
      to: notify
    - to: default_node
```

Use `==`, `!=`, `<`, `<=`, `>`, `>=`, `&&`, `||`, `!`, `in`, arithmetic,
ternaries, and `??` for missing/null fallback. Pure functions include
`length`, `contains`, `keys`, `upper`, `lower`, `json`, `from_json`, `type`,
`exists`, `matches`, `string`, and `number`. Operators are strictly typed:
boolean operators require booleans, ordering compares two numbers or two
strings, and `+` adds two numbers or concatenates two strings. Missing paths
must be handled explicitly with `??` or `exists(path)`. Do not use pipe filters
or structured `path`/`op`/`value` conditions.

#### Assignment and branch candidate

All values in one `assign` mapping read the same pre-assignment state. RyeOS
then merges the complete delta into a candidate and evaluates `next` against
that candidate plus the action `result`. A same-node condition therefore sees
newly assigned `state.*` values:

```yaml
recall:
  action: { item_id: "tool:recall" }
  assign:
    previous_attempts: "${state.attempts}"
    attempts: "${state.attempts + 1}"
    found: "${result.found}"
  next:
    type: conditional
    branches:
      - when: 'state.found && state.attempts > state.previous_attempts'
        to: warm
      - to: study
```

If assignment or branch evaluation fails, the candidate is discarded. An
explicit node `on_error` target receives the unchanged state; top-level
`on_error: fail` terminates; top-level `continue` terminates this graph path as
`completed_with_errors` without retrying or skipping the failed branch.

## Foreach

Use `node_type: foreach` over an array. Sequential iterations may assign: each
successful iteration sees prior successful deltas, while keys within its own
assignment remain simultaneous. Failed items under `continue` add a `null`
result and no delta. Parallel foreach must not declare `assign`; use ordered
`collect` and derive aggregate state in a later node. Its optional
`max_concurrency` must be between 1 and 256. A foreach node cannot declare
`env_requires`; graph-wide `config.env_requires` is checked before `over`
evaluation, the foreach-start lifecycle event, or any iteration dispatch.

```yaml
deploy_all:
  node_type: foreach
  over: "${inputs.targets}"
  as: target
  parallel: true
  max_concurrency: 5
  action:
    item_id: "tool:my/deploy"
    params: {target: "${target}"}
  collect: deployments
  next: {type: unconditional, to: finish}
```

For managed-runtime children whose whole continuation chains must finish before
the graph proceeds, use an **action node with `follow: true` and `over`**, not a
`foreach` node:

```yaml
review_all:
  node_type: action
  over: "${inputs.subjects}"
  as: subject
  parallel: true
  max_concurrency: 4
  follow: true
  action:
    item_id: "directive:example/review"
    params:
      subject: "${subject}"
      run_id: "${run.graph_run_id}"
  facets: {cohort: "${run.graph_run_id}", subject: "${subject}"}
  collect: reviews
  on_error: handle-failure
  next: {type: unconditional, to: finish}
```

This cohort form requires `as` and `parallel: true`; `collect`, when present,
must differ from `as`, and the node must not declare `assign`, `retry`, caching,
or `detach`. `max_concurrency`, when set, must be between 1 and 256 and bounds
launched-and-live child chains.
Collection is input-ordered and failed slots are `null`. Under `continue`, the
ordered collection commits; an explicit redirect or failure discards the
candidate collection. An empty input succeeds with `[]`. Actions, params, and
facets render per item, including `${run.graph_run_id}`. The parent's effective
capabilities and hard limits bound every child. The complete rendered launch
cohort is also held to one rye-expr/1 JSON result budget; exceeding it fails the
node before suspension or daemon handoff. See
`graphs/follow.md` for capability wildcard examples, cancellation/resume
behavior, and a complete authoring example.

## Hooks

Declare `config.hooks` to observe graph lifecycle events with the same typed
definition the directive runtime uses (`id`, `event`, optional `condition`,
`action`) — one hook grammar across runtimes.

```yaml
config:
  hooks:
    - id: announce_done
      event: graph_completed
      result: discard
      condition: 'status == "completed"'
      action: { item_id: tool:ops/notify, params: { text: "graph ${graph_id} done" } }
```

Fire points are `graph_started`, `graph_step_completed` (after every node,
with typed `ok`, `error`, or `retry` status), and `graph_completed`.
Each event exposes an exact root schema; unknown hook events and references to
roots outside that event fail graph loading.
Every graph hook declares either `discard` for a side-effect observer or
`observation` for a bounded namespaced `{kind, payload}` record. `control` is
rejected for graph hooks at admission; a hook return value never redirects the
graph.
Hooks are **observers**: a hook action is a real dispatch, its cost accrues to
the run, and it shows in the braid, but it cannot
redirect the walk — routing stays the walker's job. Ordinary condition/action
evaluation or child-dispatch failures are warnings; accounting or integrity
failures invalidate terminal authority and fail closed. Node-level resilience
is the node `retry:` block, not a hook action. See `retry-and-hooks.md` for the
full contract.

Before launch, authored hooks and signed builtin, infrastructure, context,
operator, and project policy are normalized into one captured effective hook
plan. Authored hooks use the graph's admitted capabilities. A configured hook
uses only the dispatch grants declared by its own signed source. The runtime
does not read hook configuration from the filesystem.

## Execution and durability

A graph is advanced by a single sequential walker, one node at a time. Each node
produces exactly one outcome, and every outcome is committed through one fence.
The observable guarantees an author can rely on:

- **The checkpoint is written last.** For an advancing node the durable cursor is
  written only after that node's events, authoritative project observations,
  state mutation, and receipt. A crash anywhere before it leaves the previous
  checkpoint authoritative, so **the current node re-runs on resume** — never a
  half-applied node. Events, receipts,
  and advancing-step hooks are therefore at-least-once observability; only the
  checkpoint advances resumable state.
- **`cache_result` is fenced.** An entry becomes visible only after its advancing
  checkpoint is durable, and never on a `return` node. Entries are
  execution-local and never persist across runs or resumes.
- **Long runs drive themselves.** `max_steps` is the hard total-step ceiling;
  `segment_steps` is a soft per-segment budget. When a segment's budget is spent
  without reaching a terminal node, the walker cuts a machine continuation — it
  settles `continued` and a successor resumes from the last checkpoint, with no
  external orchestration. A crash is simply an unplanned segment cut: same resume
  path.

### State persistence

The last successfully written versioned checkpoint is the authoritative resume
cursor. It records graph definition ref, `effective_definition_digest`,
`expression_language:
"rye-expr/1"`, current node, state, retry count, accounting, and suppressed
errors. Resume requires that identity-bearing local checkpoint and the exact
definition; event replay is not a state reconstruction fallback. An older
schema or identity/language mismatch fails with
`restart_required_after_expression_language_cutover` and requires a new run.
Changing any effective contributor, composed behavior, trust/signer evidence,
or captured hook policy changes the digest, so a live run cannot resume into
that different program — start a new run instead.

Receipts, runtime events, transcripts, and artifacts remain durable
observability, but do not advance resumable state without a later successful
checkpoint write.

For the full execution-model contract — the single commit fence, its exact
ordering, the cache fence, segment continuation, and cooperative control — see
`../runtimes/graph-runtime.md`.

## Permissions

The signed generic composition rules project
`requires.capabilities.declared` into `policy_facts.effective_caps` and narrow
it across every inheritance edge. The graph validator proves parity between
the composed declaration and the policy fact. Each node action is checked
against the resulting admitted authority before execution.

## Thread Integration

Graphs run as threads. You can:
- Tail events: `ryeos thread tail <id>`
- Cancel: `ryeos commands submit <id> cancel`
- Inspect state: `ryeos thread get <id>`
- Resume interrupted graphs from their identity-bearing local checkpoint

Cancel and kill are **cooperative**: they are honored at a node boundary and
settle the thread as a distinct `cancelled` / `killed` terminal (not `failed`),
never a hard signal landing mid-node. `kill` supersedes `cancel` if both arrive
together.
