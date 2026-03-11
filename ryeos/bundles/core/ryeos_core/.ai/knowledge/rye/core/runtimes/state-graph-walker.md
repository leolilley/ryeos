<!-- rye:signed:2026-03-11T06:53:16Z:76be10a81fcbc4bfb59b33712fd0576efa318d32ae1cc662109dd14bf65d16c6:vl9oDVqgfUnftP6aa5Xh6cLDPIIGCUWPl4hdPkVw1LUg2cRiDrKpJ8xailFGjo3RZzldCGA70VykeZlC4uJkDw==:4b987fd4e40303ac -->
```yaml
name: state-graph-walker
title: "State Graph Walker"
description: Graph traversal engine that walks declarative YAML graph tools, dispatching rye_execute for each node
entry_type: reference
category: rye/core/runtimes
version: "1.0.0"
author: rye-os
created_at: 2026-02-19T00:00:00Z
tags:
  - walker
  - graph
  - state-graph
  - orchestration
  - execution-engine
references:
  - state-graph-runtime
  - executor-chain
  - "docs/orchestration/state-graphs.md"
```

# State Graph Walker

The graph walker (`walker.py`, ~1240 lines) is the execution engine behind `state-graph/runtime`. It loads a graph YAML tool, walks nodes, dispatches actions through the core MCP tools, persists state as signed knowledge items, and registers runs in the thread registry.

## Entry Points

| Entry | Used By | Pattern |
| --- | --- | --- |
| `run_sync(graph_config, params, project_path)` | Runtime YAML inline `-c` script | Handles `async` fork, then calls `asyncio.run(execute(...))` |
| `execute(graph_config, params, project_path)` | `run_sync` or direct async call | Main graph traversal loop |
| `_load_graph_yaml(graph_path)` | Runtime inline script | Strips `# rye:signed:` lines, parses YAML |

## Execution Loop

`execute()` follows this cycle per step:

```
1. Look up node in nodes dict
2. Check node type:
   - "return" → persist state as completed, fire graph_completed hooks, return
   - "foreach" → delegate to _handle_foreach(), persist, continue
   - default  → proceed to action dispatch
3. Interpolate action via interpolation.interpolate_action(node["action"], ctx), then strip None values from params
4. If execute directive / thread_directive call → inject parent context (_inject_parent_context)
5. Check capabilities via _check_permission()
6. Dispatch action via _dispatch_action() → _unwrap_result()
7. Handle continuation chains for LLM nodes (status: "continued")
8. Error handling:
   a. Classify error via error_loader.classify()
   b. Fire "error" hooks — hooks get first chance
   c. If hook returns retry → re-execute node (up to max_retries)
   d. Set state._last_error
   e. Check on_error edge → route to recovery node
   f. Check error_mode: "fail" → terminate, "continue" → track error in suppressed_errors list, skip assign
   9. Apply assign — interpolate each expression, write to state
10. Evaluate edges via _evaluate_edges()
11. Persist state (signed knowledge item, atomic write)
12. Fire "after_step" hooks
13. Check cancellation (cancel file sentinel)
13.5. If single-step mode (target_node set) → return {executed_node, next_node, state}
14. Loop back to step 1
```

Terminates on: `type: return` node, missing `next` (edge dead-end), `max_steps` exceeded, error with `fail` mode, or cancellation. On return, if any errors were suppressed via `on_error: continue`, the final status is `completed_with_errors` (not `completed`), and the result includes `errors_suppressed` count and `errors` list.

## Dispatch Pipeline

`_dispatch_action(action, project_path)` routes through the same core tool handles that `ToolDispatcher` uses:

| `primary` | Tool Class | Method |
| --- | --- | --- |
| `execute` | `ExecuteTool` | `handle(item_type, item_id, project_path, parameters)` |
| `search` | `SearchTool` | `handle(item_type, query, project_path, source, limit)` |
| `load` | `LoadTool` | `handle(item_type, item_id, project_path, source)` |
| `sign` | `SignTool` | `handle(item_type, item_id, project_path, source)` |

Tool instances are lazily initialized via `_tools_instance()` with `get_user_space()` for space resolution.

## Result Unwrapping

`_unwrap_result(raw_result)` lifts `data` from the `ExecuteTool` envelope to the top level:

