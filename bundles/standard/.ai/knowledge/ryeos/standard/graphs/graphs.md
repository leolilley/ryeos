<!-- ryeos:signed:2026-07-13T04:02:46Z:d999e51f149b70ac5945c85051d60cb99da38c4e982ecb6c668783993d20fd2e:jhwsFPcZgA0VJNYQm4cMbqKCZ5xEN6t0stULprMgBaf/BOP1cdr3JKfxjeVU9eXdXCcJtRE+1XRgYNpZ4K7BCQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
      run_id: "${_run.graph_run_id}"
  facets: {cohort: "${_run.graph_run_id}", subject: "${subject}"}
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
facets render per item, including `${_run.graph_run_id}`. The parent's effective
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
      condition: 'status == "completed"'
      action: { item_id: tool:ops/notify, params: { text: "graph ${graph_id} done" } }
```

Fire points are `graph_started`, `graph_step_completed` (after every node,
with typed `ok`, `error`, or `retry` status), and `graph_completed`.
Each event exposes an exact root schema; unknown hook events and references to
roots outside that event fail graph loading.
Hooks are **observers**: a hook action is a real dispatch (its `effective_caps`
are enforced, its cost accrues to the run, it shows in the braid) but it cannot
redirect the walk — routing stays the walker's job. Ordinary condition/action
evaluation or child-dispatch failures are warnings; accounting or integrity
failures invalidate terminal authority and fail closed. Node-level resilience
is the node `retry:` block, not a hook action. See `retry-and-hooks.md` for the
full contract.

## State Persistence

The last successfully written versioned checkpoint is the authoritative resume
cursor. It records graph definition ref/hash, `expression_language:
"rye-expr/1"`, current node, state, retry count, accounting, and suppressed
errors. Resume requires that identity-bearing local checkpoint and the exact
definition; event replay is not a state reconstruction fallback. An older
schema or identity/language mismatch fails with
`restart_required_after_expression_language_cutover` and requires a new run.

Receipts, runtime events, transcripts, and artifacts remain durable
observability, but do not advance resumable state without a later successful
checkpoint write.

## Permissions

Graph permissions are lifted by the `graph-permissions` composer
into `policy_facts.effective_caps`. Each node action is checked
against these capabilities before execution.

## Thread Integration

Graphs run as threads. You can:
- Tail events: `ryeos thread tail <id>`
- Cancel: `ryeos commands submit <id> cancel`
- Inspect state: `ryeos thread get <id>`
- Resume interrupted graphs from their identity-bearing local checkpoint