```
Before: {status, type, item_id, data: {stdout, stderr, exit_code}, chain, metadata}
After:  {stdout, stderr, exit_code}
```

- Drops envelope keys: `chain`, `metadata`, `path`, `source`, `resolved_env_keys`
- Error propagation: if outer `status == "error"` or inner `success == false`, injects `status: "error"` into unwrapped result
- Non-dict results wrapped as `{"result": value}`

This is why `${result.stdout}` works in `assign` expressions.

## Interpolation Context

The walker builds a context dict with these namespaces:

| Namespace | Contents | Available In |
| --- | --- | --- |
| `state` | Accumulated state from prior `assign` mutations | `action.params`, `assign`, `next` conditions |
| `inputs` | Original graph input parameters | `action.params`, `assign`, `next` conditions (including gate `when` paths) |
| `result` | Unwrapped output of current node's action | `assign`, `next` conditions (not `action.params`) |
| `_now` | ISO 8601 UTC timestamp (e.g., `2026-03-02T12:00:00+00:00`) | All interpolation sites |
| `_timestamp` | Unix epoch milliseconds (e.g., `1740912000000`) | All interpolation sites |

Foreach nodes add the iteration variable (e.g., `task`) as an additional top-level namespace.

### Fallback Chains (`||`)

Interpolation expressions support `||` as a fallback operator:

```yaml
params:
  universe_id: "${inputs.universe_id || state.universe_id}"
```

Each path is tried left-to-right; the first non-None value is used. This works in both whole-expression (`${a || b}`) and inline (`prefix-${a || b}-suffix`) contexts.

### Gate Node Execution Order

Gate nodes execute in this order: **assign → next evaluation**. The `assign` block runs first, writing values into state, then `next` edge conditions are evaluated against the updated state. This means you can assign a derived value and then gate on it in the same node.

### Consistent Context Across Gates and Interpolation

The `inputs` namespace is available in both `${...}` interpolation expressions and gate `when` condition paths. You can use `inputs.x` consistently everywhere — there is no need to use `state.inputs.x` in gate conditions.

### None Stripping

After `interpolate_action()`, `None` values are stripped from the params dict. This means missing `${inputs.x}` references no longer pass empty strings or `None` to tools — instead, the key is omitted entirely. As a result, tool `CONFIG_SCHEMA` defaults take effect when graph inputs are omitted, so there is no need to hardcode defaults in graph YAML when the tool already defines them.

## Permission Enforcement

`_check_permission(exec_ctx, primary, item_type, item_id)`:

- **Fail-closed**: empty capabilities = deny all
- Internal thread tools (`rye/agent/threads/internal/*`) always allowed
- Capability format: `rye.<primary>.<item_type>.<dotted.item.id>` with `fnmatch` wildcards
- Same logic as `SafetyHarness.check_permission()` in `runner.py`

Context resolution (`_resolve_execution_context`):
1. Check `RYE_PARENT_THREAD_ID` env var → read + verify signed `thread.json`
2. Fall back to explicit `capabilities` parameter
3. No context → empty capabilities (deny all)

## Hooks System

`_merge_graph_hooks()` combines (note: graph hooks are separate from thread hooks — no user/project hooks):
- Layer 1: graph-defined hooks (from `config.hooks`)
- Layer 2: builtin hooks (from `hook_conditions.yaml`, filtered)
- Layer 3: infra hooks (filtered)

Filtered out events: `context_limit_reached`, `thread_started` (thread-only, not applicable to walker).

`_run_hooks(event, context, hooks, project_path)`:
- Filters by event name
- Evaluates conditions via `condition_evaluator.matches()`
- Interpolates actions via `interpolation.interpolate_action()`
- Dispatches via `_dispatch_action()`
- Layers 1-2: first non-None result wins (control flow)
- Layer 3: always runs (infra telemetry)

### Hook Events

| Event | Context Shape | Fires When |
| --- | --- | --- |
| `graph_started` | `{graph_id, state}` | Before first node (fresh runs only) |
| `error` | `{error, classification, node, state, step_count}` | Node action returns `status: error` |
| `after_step` | `{node, next_node, state, step_count, result}` | After each successful node |
| `limit` | `{limit_code, current_value, current_max, state}` | `max_steps` exceeded |
| `graph_completed` | `{graph_id, state, steps[, error]}` | Terminal node or max_steps |

## Foreach Nodes

`_handle_foreach(node, state, inputs, exec_ctx, project_path)`:

- **`over`**: expression resolving to a list via `interpolation.interpolate()`
- **`as`**: variable name bound to each item (default: `item`)
- **`collect`**: optional state key to store collected results
- **Sequential** (default): each iteration completes before next, full state visible
- **Parallel** (`parallel: true` at node level): dispatched via `asyncio.gather`, isolated per-item state

`parallel: true` is set at the **node level**, not inside `action.params`. The old `action.params.async: true` pattern is no longer supported — the walker validation will error if it encounters it.

```yaml
my_node:
  type: foreach
  over: "${state.items}"
  as: item
  parallel: true  # ← node-level, not in action.params
  action:
    primary: execute
    item_type: tool
    item_id: my/tool
    params:
      data: "${item}"
```

After iteration, the `as` variable is cleaned up from state.

## Node Result Caching (`cache_result`)

Nodes can opt into result caching with `cache_result: true`. This caches the **return value only** — the unwrapped result dict from the action dispatch. It does **not** replay filesystem side effects.

```yaml
my_node:
  cache_result: true  # ← caches return value, NOT side effects
  action:
    primary: execute
    item_type: tool
    item_id: rye/bash
    params:
      command: "echo hello"
```

**When to use:** Pure computation nodes where the action has no side effects — the result is fully captured in the return value (e.g., hash computations, API lookups, read-only file reads).

**When NOT to use:** Nodes that write files, mutate state on disk, or produce outputs consumed by downstream nodes via the filesystem. A cache hit returns the prior result but skips re-execution, so files won't be created/updated.

Cache key is computed from: `graph_hash + node_name + interpolated_action + lockfile_hash + config_snapshot_hash`. Cache entries are stored as CAS objects in `.ai/objects/cache/nodes/`.

## Error Visibility (`completed_with_errors`)

When a graph uses `on_error: continue` and one or more nodes fail, the walker tracks every suppressed error in a runtime `suppressed_errors` list. At graph completion:

- **Status** is set to `completed_with_errors` (not `completed`)
- **Result dict** includes `errors_suppressed` (count) and `errors` (list of `{step, node, error}` dicts)
- **Execution snapshot** (CAS) includes the `errors` list
- **Node receipts** (CAS) include `error` field for failed nodes
- **Thread registry** stores `completed_with_errors` as a terminal status
- **`processes/status`** surfaces `errors_suppressed` count
- **`processes/list`** includes `errors_suppressed` in per-run summaries
- **`processes/steps`** returns the full step-by-step summary with error/cache_hit per node

Use `processes/steps` to inspect a graph run without parsing raw JSONL:

```
rye execute tool rye/core/processes/steps --run_id <graph_run_id>
```

## State Persistence

`_persist_state()` writes state as a signed knowledge item:

- **Path**: `.ai/knowledge/graphs/<graph_id>/<graph_run_id>.md`
- **Format**: YAML frontmatter (id, title, entry_type, graph_id, graph_run_id, parent_thread_id, status, current_node, step_count, updated_at) + JSON body
- **Signing**: `MetadataManager.create_signature(ItemType.KNOWLEDGE, content)` prepended
- **Atomicity**: writes to `.md.tmp` then renames

## Resume

Pass `resume: true` + `graph_run_id` in params:

1. `_load_resume_state()` reads the knowledge item
2. Verifies signature via `MetadataManager.parse_and_verify()`
3. Parses frontmatter for `current_node` and `step_count`
4. Parses body as JSON state
5. Continues execution from `current_node` at `step_count`

## Single-Node Execution

Pass `node` and optionally `inject_state` in params to execute a single node:

1. State is initialized normally (fresh or resume)
2. If `inject_state` provided, it's merged over state via `state.update(inject_state)`
3. `current` is set to `target_node`
4. Run ID gets a `-step` suffix to avoid corrupting real transcripts
5. After executing the one node (action, foreach, or gate), returns immediately with:
   `{success, state, executed_node, next_node, step_count}`

## Continuation Chain Handling

When an LLM node returns `status: "continued"` with `continuation_thread_id`:

1. `_follow_continuation_chain()` calls `orchestrator.resolve_thread_chain()` to find terminal thread
2. Reads terminal thread's persisted result from registry
3. Merges result into the walker's result dict

This handles context-limit handoffs transparently — the walker doesn't implement continuation logic itself.

## Async Execution

`run_sync()` with `async: true`:

1. Pre-generates `graph_run_id` and pre-registers in thread registry
2. Forks via `os.fork()`, child calls `os.setsid()` to daemonize
3. Child redirects stderr → `.ai/agent/threads/<graph_run_id>/async.log`, stdout → `/dev/null`
4. Parent returns immediately: `{success, graph_run_id, graph_id, status: "running", pid}`
5. Child runs `execute()` to completion, updates registry status

## CLI

Graph operations are available from the terminal via `ryeos-cli` (`pip install ryeos-cli`):

- `rye graph run <id>` — full execution
- `rye graph step <id> --node <name>` — single-node execution
- `rye graph validate <id>` — static analysis
- `rye graph run <id> --async` — background execution

The CLI is a thin parameter translator — it constructs the same `walker.run_sync()` call that `rye execute tool` uses.

## Cancellation

The walker checks for a `cancel` sentinel file at `.ai/agent/threads/<graph_run_id>/cancel` after each step. If found, persists state as `cancelled` and returns.

## Streaming Progress

The walker writes one-line progress to stderr at step boundaries:

```
[graph:<id>] step N/M <node> <icon> <elapsed> (<detail>)
```

- Icons: `✓` (ok), `✗` (error), `⏹` (return), `...` (in progress)
- Detail includes state diff (`+key1, key2`) for action nodes, "foreach"/"gate" for typed nodes
- Suppressed by `RYE_GRAPH_QUIET=1` env var
- Never writes to stdout (walker returns JSON on stdout)

## Graph Validation

`_validate_graph(cfg)` checks before execution:
- `start` node exists in `nodes`
- All `next` edge targets reference existing nodes
- All `on_error` targets reference existing nodes
- Warns (non-fatal) if no `return` node exists

## Static Analysis

`_analyze_graph(cfg, graph_config)` extends `_validate_graph` with:

- **Reachability**: BFS from `start` node, reports unreachable nodes as warnings
- **State flow**: regex scans `${state.X}` references across all nodes, compares against `assign` keys and `collect` vars
  - Reports keys referenced but never assigned (warning)
  - Reports keys assigned but never referenced (warning)
- **Foreach checks**: validates `over` expression and `action` field exist

Triggered by `validate: true` in params — returns `{success, errors, warnings, node_count}` without executing.

## Environment Pre-Validation

`_preflight_env_check(cfg, graph_config)` scans for `env_requires` declarations:

- **Graph-level**: `graph_config.env_requires` — list of required env vars
- **Node-level**: `node.env_requires` — per-node required env vars

Checked against `os.environ` before execution starts. Returns list of missing var descriptions. If any are missing, execution fails immediately.

## Implementation Files

| File | Purpose |
| --- | --- |
| `.ai/tools/rye/core/runtimes/walker.py` | Graph traversal engine (~1240 lines) |
| `.ai/tools/rye/core/runtimes/state-graph/runtime.yaml` | Runtime config (anchor, env, inline loader) |
| `.ai/tools/rye/core/runtimes/lib/python/module_loader.py` | Module loading utilities |
| `.ai/tools/rye/agent/threads/loaders/interpolation.py` | Template interpolation (`${...}` syntax) |
| `.ai/tools/rye/agent/threads/loaders/condition_evaluator.py` | Edge condition evaluation + path resolution |
| `.ai/tools/rye/agent/threads/loaders/error_loader.py` | Error classification for hook context |
| `.ai/tools/rye/agent/threads/loaders/hooks_loader.py` | Builtin + infra hook loading |
| `.ai/tools/rye/agent/threads/persistence/thread_registry.py` | Run registration + status tracking |
| `.ai/tools/rye/core/processes/steps.py` | Step-by-step transcript summary |
| `ryeos-cli/rye_cli/verbs/graph.py` | CLI integration |
