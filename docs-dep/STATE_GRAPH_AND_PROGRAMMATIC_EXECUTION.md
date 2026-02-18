# State Graph Execution & Programmatic Tool Calling

> **Extends:** [`THREAD_SYSTEM_PART2_ADVANCED_ORCHESTRATION.md`](THREAD_SYSTEM_PART2_ADVANCED_ORCHESTRATION.md)
> **Architecture:** Same data-driven patterns — graph definitions as signed YAML tools, state as signed knowledge items, graph runs registered in the thread registry for status tracking and `wait_threads` support, node actions as standard `{primary, item_type, item_id, params}` dicts dispatched through `rye_execute`, condition_evaluator for edges.

### Thread System Realities Relied Upon by Graphs

The graph walker builds on the following thread system features (all implemented):

- **`thread.json` is Ed25519-signed** — `transcript_signer.sign_json()` signs the metadata dict (capabilities, limits, depth) with a `_signature` field using canonical JSON serialization. The walker should verify this signature before trusting parent context.
- **`transcript.jsonl` is checkpoint-signed** — `TranscriptSigner.checkpoint()` appends signed checkpoint events at turn boundaries, covering all preceding bytes. `handoff_thread` and `resume_thread` verify transcript integrity before reconstructing messages.
- **Streaming** — providers with `supports_streaming` write `token_delta` events to both `transcript.jsonl` (JSONL) and the knowledge markdown file in real-time via `TranscriptSink`. Clean rendering occurs at checkpoints via `transcript.render_knowledge()`.
- **Structured outputs** — directives declaring `<outputs>` instruct the LLM to call `directive_return` with typed fields. Registry persistence stores `{cost, outputs}` (not freeform result text). Graphs should prefer `${result.outputs.<field>}` for machine-shaped consumption.
- **Extended orchestrator operations** — `resume_thread` (continue a completed/errored thread with a new message), `kill_thread` (SIGTERM/SIGKILL via PID), `get_chain` (full continuation chain), `chain_search` (search across chain transcripts), `read_transcript` (read knowledge markdown with optional tail).
- **Result truncation** — `thread_directive.py` truncates returned `result.result` to 4000 chars before returning to callers, protecting parent context windows.

---

## Table of Contents

1. [Core Insight](#1-core-insight)
2. [Design Principles](#2-design-principles)
3. [Item Type Mapping](#3-item-type-mapping)
4. [Graph Definition — YAML Tool](#4-graph-definition--yaml-tool)
5. [Graph State — Knowledge Item](#5-graph-state--knowledge-item)
6. [Graph Walker — Runtime](#6-graph-walker--runtime)
   - [6.1 Graph Hooks — Same Infrastructure, Same Event Names](#61-graph-hooks--same-infrastructure-same-event-names)
   - [6.2 Multi-Thread Chains — Context Limit Handoffs](#62-multi-thread-chains--context-limit-handoffs-in-llm-nodes)
7. [Execution Context and Permissions](#7-execution-context-and-permissions)
8. [Node Execution — Just rye_execute](#8-node-execution--just-rye_execute)
9. [Result Unwrapping](#9-result-unwrapping)
10. [Conditional Edges](#10-conditional-edges)
11. [LLM Nodes and Parent Propagation](#11-llm-nodes-and-parent-propagation)
12. [Foreach and Parallel Fan-Out](#12-foreach-and-parallel-fan-out)
13. [Error Handling](#13-error-handling)
14. [Graph Validation](#14-graph-validation)
15. [The Execution Spectrum](#15-the-execution-spectrum)
16. [Programmatic Tool Calling — An Emergent Property](#16-programmatic-tool-calling--an-emergent-property)
17. [Implementation Plan](#17-implementation-plan)
18. [Examples](#18-examples)
19. [Guardrails and Limits](#19-guardrails-and-limits)
20. [What This Replaces in LangGraph Terms](#20-what-this-replaces-in-langgraph-terms)

---

## 1. Core Insight

No new abstractions. A graph definition is a YAML tool. Graph state is a knowledge item. Graph runs register in the thread registry. The graph walker is a runtime. Node actions are `rye_execute` calls. Edge conditions use `condition_evaluator`. Everything goes through the existing chain.

---

## 2. Design Principles

1. **A graph is a tool** — a YAML tool file with `executor_id` pointing to the graph walker runtime, just like a Python tool points to `python_function_runtime`. You execute it with `rye_execute(item_type="tool", item_id="workflows/my-pipeline")`. It gets signed, integrity-verified, participates in space precedence.

2. **State is knowledge** — state is something you know. You query your knowledge to find it. Graph state is a knowledge item at `.ai/knowledge/graphs/<graph_id>/<graph_run_id>.md` with YAML frontmatter and a JSON body. Any tool or LLM can load it via `rye_load`. It's signed, inspectable, searchable.

3. **Nodes are action dicts** — the same `{primary, item_type, item_id, params}` format used everywhere in rye: hooks, ToolDispatcher, orchestrator. A node doesn't know it's in a graph. It's a `rye_execute` call.

4. **Edges are conditions** — the same `path`/`op`/`value` + `any`/`all`/`not` combinators from `condition_evaluator.py`. No new expression language, no JMESPath dependency.

5. **Registry integration** — graph runs register in the thread registry (SQLite) for status tracking, `wait_threads` support, and `list_children` queries. The registry doesn't care what kind of execution it's tracking — it tracks thread_id, directive, parent_id, status, and operational columns (turns, input_tokens, output_tokens, spend, spawn_count, pid, model, continuation chain metadata). Graph runs use the same columns with `graph_run_id` as `thread_id` and `graph_id` as `directive`. No new schema needed — graphs use the existing columns. State content stays in knowledge (signed, searchable); the registry provides the coordination layer.

---

## 3. Item Type Mapping

| Thing            | Item Type               | Where It Lives                                         | Analogy                                       |
| ---------------- | ----------------------- | ------------------------------------------------------ | --------------------------------------------- |
| Graph definition | Tool (YAML)             | `.ai/tools/workflows/my-pipeline.yaml`                 | Like `anthropic.yaml` — declarative YAML tool |
| Graph walker     | Runtime (YAML + Python) | `.ai/tools/rye/core/runtimes/state_graph_runtime.yaml` | Like `python_function_runtime.yaml`           |
| Graph state      | Knowledge               | `.ai/knowledge/graphs/<graph_id>/<graph_run_id>.md`    | Queryable, signed execution state             |
| Graph run status | Thread registry (SQLite) | `.ai/threads/registry.db`                             | Same registry as LLM threads                  |
| Thread metadata  | Signed JSON              | `.ai/threads/<thread_id>/thread.json`                 | Ed25519-signed capabilities/limits/depth       |
| Transcript       | Checkpoint-signed JSONL  | `.ai/threads/<thread_id>/transcript.jsonl`             | Turn-boundary checksums + Ed25519 signatures   |
| Knowledge entry  | Signed markdown          | `.ai/knowledge/agent/threads/.../<thread>.md`         | Checkpoint-rendered + streaming append          |

### How It Fits the Existing Chain

```
graph tool YAML  →  state_graph_runtime  →  subprocess primitive
(nodes/edges)       (walks graph,              (runs the walker
                     dispatches rye_execute)     Python script)
```

Same chain pattern as:

```
my_tool.py  →  python_function_runtime  →  subprocess primitive
```

---

## 4. Graph Definition — YAML Tool

A graph definition is a signed YAML tool. It declares `executor_id: rye/core/runtimes/state_graph_runtime` and contains nodes, edges, and a state schema in its `config` block.

**Interpolation syntax:** Graph node templates use `${...}` — the same syntax as the existing interpolation engine in `loaders/interpolation.py`. This is distinct from runtime template vars (`{tool_path}`, `{params_json}`) which use bare `{...}` and are resolved by the subprocess primitive at a different stage.

```yaml
# .ai/tools/workflows/code-review/graph.yaml
# rye:signed:2026-02-18T...
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
category: workflows/code-review
description: "Analyze code, detect issues, generate fixes, produce report"

config_schema:
  type: object
  properties:
    files:
      type: array
      description: "Files to review"
    severity_threshold:
      type: string
      default: "warning"
  required: [files]

config:
  start: analyze
  max_steps: 50

  hooks:
    - event: error
      condition:
        path: "classification.category"
        op: in
        value: ["transient", "rate_limited"]
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/internal/control
        params:
          action: retry
          max_retries: 3

  nodes:
    analyze:
      action:
        primary: execute
        item_type: tool
        item_id: utilities/code-analyzer
        params:
          files: "${inputs.files}"
          threshold: "${inputs.severity_threshold}"
      assign:
        issues: "${result.issues}"
        file_count: "${result.file_count}"
      next:
        - to: generate_fixes
          when:
            path: "state.issues"
            op: gt
            value: 0
        - to: approve

    generate_fixes:
      action:
        primary: execute
        item_type: tool
        item_id: utilities/fix-generator
        params:
          issues: "${state.issues}"
      assign:
        fixes: "${result.fixes}"
      next: review

    review:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: workflows/code-review/review-fixes
          inputs:
            fixes: "${state.fixes}"
            issues: "${state.issues}"
          limit_overrides:
            turns: 8
            spend: 0.10
      assign:
        review_verdict: "${result.outputs.verdict}"
      next: report

    approve:
      action:
        primary: execute
        item_type: tool
        item_id: utilities/approval-writer
        params:
          file_count: "${state.file_count}"
          message: "No issues found"
      assign:
        approval: "${result}"
      next: report

    report:
      type: return
```

### Key Points

- **`config_schema`** defines the graph's input parameters. Same schema format as every other tool. The graph walker validates inputs against this before starting.
- **`config.hooks`** declares graph-level hooks — same format as directive hooks (event, condition, action dict). Merged with applicable builtin hooks at execution time (§6.1). Optional.
- **`config.nodes`** contains the graph. Each node has an `action` (action dict), optional `assign` (state mutations from result), and `next` (edges).
- **`next`** can be a string (unconditional edge) or a list of conditional edges. First match wins. Last entry without `when` is the default.
- **`type: return`** terminates the graph and returns `state` to the caller.
- **Interpolation** uses `${...}` — the existing interpolation engine from `loaders/interpolation.py`, which resolves dotted paths via `condition_evaluator.resolve_path()`. The walker builds a context dict with `state`, `inputs`, and `result` namespaces so templates like `${state.issues}`, `${inputs.files}`, and `${result.fixes}` all resolve naturally.
- **Missing paths** resolve to empty string (existing interpolation behavior). The walker logs a warning for missing paths in `assign` expressions to aid debugging.
- **Type preservation (known limitation)** — `interpolation.interpolate()` currently uses `re.sub()` which stringifies all values via `str(value)`. This means `assign: { issues: "${result.issues}" }` where `result.issues` is integer `3` stores the string `"3"` in state, breaking numeric edge conditions like `op: gt, value: 0`. **Recommended fix before numeric graph conditions are used:** when a template is a single whole expression (`"${path}"` with no surrounding text), return the raw resolved value without string conversion. Mixed templates like `"Count: ${x}"` retain string behavior. This is a small change to `interpolation.py`'s `interpolate()` function (~10 lines).

---

## 5. Graph State — Knowledge Item

State is a knowledge item. It's created by the graph walker when execution starts, signed and updated after each node.

### Example State

```yaml
# .ai/knowledge/graphs/code-review-graph/code-review-graph-1739820456.md (frontmatter)
id: graphs/code-review-graph/code-review-graph-1739820456
title: "State: code-review-graph (code-review-graph-1739820456)"
entry_type: graph_state
category: graphs/code-review-graph
version: "1.0.0"
graph_id: code-review-graph
graph_run_id: code-review-graph-1739820456
parent_thread_id: my-directive-1739820400
status: running
current_node: generate_fixes
step_count: 2
started_at: "2026-02-18T10:30:00Z"
updated_at: "2026-02-18T10:30:05Z"
tags: [graph_state, code-review]
```

Body (JSON):

```json
{
  "inputs": {
    "files": ["src/auth.py", "src/api.py"],
    "severity_threshold": "warning"
  },
  "issues": 3,
  "file_count": 2
}
```

### Why Knowledge

- **Queryable** — `rye_search(item_type="knowledge", query="graph_state code-review")` finds all runs of this graph.
- **Loadable** — any tool or LLM can `rye_load` the state to check progress, inspect intermediate values, or resume.
- **Signed** — the graph walker signs state after each node via `rye_sign`. Ed25519 signing is ~50μs — negligible compared to the IO of writing the file. Sign every step, no skipping.
- **Space precedence** — state lives in the project space, not system. Projects own their execution state.
- **Inspectable** — it's a markdown file with YAML frontmatter. A human can read it.

### Registry Integration

The walker also registers graph runs in the thread registry — the same SQLite database used by `thread_directive` and `orchestrator`. This provides status tracking, `wait_threads` support, and parent-child visibility without moving state out of knowledge.

**Registration:** At graph start, the walker calls `registry.register(graph_run_id, graph_id, parent_thread_id)`. The registry columns map naturally:

| Registry Column            | Graph Run Value                                         | Notes                                      |
| -------------------------- | ------------------------------------------------------- | ------------------------------------------ |
| `thread_id`                | `graph_run_id` (e.g., `code-review-graph-1739820456`)   | Primary key                                |
| `directive`                | `graph_id` (the graph tool's item_id)                   |                                            |
| `parent_id`                | `RYE_PARENT_THREAD_ID` (if present)                     |                                            |
| `status`                   | `created` → `running` → `completed` / `error`           |                                            |
| `pid`                      | Walker's process ID (auto-set by `register()`)           |                                            |
| `turns`                    | N/A for pure graph runs (populated by LLM threads)       | Cost snapshot columns                      |
| `input_tokens`             | N/A for pure graph runs                                  |                                            |
| `output_tokens`            | N/A for pure graph runs                                  |                                            |
| `spend`                    | N/A for pure graph runs                                  |                                            |
| `spawn_count`              | Number of child threads spawned                          | Incremented by `increment_spawn_count()`   |
| `model`                    | N/A for graph runs (populated by LLM threads)            |                                            |
| `continuation_of`          | Previous thread in continuation chain                    | Set by `set_chain_info()`                  |
| `continuation_thread_id`   | Next thread in continuation chain                        | Set by `set_continuation()`                |
| `chain_root_id`            | Root of the continuation chain                           | Set by `set_chain_info()`                  |
| `result`                   | JSON: `{cost, outputs}` (structured outputs if present)  | Set by `set_result()`                      |

No new schema needed — these columns already exist in `thread_registry.py` (auto-migrated via `ALTER TABLE` on first access). Graph runs use a subset; cost snapshot and continuation columns are primarily consumed by LLM threads.

**Status updates:** The walker calls `registry.update_status(graph_run_id, status)` at key transitions:

```
register()          →  status: created
graph validated     →  status: running
graph completes     →  status: completed
graph errors        →  status: error
graph cancelled     →  status: cancelled
```

**What this enables:**

- **`wait_threads`** — a parent LLM thread or another graph can call `orchestrator.wait_threads([graph_run_id])`. The orchestrator polls the registry (via `_poll_registry()`) — works cross-process because it's SQLite. No new code needed.
- **`list_children`** — `registry.list_children(parent_thread_id)` returns both LLM threads and graph runs spawned by a parent. The registry doesn't distinguish them — they're all rows with a `parent_id`.
- **`get_status`** — `orchestrator.get_status(graph_run_id)` works out of the box. The orchestrator checks in-process tracking first (won't match for graph runs in a different process), then falls back to registry.
- **`list_active`** — graph runs appear alongside active threads. Useful for dashboards and monitoring.
- **`get_chain`** — `registry.get_chain(thread_id)` walks backward to find the chain root, then forward to build an ordered list of all threads in the continuation chain. Useful for inspecting multi-thread LLM runs spawned by graph nodes.
- **`chain_search`** — search across all transcripts in a continuation chain (regex or text). Returns matching lines with context.
- **`read_transcript`** — read a thread's knowledge markdown entry (the signed, rendered transcript). Supports `tail_lines` for efficient access.
- **`resume_thread`** — continue a completed/errored thread with a new user message. Reconstructs conversation from transcript (with integrity verification), spawns a sibling thread, and links via continuation chain.
- **`kill_thread`** — hard termination using registry PID: SIGTERM with 3s grace period, then SIGKILL.

**What this does NOT replace:**

The registry tracks status, not state content. The actual graph state (accumulated `assign` values, `current_node`, `step_count`) lives in the signed knowledge item. The registry answers "is it done?" — the knowledge item answers "what did it compute?"

**Separation of concerns:**

| Concern            | System           | Why                                                      |
| ------------------ | ---------------- | -------------------------------------------------------- |
| State content      | Knowledge item   | Signed, searchable, loadable, inspectable, resumable     |
| Status tracking    | Thread registry  | SQLite, cross-process, `wait_threads`, `list_children`   |
| Integrity          | Knowledge signing | Ed25519 signature verifies state wasn't tampered (resume) |

### State Lifecycle

```
graph starts  →  register in thread registry (status: created)
              →  create knowledge item (status: running, step_count: 0)
              →  update registry (status: running)
each node     →  update + sign knowledge item (assign values, increment step, advance current_node)
graph ends    →  update + sign knowledge item (status: completed | error)
              →  update registry (status: completed | error)
```

### State Size

Graph state is a knowledge item on disk, not an LLM context window. There is no token pressure. Large node results are stored directly in the state JSON body — no offload mechanism needed. This is unlike `tool_result_guard` in the runner, which bounds results to protect the LLM's context window. The walker has no context window.

If a graph is called from within an LLM thread, the _final graph result_ returned to the caller goes through the normal `tool_result_guard` path in `runner.py` — so large graph outputs are automatically bounded before they enter the LLM's context. The walker doesn't need to duplicate that logic.

### Resumability

If a graph fails mid-execution, the state knowledge item records which node it was on and the full accumulated state. A `resume: true` parameter on the graph tool reloads state and continues from `current_node`. Atomic writes (write temp → rename) ensure partial writes never corrupt state.

### Concurrent Runs

State paths include the `graph_run_id` (always uniquely generated as `{graph_id}-{timestamp}`): `.ai/knowledge/graphs/<graph_id>/<graph_run_id>.md`. Two concurrent runs of the same graph produce two separate state files, even when invoked from the same parent thread. No contention.

---

## 6. Graph Walker — Runtime

The graph walker is a runtime YAML + Python script, same pattern as `python_function_runtime`.

### Runtime YAML

```yaml
# .ai/tools/rye/core/runtimes/state_graph_runtime.yaml
version: "1.0.0"
tool_type: runtime
executor_id: rye/core/primitives/subprocess
category: rye/core/runtimes
description: "State graph runtime — walks graph YAML tools, dispatching rye_execute for each node"

env_config:
  interpreter:
    type: venv_python
    venv_path: .venv
    var: RYE_PYTHON
    fallback: python3
  env:
    PYTHONUNBUFFERED: "1"
    PROJECT_VENV_PYTHON: "${RYE_PYTHON}"

anchor:
  enabled: true
  mode: auto
  markers_any: ["__init__.py", "pyproject.toml"]
  root: tool_dir
  lib: lib/python
  env_paths:
    PYTHONPATH:
      prepend: ["{anchor_path}", "{runtime_lib}"]

verify_deps:
  enabled: true
  scope: anchor
  recursive: true
  extensions: [".py", ".yaml", ".yml", ".json"]
  exclude_dirs: ["__pycache__", ".venv", "node_modules", ".git", "config"]

config:
  command: "${RYE_PYTHON}"
  args:
    - "{runtime_dir}/state_graph_walker.py"
    - "--graph-path"
    - "{tool_path}"
    - "--params"
    - "{params_json}"
    - "--project-path"
    - "{project_path}"
  timeout: 600
```

### Walker Script (Pseudocode)

```python
# state_graph_walker.py
# ~350-400 lines
#
# Async strategy: The walker uses a single asyncio.run() at __main__,
# same pattern as thread_directive.py and the primary tool scripts
# (rye_execute.py, rye_sign.py, rye_load.py). All core tool handles
# (ExecuteTool, SearchTool, LoadTool, SignTool) are async def, so
# _dispatch_action() is async and awaits them directly.

async def execute(graph_config, params, project_path):
    """Walk a state graph, dispatching actions for each node."""

    cfg = graph_config["config"]
    nodes = cfg["nodes"]
    current = cfg["start"]
    max_steps = cfg.get("max_steps", 100)
    error_mode = cfg.get("on_error", "fail")  # "fail" | "continue"
    state = {"inputs": params}
    step_count = 0

    # Derive IDs — graph_run_id is always unique to avoid state file collision
    # when the same graph is invoked multiple times from the same parent thread
    graph_id = graph_config.get("_item_id", "unknown")
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")
    graph_run_id = f"{graph_id}-{int(time.time())}"

    # Register in thread registry for status tracking and wait_threads support (§5)
    registry = thread_registry.get_registry(Path(project_path))
    registry.register(graph_run_id, graph_id, parent_thread_id)

    # Resolve execution context (see §7)
    exec_ctx = _resolve_execution_context(params, project_path)

    # Merge hooks: graph-level (layer 1) + applicable builtins (layer 2) + infra (layer 3)
    # Same merge pattern as thread_directive._merge_hooks() — see §6.1
    hooks = _merge_graph_hooks(cfg.get("hooks", []), project_path)

    # Validate graph before running (see §14)
    validation_errors = _validate_graph(cfg)
    if validation_errors:
        registry.update_status(graph_run_id, "error")
        return {"success": False, "error": f"Graph validation failed: {validation_errors}"}

    # Create initial state knowledge item + update registry to running
    registry.update_status(graph_run_id, "running")
    await _persist_state(project_path, graph_id, graph_run_id, state, current, "running", step_count)

    # Fire graph_started hooks (see §6.1)
    await _run_hooks("graph_started", {"graph_id": graph_id, "state": state}, hooks, project_path)

    while current and step_count < max_steps:
        node = nodes[current]
        step_count += 1
        executed_node = current  # track which node we're executing (before edge eval changes current)

        # Return node — terminate
        if node.get("type") == "return":
            await _persist_state(project_path, graph_id, graph_run_id, state, current, "completed", step_count)
            registry.update_status(graph_run_id, "completed")
            await _run_hooks("graph_completed", {"graph_id": graph_id, "state": state, "steps": step_count}, hooks, project_path)
            return {"success": True, "state": state, "steps": step_count}

        # Foreach node — iterate (see §12)
        if node.get("type") == "foreach":
            current, state = await _handle_foreach(node, state, exec_ctx, project_path)
            await _persist_state(project_path, graph_id, graph_run_id, state, current, "running", step_count)
            continue

        # Build interpolation context: {state, inputs, result (empty before exec)}
        interp_ctx = {"state": state, "inputs": params}

        # Interpolate action params from state
        action = interpolation.interpolate_action(node["action"], interp_ctx)

        # If this is a thread_directive call, inject parent context (see §11)
        if action.get("item_id") == "rye/agent/threads/thread_directive":
            action["params"] = _inject_parent_context(action.get("params", {}), exec_ctx)

        # Check capabilities before dispatch (see §7)
        denied = _check_permission(exec_ctx, action.get("primary", "execute"),
                                   action.get("item_type", "tool"), action.get("item_id", ""))
        if denied:
            result = denied
        else:
            # Dispatch via appropriate primary tool (execute, search, load, sign)
            raw_result = await _dispatch_action(action, project_path)

            # Unwrap result envelope (see §9)
            result = _unwrap_result(raw_result)

        # Handle continuation chains for LLM nodes (see §6.2)
        if (action.get("item_id") == "rye/agent/threads/thread_directive"
            and result.get("status") == "continued"
            and result.get("continuation_thread_id")):
            result = await _follow_continuation_chain(
                result["continuation_thread_id"], exec_ctx, project_path
            )

        # Check for errors — hooks get first chance to handle (see §6.1, §13)
        if result.get("status") == "error":
            # Classify error for hook condition matching (same as runner.py)
            classification = error_loader.classify(project_path, _error_to_context(result))
            error_ctx = {"error": result, "classification": classification,
                         "node": executed_node, "state": state, "step_count": step_count}
            hook_action = await _run_hooks("error", error_ctx, hooks, project_path)
            if hook_action and hook_action.get("action") == "retry":
                max_retries = hook_action.get("max_retries", 3)
                retries = state.get("_retries", {}).get(executed_node, 0)
                if retries < max_retries:
                    state.setdefault("_retries", {})[executed_node] = retries + 1
                    step_count -= 1  # don't count retry against max_steps
                    continue  # re-execute same node

            state["_last_error"] = {"node": executed_node, "error": result.get("error", "unknown")}
            error_edge = _find_error_edge(node)
            if error_edge:
                current = error_edge
                await _persist_state(project_path, graph_id, graph_run_id, state, current, "running", step_count)
                continue
            if error_mode == "fail":
                await _persist_state(project_path, graph_id, graph_run_id, state, current, "error", step_count)
                registry.update_status(graph_run_id, "error")
                return {"success": False, "error": result.get("error"), "node": executed_node, "state": state}
            # error_mode == "continue" — skip assign, proceed to edge evaluation with error in state

        # Assign result values to state (skipped on error in "continue" mode — see §13)
        if result.get("status") != "error":
            interp_ctx["result"] = result
            if "assign" in node:
                for key, expr in node["assign"].items():
                    resolved = interpolation.interpolate(expr, interp_ctx)
                    if resolved == "" and expr != "":
                        logger.warning("assign '%s' resolved to empty for expr '%s'", key, expr)
                    state[key] = resolved

        # Evaluate edges
        next_spec = node.get("next")
        current = _evaluate_edges(next_spec, state, result)

        # Persist + sign state after each step
        await _persist_state(project_path, graph_id, graph_run_id, state, current, "running", step_count)

        # Fire after_step hooks — observability, telemetry (see §6.1)
        # Pass executed_node (the node that just ran), not current (the next node)
        await _run_hooks("after_step", {"node": executed_node, "next_node": current,
                                        "state": state, "step_count": step_count, "result": result}, hooks, project_path)

    # Max steps exceeded — fire limit hooks for escalation (see §6.1)
    limit_ctx = {"limit_code": "max_steps_exceeded", "current_value": step_count, "current_max": max_steps, "state": state}
    hook_action = await _run_hooks("limit", limit_ctx, hooks, project_path)
    if hook_action and hook_action.get("action") == "continue":
        max_steps += hook_action.get("additional_steps", 50)
        # Resume loop (would need restructuring — shown conceptually)

    await _persist_state(project_path, graph_id, graph_run_id, state, current, "error", step_count)
    registry.update_status(graph_run_id, "error")
    await _run_hooks("graph_completed", {"graph_id": graph_id, "state": state, "steps": step_count, "error": "max_steps_exceeded"}, hooks, project_path)
    return {"success": False, "error": f"Max steps exceeded ({max_steps})", "state": state}


# Entry point — same pattern as thread_directive.py (line 564)
if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--graph-path", required=True)
    parser.add_argument("--params", required=True)
    parser.add_argument("--project-path", required=True)
    args = parser.parse_args()

    graph_config = _load_graph_yaml(args.graph_path)
    result = asyncio.run(execute(graph_config, json.loads(args.params), args.project_path))
    print(json.dumps(result))
```

### Hook Evaluation

The walker evaluates hooks using the same components it already imports for edge evaluation and action interpolation:

```python
async def _run_hooks(event, context, hooks, project_path):
    """Evaluate hooks for a graph event.

    Same evaluation logic as SafetyHarness.run_hooks():
    - Filter by event name
    - Evaluate condition via condition_evaluator.matches()
    - Interpolate action via interpolation.interpolate_action()
    - Dispatch via _dispatch_action() (same path as node actions)
    - Layer 1-2: first non-None result wins (control flow)
    - Layer 3: always runs (infra telemetry)
    """
    control_result = None
    for hook in hooks:
        if hook.get("event") != event:
            continue
        if not condition_evaluator.matches(context, hook.get("condition", {})):
            continue

        action = hook.get("action", {})
        interpolated = interpolation.interpolate_action(action, context)
        result = await _dispatch_action(interpolated, project_path)

        if hook.get("layer") == 3:
            continue  # infra hooks don't affect control flow

        if result and control_result is None:
            unwrapped = _unwrap_result(result)
            if unwrapped is not None and unwrapped != {"success": True}:
                control_result = unwrapped

    return control_result


def _merge_graph_hooks(graph_hooks, project_path):
    """Merge graph-level hooks with applicable builtins.

    Same pattern as thread_directive._merge_hooks():
    - Graph hooks → layer 1
    - Applicable builtins from hook_conditions.yaml → layer 2
    - Infra hooks → layer 3
    - Sorted by layer
    - Filters out inapplicable thread-only hooks (context_limit_reached)

    Uses the existing HooksLoader API (get_builtin_hooks, get_infra_hooks)
    — no new config keys needed.
    """
    hooks_loader = load_module("loaders/hooks_loader", anchor=_ANCHOR)
    loader = hooks_loader.get_hooks_loader()
    builtin = loader.get_builtin_hooks(Path(project_path))
    infra = loader.get_infra_hooks(Path(project_path))

    # Filter out hooks for events that don't apply to the walker
    EXCLUDED_EVENTS = {"context_limit_reached", "thread_started"}
    builtin = [h for h in builtin if h.get("event") not in EXCLUDED_EVENTS]
    infra = [h for h in infra if h.get("event") not in EXCLUDED_EVENTS]

    for h in graph_hooks:
        h.setdefault("layer", 1)
    for h in builtin:
        h.setdefault("layer", 2)
    for h in infra:
        h.setdefault("layer", 3)

    return sorted(graph_hooks + builtin + infra, key=lambda h: h.get("layer", 2))
```

**Key design point:** `_run_hooks()` uses the same `condition_evaluator.matches()` → `interpolation.interpolate_action()` → dispatch pipeline as `SafetyHarness.run_hooks()`, through `_dispatch_action()` instead of `ToolDispatcher.dispatch()`. No SafetyHarness wrapper, no thread context required.

**Event name reuse:** The walker uses the **same event names** as the thread system (`error`, `limit`, `after_step`) rather than graph-specific names. This allows existing builtin hooks from `hook_conditions.yaml` to fire without duplication. Graph-only events (`graph_started`, `graph_completed`) are additions with no thread equivalents. The walker also produces the same error context shape (`{error, classification}`) by running `error_loader.classify()` before firing `error` hooks — this ensures `default_retry_transient`'s condition on `classification.category` works identically.

### Action Dispatch

The walker routes each node action through the appropriate primary tool based on `action.primary`:

```python
async def _dispatch_action(action, project_path):
    """Dispatch a node action through the appropriate primary tool.

    Supports all four primaries: execute, search, load, sign.
    Same action dict format used by hooks, ToolDispatcher, and everywhere else.

    All core tool handles are async def — the walker awaits them directly
    within its single event loop (same pattern as ToolDispatcher.dispatch()).
    """
    primary = action.get("primary", "execute")
    item_type = action.get("item_type", "tool")
    item_id = action.get("item_id", "")
    params = action.get("params", {})

    if primary == "execute":
        return await ExecuteTool(user_space).handle(
            item_type=item_type, item_id=item_id,
            project_path=project_path, parameters=params,
        )
    elif primary == "search":
        return await SearchTool(user_space).handle(
            item_type=item_type, query=params.get("query", ""),
            project_path=project_path, source=params.get("source", "project"),
            limit=params.get("limit", 10),
        )
    elif primary == "load":
        return await LoadTool(user_space).handle(
            item_type=item_type, item_id=item_id,
            project_path=project_path, source=params.get("source", "project"),
        )
    elif primary == "sign":
        return await SignTool(user_space).handle(
            item_type=item_type, item_id=item_id,
            project_path=project_path, source=params.get("source", "project"),
        )
    else:
        return {"status": "error", "error": f"Unknown primary: {primary}"}
```

This is the same routing logic as `ToolDispatcher.dispatch()` — same action dict format, same primary tools, same `await` pattern.

### What the Walker Does

1. **Validate graph** — check for missing node references, invalid `start`, dangling edges (see §14)
2. **Register in thread registry** — `registry.register(graph_run_id, graph_id, parent_thread_id)` for status tracking and `wait_threads` support (§5)
3. **Resolve execution context** — determine capabilities, parent thread, limits (see §7)
4. **Merge hooks** — graph-level hooks (layer 1) + applicable builtins (layer 2) + infra (layer 3), same merge pattern as `thread_directive._merge_hooks()` (§6.1)
5. **Load graph config** from the tool YAML (passed by the runtime as `{tool_path}`)
6. **Validate inputs** against `config_schema`
7. **Create state** knowledge item (status: running), update registry to `running`
8. **Fire `graph_started` hooks** — setup, validation, custom initialization
9. **Walk nodes**: interpolate action → inject parent context for thread spawns → `await _dispatch_action()` → unwrap result → follow continuation chains (§6.2) → classify error + evaluate `error` hooks (retry/handle) → graph-level error routing → assign results (skipped on error) → evaluate edges → persist + sign state → fire `after_step` hooks
10. **Handle foreach** — expand iterations, collect results
11. **Terminate** on `type: return` node, edge dead-end, error (in fail mode), or max steps — fire `graph_completed` hooks
12. **Finalize** — update state knowledge item (status: completed | error), update registry status

### What the Walker Does NOT Do

- Does not manage tool signing — each tool called via `rye_execute` is verified by the executor chain (integrity is always enforced)
- Does not manage budgets — child threads inherit budget context via `RYE_PARENT_THREAD_ID`, `ledger.reserve()` / `ledger.cascade_spend()` handle the accounting
- Does not manage LLM context — LLM nodes spawn threads via `thread_directive`, which manages its own context window, `tool_result_guard`, and continuation/handoff. When a child thread triggers a context-limit handoff, the walker follows the continuation chain via `orchestrator.wait_threads()` (§6.2)
- Does not define any new APIs — just calls `rye_execute` and the existing interpolation/condition/hook engines

### What the Walker DOES Enforce

- **Hooks** — `_run_hooks()` at graph lifecycle events (`graph_started`, `error`, `after_step`, `limit`, `graph_completed`). Same condition evaluator + interpolation + dispatch pipeline as `SafetyHarness.run_hooks()`, same event names for shared events (§6.1)
- **Capabilities** — `_check_permission()` before every dispatch, same `fnmatch` logic as `SafetyHarness.check_permission()` (§7)
- **Max steps** — `config.max_steps` guard, with `limit` hook for escalation (§6.1)
- **Cancellation** — checks for cancel signal file at each step (§13)
- **Graph validation** — validates node references, start node, required inputs before execution (§14)

### Relationship to the Thread System (Hooks, Limits, Context)

The graph walker is a **subprocess**, not an LLM thread. It runs outside `runner.py`'s loop. Some thread infrastructure applies directly (hooks, capabilities, integrity), while LLM-specific features (context management, token limits, budget tracking) only apply to child threads spawned by the walker:

| Thread Feature                              | Applies to Walker?                                                                                                                                                                                                               | Applies to LLM Nodes?                                                                                   |
| ------------------------------------------- | -------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------- |
| **Hooks**                                   | Yes — same event names (`error`, `limit`, `after_step`) + graph-only events, via async `_run_hooks()` (§6.1). Reuses existing builtins from `hook_conditions.yaml` with event filtering, dispatched through `_dispatch_action()` | Yes — full hook pipeline via `SafetyHarness.run_hooks()`, async, with `ToolDispatcher`                  |
| **Limits** (turns, tokens, spend, duration) | `max_steps` only — walker doesn't consume LLM tokens                                                                                                                                                                             | Yes — child threads inherit `parent_limits` via §11 injection, capped by `min()` in `_resolve_limits()` |
| **Context window management**               | No — walker has no context window                                                                                                                                                                                                | Yes — child threads have `_check_context_limit()`, `tool_result_guard`, and automatic `handoff_thread()` with transcript integrity verification |
| **Budget tracking**                         | No — walker makes no LLM calls                                                                                                                                                                                                   | Yes — child threads use `ledger.reserve()` / `ledger.cascade_spend()` with parent thread budget         |
| **Streaming**                               | No — walker doesn't produce token streams                                                                                                                                                                                         | Yes — `TranscriptSink` writes `token_delta` events to JSONL + knowledge markdown in real-time           |
| **Transcript signing**                      | No — walker doesn't have a transcript (state is in knowledge items)                                                                                                                                                               | Yes — `TranscriptSigner.checkpoint()` at turn boundaries, verified before handoff/resume                 |
| **Capability enforcement**                  | Yes — walker checks capabilities before every dispatch (§7)                                                                                                                                                                      | Yes — child threads inherit capabilities, `SafetyHarness` enforces per-call                             |
| **Integrity verification**                  | Yes — every `rye_execute` call goes through chain verification                                                                                                                                                                   | Yes — same chain                                                                                        |

The walker is a deterministic orchestrator, but it participates in the hook system for error handling, observability, and step-limit escalation. The hook infrastructure is the same — same format, same condition evaluator, same action dicts, same event names for shared events. When the walker spawns an LLM thread (via `thread_directive`), that thread enters the full thread system with its own `SafetyHarness` and async hook pipeline.

### 6.1 Graph Hooks — Same Infrastructure, Same Event Names

The walker participates in the hook system using the same infrastructure the thread system uses: same hook format, same condition evaluator, same action dicts, same layer ordering, and — critically — the **same event names** for shared events (`error`, `limit`, `after_step`). This allows existing builtin hooks to fire without duplication. The difference is dispatch path — `_dispatch_action()` instead of `ToolDispatcher.dispatch()` — but both `await` the same core tool handles.

**How hooks work in the thread system (for reference):**

1. **Loading**: `thread_directive.py._merge_hooks()` combines three layers — directive hooks (layer 1, from directive XML), builtin hooks (layer 2, from `hook_conditions.yaml`), and infra hooks (layer 3). Sorted by layer.
2. **Injection**: The merged hooks list is passed to `SafetyHarness.__init__()`.
3. **Dispatch**: `SafetyHarness` has two dispatch methods:
   - `run_hooks_context(event="thread_started")` — runs ALL matching hooks, collects context strings (knowledge items loaded for LLM framing). Used once at thread start to build the first message.
   - `run_hooks(event, context, dispatcher)` — for `error`, `limit`, `after_step`. Evaluates `condition_evaluator.matches()` against context, dispatches action via `ToolDispatcher`. Layer 1-2: first non-None result wins (control flow). Layer 3: always runs regardless (infra telemetry).
4. **Condition evaluation**: Same `path`/`op`/`value` + `any`/`all`/`not` combinators as graph edge conditions (§10). Both use `condition_evaluator.matches()`.
5. **Action execution**: Hook actions are action dicts — same `{primary, item_type, item_id, params}` format. Interpolated via `interpolation.interpolate_action()`. Dispatched through `ToolDispatcher.dispatch()`.

**How hooks work in the graph walker:**

Same pipeline, same event names, different dispatch path:

1. **Loading**: `_merge_graph_hooks()` uses the existing `HooksLoader.get_builtin_hooks()` and `get_infra_hooks()` — same API as `thread_directive._merge_hooks()`. Filters out inapplicable thread-only events (`context_limit_reached`, `thread_started`). No new config keys needed.
2. **No SafetyHarness**: Hooks are evaluated directly in the walker loop via `_run_hooks()`.
3. **Dispatch**: `_run_hooks()` uses `condition_evaluator.matches()` for condition evaluation, `interpolation.interpolate_action()` for template resolution, and `await _dispatch_action()` for execution — the same functions the walker already uses for node actions.
4. **Error context**: The walker calls `error_loader.classify()` before firing `error` hooks, producing the same `{error, classification}` context shape as `runner.py`. This ensures builtin hooks like `default_retry_transient` (which check `classification.category`) work identically.
5. **Layer ordering**: Same as threads — layer 1 (graph-defined) evaluated first, then layer 2 (builtins), then layer 3 (infra). First non-None control result from layers 1-2 wins. Layer 3 always runs.

**Graph walker events:**

| Walker Event      | Same Thread Event?                       | Fires When                            | Purpose                                                                                                                                                                                                                                        |
| ----------------- | ---------------------------------------- | ------------------------------------- | ---------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| `graph_started`   | No (graph-only)                          | Before first node                     | Setup, validation, initialization hooks. No context framing (no LLM)                                                                                                                                                                           |
| `error`           | **Yes** — same event, same context shape | Node dispatch returns `status: error` | Retry transient errors, classify errors, custom error handling. Evaluated **before** graph-level `on_error` edges — hooks get first chance. Walker produces `{error, classification}` via `error_loader.classify()` so existing builtins match |
| `after_step`      | **Yes** — same event                     | After each successful node            | Observability, telemetry, rate limiting between steps                                                                                                                                                                                          |
| `limit`           | **Yes** — same event                     | `max_steps` approached/exceeded       | Escalation, approval to continue, extend step budget. Context includes `limit_code: "max_steps_exceeded"`                                                                                                                                      |
| `graph_completed` | No (graph-only)                          | After terminal node or max_steps      | Cleanup, notifications, result post-processing                                                                                                                                                                                                 |

**Events that DON'T apply to the walker:**

| Thread Event                       | Why Not                                                                                                                  |
| ---------------------------------- | ------------------------------------------------------------------------------------------------------------------------ |
| `thread_started` (context framing) | Walker has no LLM context to frame. `graph_started` serves the setup role without context collection                     |
| `context_limit_reached`            | Walker has no context window. LLM child threads handle their own context limits and trigger handoff internally            |
| `limit` (turns/tokens/spend)       | These are LLM resource limits. Walker doesn't consume LLM resources. Child threads enforce their own via `SafetyHarness` |

**The critical win — transparent retry:**

Without hooks, a tool that fails due to a network timeout immediately triggers graph-level error routing (`on_error` edge or fail mode). With hooks, the existing `default_retry_transient` builtin retries transparently — the graph author doesn't need to add retry logic to every node. Because the walker uses the same `error` event name and produces the same `{error, classification}` context shape, this builtin fires identically in both contexts:

```yaml
# Existing builtin hook (layer 2) — fires for both thread and graph errors
# No duplication needed — same event name, same context shape
- id: default_retry_transient
  event: error # same event as thread system
  layer: 2
  condition:
    path: "classification.category" # walker produces this via error_loader.classify()
    op: in
    value: ["transient", "rate_limited"]
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/internal/control
    params:
      action: retry
```

**Hook priority vs error edges:**

When a node fails, the evaluation order is:

1. **`error` hooks** — evaluated first. The walker classifies the error via `error_loader.classify()` (same as `runner.py`) and fires hooks with `event: "error"`. If a hook returns `{action: "retry"}`, the node re-executes (up to `max_retries`). If a hook returns `{action: "fail"}` or `{action: "abort"}`, the graph terminates immediately.
2. **`on_error` edge** — evaluated next, only if no hook handled the error. Routes to an error-handling node.
3. **Graph-level `on_error` mode** — evaluated last. `"fail"` terminates, `"continue"` records error in `state._last_error` and proceeds to edge evaluation (assign is skipped — see §13).

This ordering means hooks handle infrastructure-level errors (transient failures, rate limits) transparently, while `on_error` edges handle application-level error routing (fallback paths, degraded workflows).

**Builtin hooks from `hook_conditions.yaml` — applicability:**

Because the walker uses the same event names, existing builtins work without duplication. `_merge_graph_hooks()` loads them via the existing `HooksLoader` API and filters out inapplicable thread-only events:

```
default_retry_transient   (event: error)                    — APPLICABLE (same event + context shape)
default_fail_permanent    (event: error)                    — APPLICABLE
default_abort_cancelled   (event: error)                    — APPLICABLE
default_escalate_limit    (event: limit)                    — APPLICABLE (walker fires limit with limit_code)
default_context_compaction (event: context_limit_reached)    — FILTERED OUT (no context window)

infra_save_state          (event: after_step)               — APPLICABLE (checkpoint telemetry)
infra_completion_signal   (event: after_complete)           — FILTERED OUT (graph uses graph_completed instead)
```

No `graph_builtin_hooks` config key needed — the walker reuses `builtin_hooks` and `infra_hooks` with event filtering.

**What's shared vs. what's different:**

| Aspect              | Thread Hooks                                            | Graph Hooks                                               |
| ------------------- | ------------------------------------------------------- | --------------------------------------------------------- |
| Hook format         | `{event, condition, action, layer}`                     | Same                                                      |
| Condition evaluator | `condition_evaluator.matches()`                         | Same                                                      |
| Action format       | `{primary, item_type, item_id, params}`                 | Same                                                      |
| Interpolation       | `interpolation.interpolate_action()`                    | Same                                                      |
| Layer ordering      | 1 (directive) → 2 (builtin) → 3 (infra)                 | 1 (graph) → 2 (builtin) → 3 (infra)                       |
| Dispatch            | Async, via `ToolDispatcher.dispatch()`                  | Async, via `_dispatch_action()` (same `await` pattern)    |
| Wrapper             | `SafetyHarness` instance                                | None — direct evaluation in walker loop                   |
| Error context       | `{error, classification}` via `error_loader.classify()` | Same — walker calls `error_loader.classify()` identically |
| Config source       | `HooksLoader.get_builtin_hooks()` / `get_infra_hooks()` | Same — filtered by event applicability                    |
| Shared events       | `error`, `limit`, `after_step`                          | Same — reused, not duplicated                             |
| Unique events       | `thread_started`, `context_limit_reached`               | `graph_started`, `graph_completed`                        |

### 6.2 Multi-Thread Chains — Context Limit Handoffs in LLM Nodes

When an LLM node spawned by the graph walker runs long enough to fill its context window, `runner.py` triggers a continuation handoff. The walker needs to understand this mechanism because the LLM node's result changes shape.

**How context-limit handoff works in the thread system (implemented):**

1. **Detection**: After each tool dispatch in `runner.py`, `_check_context_limit()` estimates token usage (~4 chars/token). If `usage_ratio >= threshold` (default 0.9, configurable in `coordination.yaml`), it returns a limit info dict. Runner emits a `context_limit_reached` event.
2. **Integrity verification**: `handoff_thread()` verifies transcript integrity via `TranscriptSigner.verify()` before trusting transcript content. Supports `strict` (default) and `lenient` (allows unsigned trailing content) policies from `coordination.yaml`.
3. **Handoff**: `runner.py` calls `orchestrator.handoff_thread(thread_id, project_path, messages)`. If handoff fails, falls back to hooks on `context_limit_reached`.
4. **Summary phase**: `handoff_thread()` reads the signed knowledge entry (not raw transcript) and spawns a summary thread via `summary_directive` from `coordination.yaml`. Configurable `summary_model` (default "fast"), `summary_limit_overrides` (default `{turns: 3, spend: 0.02}`), `summary_max_tokens` (default 4000).
5. **Resume construction**: Builds `resume_messages` = summary context + trailing messages (within `resume_ceiling_tokens`, default 16000) + continuation prompt. Ensures trailing slice starts with a user message (provider requirement).
6. **New thread spawn**: Calls `thread_directive.execute()` with `resume_messages` and the same `directive_name`. The new thread starts from the resume messages, not from scratch.
7. **Chain linking**: `registry.set_continuation(old_id, new_id)` links old→new. `registry.set_chain_info(new_id, chain_root_id, old_id)` records chain metadata. `resolve_thread_chain()` follows these links to find the terminal thread.
8. **Old thread finishes**: `runner.py._finalize()` returns `{status: "continued", continuation_thread_id: "..."}` for the original thread. A `thread_handoff` event is logged in the old transcript.

**What the graph walker sees:**

When the walker calls `thread_directive` synchronously (the default), and the child thread hits a context limit:

```
walker calls thread_directive.execute(params) [awaited]
  → runner.run() starts
  → ... many turns ...
  → _check_context_limit() triggers → emits context_limit_reached
  → handoff_thread() runs:
      1. Verifies transcript integrity (TranscriptSigner.verify())
      2. Summarizes transcript via summary_directive (reads signed knowledge entry)
      3. Builds resume_messages (summary + trailing turns within ceiling)
      4. Calls thread_directive.execute() with resume_messages [awaited — runs synchronously]
      5. Links old → new via registry.set_continuation() + set_chain_info()
      6. Logs thread_handoff event in old transcript
  → The continuation thread completes (or itself continues further)
  → handoff_thread() returns {success: True, new_thread_id: "..."}
  → runner._finalize() returns {status: "continued", continuation_thread_id: "..."}
  → thread_directive.execute() returns the full result chain
walker receives result with status: "continued"
```

**Key insight:** `handoff_thread()` calls `thread_directive.execute()` **without** `async_exec: true`, meaning the continuation thread runs synchronously (awaited in-process). The continuation may itself trigger another handoff, forming a chain — but each link runs to completion before returning. By the time the original `thread_directive.execute()` returns to the walker, **the entire continuation chain has completed**.

**Result persistence (implemented):**

`thread_directive.py` already persists a result payload to the registry alongside cost. The implementation stores structured outputs (from `directive_return`) plus cost:

```python
# thread_directive.py (both sync and async paths):
result_data = {"cost": result.get("cost")}
if result.get("outputs"):
    result_data["outputs"] = result["outputs"]
registry.set_result(thread_id, result_data)
```

Additionally, the returned `result.result` text is truncated to 4000 chars before returning to callers:

```python
MAX_RESULT_CHARS = 4000
if isinstance(result.get("result"), str) and len(result["result"]) > MAX_RESULT_CHARS:
    result["result"] = result["result"][:MAX_RESULT_CHARS] + "\n\n[... truncated]"
```

**Important:** Registry persistence stores **structured outputs** (`outputs`), not freeform result text. For reliable cross-process retrieval, directives called from graph nodes should declare `<outputs>` fields and the graph should use `${result.outputs.<field>}` in assign expressions.

The walker's continuation handling:

```python
# After _dispatch_action for a thread_directive call
result = _unwrap_result(raw_result)

# Handle context-limit continuation chains
if (action.get("item_id") == "rye/agent/threads/thread_directive"
    and result.get("status") == "continued"
    and result.get("continuation_thread_id")):

    continuation_id = result["continuation_thread_id"]

    # Resolve the full chain to the terminal thread
    terminal_id = orchestrator.resolve_thread_chain(continuation_id, Path(project_path))

    # Read the terminal thread's persisted result from registry
    registry = thread_registry.get_registry(Path(project_path))
    terminal_thread = registry.get_thread(terminal_id)
    if terminal_thread:
        persisted = terminal_thread.get("result", {})
        if isinstance(persisted, str):
            persisted = json.loads(persisted)
        result = {**result, **persisted, "status": persisted.get("status", "completed")}
```

Note: This is simpler than the previous approach of calling `orchestrator.wait_threads()` because the chain has already completed synchronously — we just need to read the persisted result, not wait for it.

**When `async_exec: true`:** The caller already expects to wait separately via `orchestrator.wait_threads()`. The `wait_threads` operation calls `_wait_single()`, which calls `resolve_thread_chain()` to follow continuation links. So async LLM nodes handle continuation chains automatically — `wait_threads` follows the chain to the terminal thread. The same `_bounded_result` fix ensures the terminal result is available.

**Design principle:** The walker doesn't implement continuation logic itself. It delegates to the existing `resolve_thread_chain()` infrastructure for chain resolution and reads persisted results from the registry.

---

## 7. Execution Context and Permissions

The graph walker runs as a subprocess (via `state_graph_runtime` → `subprocess` primitive). It is outside the `runner.py` LLM loop, which means `SafetyHarness` is not automatically present. The walker must resolve its own execution context.

### Context Resolution

The walker determines its execution context from two sources:

1. **Environment variable `RYE_PARENT_THREAD_ID`** — set by `thread_directive` before forking. If present, the walker reads the parent thread's `thread.json` to get capabilities, limits, and depth. **The walker must verify the `thread.json` signature** via `transcript_signer.verify_json(meta)` and fail-closed if invalid — `thread.json` contains security-relevant fields (capabilities, limits, depth) and is Ed25519-signed by `_write_thread_meta()`.
2. **Explicit `capabilities` parameter** — passed directly in the graph tool's params.

```python
def _resolve_execution_context(params, project_path):
    """Resolve capabilities and parent context for permission enforcement."""
    parent_thread_id = os.environ.get("RYE_PARENT_THREAD_ID")

    if parent_thread_id:
        meta = _read_thread_meta(project_path, parent_thread_id)
        if meta:
            # Verify thread.json signature (security-critical: capabilities/limits)
            transcript_signer = load_module("persistence/transcript_signer", anchor=_ANCHOR)
            if not transcript_signer.verify_json(meta):
                logger.warning("thread.json signature invalid for %s — fail-closed", parent_thread_id)
                return {"parent_thread_id": None, "capabilities": [], "limits": {}, "depth": 0}
            return {
                "parent_thread_id": parent_thread_id,
                "capabilities": meta.get("capabilities", []),
                "limits": meta.get("limits", {}),
                "depth": meta.get("limits", {}).get("depth", 0),
            }

    if "capabilities" in params:
        return {
            "parent_thread_id": None,
            "capabilities": params["capabilities"],
            "limits": params.get("limits", {}),
            "depth": params.get("depth", 5),
        }

    # No thread context, no explicit capabilities — fail-closed
    return {
        "parent_thread_id": None,
        "capabilities": [],  # empty = deny all in SafetyHarness
        "limits": {},
        "depth": 0,
    }
```

### Permission Enforcement

The walker must check capabilities before every dispatch. `ExecuteTool.handle()` verifies chain integrity (signatures) but does **not** check capabilities — that's `SafetyHarness.check_permission()`'s job in `runner.py`. Since the walker runs outside the runner's LLM loop, it must enforce capabilities itself.

The walker reuses the same `fnmatch`-based check from `SafetyHarness.check_permission()`:

```python
def _check_permission(exec_ctx, primary, item_type, item_id):
    """Check if action is permitted by resolved capabilities.

    Same logic as SafetyHarness.check_permission():
    - Empty capabilities = deny all (fail-closed)
    - Internal thread tools always allowed
    - Capability format: rye.<primary>.<item_type>.<item_id_dotted>
    - fnmatch wildcards for glob matching
    """
    if item_id and item_id.startswith("rye/agent/threads/internal/"):
        return None  # always allowed

    capabilities = exec_ctx.get("capabilities", [])
    if not capabilities:
        return {"status": "error", "error": f"Permission denied: no capabilities. Cannot {primary} {item_type} '{item_id}'"}

    if item_id:
        item_id_dotted = item_id.replace("/", ".")
        required = f"rye.{primary}.{item_type}.{item_id_dotted}"
    else:
        required = f"rye.{primary}.{item_type}"

    for cap in capabilities:
        if fnmatch.fnmatch(required, cap):
            return None

    return {"status": "error", "error": f"Permission denied: '{required}' not covered by capabilities"}
```

This is called before `_dispatch_action()` in the walker loop:

```python
# Check capabilities before dispatch
denied = _check_permission(exec_ctx, action.get("primary", "execute"),
                           action.get("item_type", "tool"), action.get("item_id", ""))
if denied:
    # Treat as node error — route through error handling (§13)
    result = denied
else:
    raw_result = _dispatch_action(action, project_path)
    result = _unwrap_result(raw_result)
```

**Inside a thread** (common case): The graph is called via `rye_execute` from an LLM thread. `RYE_PARENT_THREAD_ID` is set. The walker reads parent capabilities from `thread.json` and enforces them on every node dispatch. Each `rye_execute` call also goes through chain integrity verification automatically.

**Direct MCP call** (no thread): The caller must pass `capabilities` explicitly, otherwise the walker runs with empty capabilities. With empty capabilities, every node dispatch is denied (fail-closed). This is the correct behavior — you don't get permissions for free.

**Design invariant:** The graph walker never grants capabilities. It can only pass through capabilities it received from its parent context. This preserves the "children can only narrow, never escalate" rule. The capability check is the same `fnmatch` logic used by `SafetyHarness` — same format, same wildcards, same fail-closed default.

---

## 8. Node Execution — Just rye_execute

Every node action is dispatched through `rye_execute`. The graph walker doesn't care what kind of tool it's calling. Examples:

### Call a Python tool

```yaml
action:
  primary: execute
  item_type: tool
  item_id: utilities/code-analyzer
  params:
    files: "${inputs.files}"
```

### Call a bash tool

```yaml
action:
  primary: execute
  item_type: tool
  item_id: rye/bash/bash
  params:
    command: "wc -l ${inputs.file_path}"
```

### Load knowledge for context

```yaml
action:
  primary: load
  item_type: knowledge
  item_id: domain/scoring-rubric
```

### Search for tools

```yaml
action:
  primary: search
  item_type: tool
  params:
    query: "email sender"
    source: project
```

### Sign an item

```yaml
action:
  primary: sign
  item_type: tool
  item_id: "${state.generated_tool_id}"
```

All four primary actions (`execute`, `search`, `load`, `sign`) work because the walker routes each action dict through the appropriate primary tool. The node format is the same `{primary, item_type, item_id, params}` dict used by hooks, ToolDispatcher, and everywhere else in the system.

### Important: Directives vs Thread Execution

There are two distinct ways to use a directive in a graph node:

**Load the directive content** (no LLM):

```yaml
action:
  primary: execute
  item_type: directive
  item_id: workflows/code-review/review-fixes
  params:
    fixes: "${state.fixes}"
```

This calls `ExecuteTool.handle(item_type="directive", ...)` which parses the directive markdown/XML, validates inputs, interpolates placeholders, and **returns the parsed directive with its prompt text**. No LLM is involved. The directive content is available in `${result}` for downstream use — useful for loading instructions into state or passing them to another tool.

**Run the directive through an LLM thread**:

```yaml
action:
  primary: execute
  item_type: tool
  item_id: rye/agent/threads/thread_directive
  params:
    directive_name: workflows/code-review/review-fixes
    inputs:
      fixes: "${state.fixes}"
    limit_overrides:
      turns: 8
      spend: 0.10
```

This calls `thread_directive` as a tool, which spawns a full LLM thread — building the prompt from the directive, running the LLM loop with the safety harness, and returning the thread result. This is the pattern used for LLM nodes (§11).

**Run the directive through an LLM thread with structured outputs** (recommended for graph consumption):

```yaml
action:
  primary: execute
  item_type: tool
  item_id: rye/agent/threads/thread_directive
  params:
    directive_name: workflows/code-review/review-fixes
    inputs:
      fixes: "${state.fixes}"
    limit_overrides:
      turns: 8
      spend: 0.10
assign:
  verdict: "${result.outputs.verdict}"
  confidence: "${result.outputs.confidence}"
```

When the directive declares `<outputs>` fields, the LLM is instructed to call `rye/agent/threads/directive_return` with typed results. These are persisted to the registry as `{cost, outputs}` and returned in `result.outputs`. This is the preferred pattern for graph nodes because structured outputs are reliably available cross-process (unlike freeform `result.result` which is truncated to 4000 chars).

**The distinction:** `item_type: directive` loads and parses. `item_type: tool, item_id: rye/agent/threads/thread_directive` loads, injects into an LLM context, and executes. The graph walker doesn't need to know the difference — both are just action dicts routed through the same dispatch.

---

## 9. Result Unwrapping

`rye_execute` returns an envelope: `{status, type, item_id, data: {...}, chain, metadata}`. The graph walker must unwrap this so `${result.X}` resolves to the inner tool result, not the envelope.

The walker reuses the same unwrapping logic as `runner.py._clean_tool_result()`:

```python
def _unwrap_result(raw_result):
    """Unwrap rye_execute envelope to get the inner tool result.

    Same logic as runner.py._clean_tool_result():
    - If result has a 'data' key and item_id starts with 'rye/primary/',
      return the inner data dict.
    - Strip chain, metadata, resolved_env_keys, path, source.
    """
    if not isinstance(raw_result, dict):
        return raw_result

    DROP_KEYS = frozenset(("chain", "metadata", "path", "source", "resolved_env_keys"))

    inner = raw_result.get("data")
    if isinstance(inner, dict) and raw_result.get("item_id", "").startswith("rye/primary/"):
        return {k: v for k, v in inner.items() if k not in DROP_KEYS}

    return {k: v for k, v in raw_result.items() if k not in DROP_KEYS}
```

After unwrapping, `${result.issues}` resolves to the actual tool output field, not `${result.data.data.issues}`.

---

## 10. Conditional Edges

Edges use `condition_evaluator.matches()` — the exact same evaluator used by hooks in `safety_harness.py` and defined in `hook_conditions.yaml`. Same `path`/`op`/`value` format, same `any`/`all`/`not` combinators, same operator set (`eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `contains`, `regex`, `exists`). If you know how to write hook conditions, you know how to write graph edge conditions. No new DSL.

### Unconditional (string)

```yaml
next: process_results
```

### Conditional (list with `when`)

```yaml
next:
  - to: generate_fixes
    when:
      path: "state.issues"
      op: gt
      value: 0
  - to: approve # default — no when clause
```

### Complex conditions (any/all/not)

```yaml
next:
  - to: escalate
    when:
      all:
        - path: "state.score"
          op: lt
          value: 30
        - path: "state.priority"
          op: eq
          value: "high"
  - to: auto_resolve
    when:
      path: "state.score"
      op: gte
      value: 80
  - to: manual_review
```

### Conditions evaluate against `{state, result}`

The document passed to `condition_evaluator.matches()` is:

```python
doc = {"state": state, "result": result}
```

So `path: "state.issues"` resolves through the state dict. `path: "result.status"` resolves through the current node's unwrapped execution result.

---

## 11. LLM Nodes and Parent Propagation

LLM nodes are just nodes whose action calls `thread_directive`. The graph walker doesn't know or care that an LLM is involved — but it does need to inject parent context.

### The Problem

In `runner.py` (lines 291-295), calls to `thread_directive` get auto-injected with `parent_thread_id`, `parent_depth`, `parent_limits`, `parent_capabilities`. This injection happens inside the runner's dispatch loop. The graph walker runs outside that loop, so it must do this explicitly.

### The Solution

When the walker detects a node action targeting `rye/agent/threads/thread_directive`, it injects parent context from the resolved execution context (§7):

```python
def _inject_parent_context(params, exec_ctx):
    """Inject parent thread context for child thread spawns."""
    params = dict(params)
    if exec_ctx.get("parent_thread_id"):
        params.setdefault("parent_thread_id", exec_ctx["parent_thread_id"])
    if exec_ctx.get("depth") is not None:
        params.setdefault("parent_depth", exec_ctx["depth"])
    if exec_ctx.get("limits"):
        params.setdefault("parent_limits", exec_ctx["limits"])
    if exec_ctx.get("capabilities"):
        params.setdefault("parent_capabilities", exec_ctx["capabilities"])
    return params
```

This ensures spawned LLM threads inherit the correct permission/budget envelope. Without this, child threads would run with empty capabilities (fail-closed) or without budget limits.

### Synchronous LLM node

```yaml
review:
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/thread_directive
    params:
      directive_name: workflows/code-review/review-fixes
      inputs:
        fixes: "${state.fixes}"
      limit_overrides:
        turns: 8
        spend: 0.10
  assign:
    # Prefer structured outputs for reliable cross-process consumption
    verdict: "${result.outputs.verdict}"
    confidence: "${result.outputs.confidence}"
    # Alternatively, for freeform text (truncated to 4000 chars):
    # verdict: "${result.result}"
  next: report
```

The graph blocks until the thread completes, then assigns the result to state. If the child thread hits a context limit and triggers a continuation handoff, the walker automatically follows the chain (§6.2) — the node appears to complete normally from the graph's perspective.

**Best practice:** Directives called from graph nodes should declare `<outputs>` fields (e.g., `<output name="verdict" type="string" />`) so the LLM returns structured data via `directive_return`. This ensures outputs are persisted in the registry and available via `${result.outputs.<field>}`.

### Async LLM node (fan-out)

```yaml
spawn_reviews:
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/thread_directive
    params:
      directive_name: workflows/code-review/review-single
      inputs:
        file: "${state.current_file}"
      async_exec: true
      limit_overrides:
        turns: 6
        spend: 0.05
  assign:
    thread_ids: "${state.thread_ids + [result.thread_id]}"
  next: wait_for_reviews
```

Combined with a wait node. Note: `orchestrator.wait_threads` uses registry polling (`_poll_registry()`) when called from a subprocess — this works cross-process because thread status is persisted to SQLite:

```yaml
wait_for_reviews:
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/orchestrator
    params:
      operation: wait_threads
      thread_ids: "${state.thread_ids}"
      timeout: 300
  assign:
    review_results: "${result.results}"
  next: aggregate
```

---

## 12. Foreach and Parallel Fan-Out

A `foreach` node iterates over a list in state, executing its action for each item. This replaces the manual "spawn N children" pattern.

```yaml
review_each_file:
  type: foreach
  over: "${inputs.files}"
  as: current_file
  action:
    primary: execute
    item_type: tool
    item_id: rye/agent/threads/thread_directive
    params:
      directive_name: workflows/review/review-single
      inputs:
        file: "${current_file}"
      async_exec: true
      limit_overrides:
        turns: 6
        spend: 0.05
  collect: thread_ids
  next: wait_for_reviews
```

The walker expands this into N `rye_execute` calls, collects results into `state.thread_ids`, then advances to the next node.

**Parallel mode** (`async_exec: true` on the inner action) — all iterations spawn concurrently via `thread_directive`'s fork mechanism. The walker collects `thread_id` values and moves to the next node. Sequential mode (`async_exec` absent or false) — each iteration completes before the next starts.

---

## 13. Error Handling

### Node Execution Errors

When a node's `rye_execute` call returns `{status: "error"}`, the walker classifies the error via `error_loader.classify()` (same as `runner.py`) and evaluates three layers in order:

1. **`error` hooks** (§6.1) — evaluated first, using event name `"error"` (same as the thread system). The error context includes `{error, classification, node, state, step_count}` — the same `classification` shape produced by `runner.py`, so existing builtins (`default_retry_transient`, `default_fail_permanent`) fire identically. Hooks can return `{action: "retry"}` (re-execute the node, up to `max_retries`), `{action: "fail"}` (terminate immediately), or `{action: "abort"}` (terminate immediately).
2. **Error edge** — if no hook handled the error and the node declares `on_error: <node_id>`, route to that node. The error is available in `state._last_error`.
3. **Graph-level error mode** — if no hook handled and no error edge exists: `on_error: "fail"` (default) terminates the graph immediately; `on_error: "continue"` records the error in `state._last_error`, **skips the `assign` block** (to avoid overwriting state with empty strings from missing result paths), and proceeds to edge evaluation.

This ordering means infrastructure-level errors (transient failures, rate limits) are handled transparently by hooks, while application-level errors (API returned an error response, validation failed) route through graph-structural `on_error` edges.

### Error-Mode "continue" Behavior

When `on_error: "continue"` is active and no hook/edge handles the error:

- `state._last_error` is set with the error details
- The `assign` block is **skipped** — this prevents `${result.X}` expressions from resolving to empty strings (since the result is an error dict, not the expected tool output)
- Edge evaluation proceeds normally — edges can inspect `state._last_error` to route accordingly
- If no edge condition matches, `current` becomes `None` and the graph terminates

### Error Edge Example

```yaml
nodes:
  call_api:
    action:
      primary: execute
      item_type: tool
      item_id: integrations/external-api
      params:
        endpoint: "${inputs.endpoint}"
    assign:
      api_response: "${result.data}"
    on_error: handle_api_failure
    next: process_response

  handle_api_failure:
    action:
      primary: execute
      item_type: tool
      item_id: utilities/log-error
      params:
        error: "${state._last_error}"
    next: fallback_path
```

Note: If the `call_api` node fails due to a transient network error, the builtin `default_retry_transient` hook (§6.1) retries it up to 3 times before the `on_error` edge is evaluated. If the API returns a permanent error (e.g., 404), the `default_fail_permanent` hook fires, and if no graph-level hook overrides it, the `on_error: handle_api_failure` edge routes to the fallback path.

### Retry State

Retry counts are tracked in `state._retries` — a dict mapping node names to attempt counts. This allows edge conditions to inspect retry history:

```yaml
next:
  - to: alert_ops
    when:
      path: "state._retries.call_api"
      op: gte
      value: 2
  - to: process_response
```

### Interpolation Errors

Missing template paths resolve to empty string (existing interpolation behavior). The walker logs warnings for `assign` expressions that resolve to empty when the template was non-empty — this usually indicates a typo or unexpected result shape.

### Cancellation

The walker checks for a cancellation signal file (`.ai/threads/<thread_id>/cancel`) at each step. If present, the graph terminates with `status: cancelled` and state is persisted.

For spawned LLM child threads, the orchestrator provides two termination mechanisms:

- **`cancel_thread`** — cooperative cancellation: sets the `SafetyHarness._cancelled` flag in-process. The runner checks this at each turn and exits cleanly. Only works for threads in the same process.
- **`kill_thread`** — hard termination: sends SIGTERM using the registry's `pid` column, waits 3 seconds for graceful shutdown, then sends SIGKILL if still alive. Works cross-process. Updates registry status to `killed`.

---

## 14. Graph Validation

Before walking, the walker validates the graph definition:

| Check                                                  | Error                                                                                    |
| ------------------------------------------------------ | ---------------------------------------------------------------------------------------- |
| `start` references a node that exists                  | `"start node 'X' not found in nodes"`                                                    |
| Every `next` (string) references an existing node      | `"node 'X' references unknown node 'Y'"`                                                 |
| Every `next[].to` references an existing node          | `"node 'X' edge references unknown node 'Y'"`                                            |
| Every `on_error` references an existing node           | `"node 'X' on_error references unknown node 'Y'"`                                        |
| At least one `type: return` node exists                | `"graph has no return node"` (warning, not error — edge dead-ends are valid termination) |
| No required `config_schema` fields missing from params | `"missing required input: 'X'"`                                                          |

Validation runs before execution. If validation fails, the graph returns immediately with `{success: false, error: ...}` and no state is created.

---

## 15. The Execution Spectrum

The same graph can be invoked at any level:

### Level 1: Pure deterministic (direct MCP call)

```python
rye_execute(
    item_type="tool",
    item_id="workflows/code-review/graph",
    parameters={"files": ["src/auth.py"], "severity_threshold": "warning"}
)
```

No LLM involved in orchestration. The graph walks mechanically. Individual nodes may spawn LLM threads if they call `thread_directive`, but the graph traversal itself is deterministic.

### Level 2: LLM calls the graph as a tool

An LLM inside a thread decides when and how to call the graph. The graph executes mechanically once invoked. The LLM reasons about the final result.

```
LLM turn: "I should run the code review pipeline on these files"
  → tool_use: rye_execute(item_id="workflows/code-review/graph", params={...})
    → graph walks deterministically
  → tool_result: {state: {...}, steps: 4}
LLM turn: "The review found 3 issues, here's the summary..."
```

### Level 3: Directive wraps the graph

A directive instructs the LLM to run the graph as one step in a larger workflow.

```xml
<process>
  <step name="review">
    <action>
      Run the code-review graph on the changed files.
      Use the output to decide if the PR can be merged.
    </action>
  </step>
  <step name="decide">
    <action>
      Based on the review results, either approve or request changes.
    </action>
  </step>
</process>
```

### Level 4: Graphs within graphs

A graph node's action calls `rye_execute` on another graph tool. Nesting is bounded by `max_steps` per graph and thread depth limits.

---

## 16. Programmatic Tool Calling — An Emergent Property

Rye gets Anthropic's "programmatic tool calling" for free. It's not a feature — it's a consequence of how the system already works.

### What Anthropic Built

Anthropic's programmatic tool calling (beta, `advanced-tool-use-2025-11-20`) lets Claude write Python code that calls tools as async functions inside a sandboxed container. The key benefits:

1. **Multiple tool calls without re-invoking the model** — 10 tools = 1 model turn instead of 10
2. **Intermediate results stay out of context** — only the final output reaches the LLM
3. **Code can loop, branch, aggregate** — eliminates N model round-trips for data-heavy workflows

They built a special container runtime, a special `allowed_callers` field, a special `code_execution` tool, and special container lifecycle management to achieve this.

### Why Rye Already Has This

In rye, a signed Python tool can call `rye_execute` internally. That's it. That's the whole thing.

```python
# .ai/tools/utilities/batch-processor.py
# A normal signed Python tool

__version__ = "1.0.0"
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_function_runtime"

def execute(params, project_path):
    """Process 50 items — calls rye_execute for each, no LLM involved."""
    from rye.tools.execute import ExecuteTool
    tool = ExecuteTool(user_space)

    results = []
    for item in params["items"]:
        result = tool.handle(
            item_type="tool",
            item_id="utilities/process-single",
            project_path=project_path,
            parameters={"item": item},
        )
        results.append(result)

    # Only this summary reaches the caller (LLM or MCP)
    return {
        "processed": len(results),
        "successes": sum(1 for r in results if r.get("status") == "success"),
        "summary": _aggregate(results),
    }
```

When an LLM calls this tool:

1. **One model turn** — the LLM calls `rye_execute(item_id="utilities/batch-processor", params={...})`
2. **50 internal `rye_execute` calls happen** — each goes through the full chain (signing, verification, execution)
3. **No model invocation between calls** — the Python tool runs them sequentially or concurrently
4. **Only the summary returns to context** — the 50 intermediate results never touch the LLM's context window
5. **Full integrity chain preserved** — every internal tool call is signature-verified, same as if the LLM had called it directly

This is exactly what Anthropic's programmatic tool calling does, except:

| Anthropic                                   | Rye                                                   |
| ------------------------------------------- | ----------------------------------------------------- |
| Special beta header required                | Just write a tool                                     |
| Special `allowed_callers` field             | Normal tool, normal permissions                       |
| Sandboxed container with managed lifecycle  | Normal signed Python tool in the execution chain      |
| MCP tools cannot be called programmatically | All tools callable — they're just `rye_execute` calls |
| Container expires after ~4.5 minutes        | Tool runs until `timeout` in runtime config           |
| Claude writes the code at inference time    | Code is pre-written, signed, verified                 |

### The Deeper Point

Anthropic had to build special infrastructure because their tools are stateless function definitions — there's no execution chain, no signing, no way for a tool to call another tool through a verified path. They needed a container to bridge that gap.

Rye doesn't have that gap. A tool calling `rye_execute` IS the verified path. The integrity chain, permission model, and budget tracking apply equally whether the call comes from an LLM turn, a graph walker, or a Python tool calling `rye_execute` internally.

Programmatic tool calling in rye isn't a feature you enable. It's what happens when you write a tool that calls other tools.

### The State Graph Completes the Picture

The state graph tool is the data-driven version of programmatic tool calling. Instead of writing Python code that calls `rye_execute` in a loop, you declare the loop as a YAML graph:

| Approach                                         | When to Use                                                                 |
| ------------------------------------------------ | --------------------------------------------------------------------------- |
| **Python tool** calling `rye_execute` internally | Custom logic, complex aggregation, code you want full control over          |
| **State graph tool** with YAML nodes             | Declarative workflows, overridable nodes, registry-shareable pipelines      |
| **Directive + LLM thread**                       | Workflows that need reasoning, adaptation, natural language decision-making |

All three go through the same execution chain. All three are signed. All three keep intermediate results out of the LLM context. The difference is just how much is declared as data vs written as code vs delegated to an LLM.

---

## 17. Implementation Status

All phases are **complete**. The implementation lives in two new files plus one modified file:

| File                                         | Type           | Status       | Description                                                                                  |
| -------------------------------------------- | -------------- | ------------ | -------------------------------------------------------------------------------------------- |
| `rye/core/runtimes/state_graph_runtime.yaml` | Runtime YAML   | **Done** ✓   | Points to walker script, configures subprocess (same pattern as `python_script_runtime.yaml`) |
| `rye/core/runtimes/state_graph_walker.py`    | Runtime script | **Done** ✓   | Graph traversal engine (~550 lines, signed)                                                  |
| `agent/threads/loaders/interpolation.py`     | Prerequisite   | **Done** ✓   | Type-preserving interpolation — single `${path}` expressions return raw values               |

### Phase 0: Prerequisites ✓

| Change                            | File                       | Status       | Description                                                                                                                                                         |
| --------------------------------- | -------------------------- | ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------------------- |
| **Type-preserving interpolation** | `loaders/interpolation.py` | **Done** ✓   | When template is exactly `"${path}"` (single whole expression), returns raw value without `str()` conversion. Mixed templates retain string behavior. Enables numeric edge conditions (`op: gt, value: 0`). |
| **Bounded result persistence**    | `thread_directive.py`      | **Done** ✓   | `registry.set_result()` persists `{cost, outputs}`. Returned `result.result` is truncated to 4000 chars. |
| **Error classification import**   | (walker itself)            | **Done** ✓   | Walker imports `error_loader.classify()` to produce `{classification}` context for error hooks, matching the thread system's error context shape.                   |
| **thread.json signature verification** | (walker itself)       | **Done** ✓   | Walker calls `transcript_signer.verify_json()` when reading parent `thread.json` for context resolution. Fails-closed on invalid signature. |

### Phase 1: State Graph Runtime ✓

Walker implements all responsibilities described in §6–§14:

1. Graph validation (§14) — start node, edge references, on_error references, return node check
2. Registry integration (§5) — `registry.register()`, status transitions, `graph_run_id` as `thread_id`
3. Execution context resolution (§7) — parent `thread.json` with signature verification, explicit capabilities, fail-closed default
4. Hook merging (§6.1) — graph-level (layer 1) + applicable builtins (layer 2) + infra (layer 3), filtered by event applicability
5. Input validation against `config_schema`
6. State persistence as signed knowledge items (§5) — atomic write (temp → rename), YAML frontmatter + JSON body, `MetadataManager.create_signature()`
7. Node walking — interpolate action → inject parent context for thread spawns → `_check_permission()` → `await _dispatch_action()` → `_unwrap_result()` → continuation chain following (§6.2) → error classification + `error` hooks → graph-level error routing → assign (skipped on error) → edge evaluation → persist state → `after_step` hooks
8. Capability enforcement (§7) — same `fnmatch` logic as `SafetyHarness.check_permission()`
9. `max_steps` guard with `limit` hooks, cancellation via signal file
10. `graph_started` / `graph_completed` hooks

### Phase 2: Foreach Support ✓

- Sequential mode (default) — each iteration completes before the next
- Parallel mode (`async_exec: true` in inner action) — all iterations dispatched concurrently via `asyncio.gather()`
- Collects results into `state[collect]` (thread_ids for async, full results for sync)
- Permission checking and parent context injection per iteration

### Phase 3: Resume Support ✓

- `resume: true` + `graph_run_id` params to resume a failed/errored graph run
- Loads signed knowledge item, verifies signature via `MetadataManager.parse_and_verify()`
- Extracts `current_node`, `step_count`, and full state JSON from frontmatter + body
- Updates registry status back to `running`, continues walking from saved node
- Skips `graph_started` hooks on resume (already fired on original run)

### What Does NOT Need Building

| Capability                                 | Already Exists                                                  |
| ------------------------------------------ | --------------------------------------------------------------- |
| Tool execution with integrity verification | `ExecuteTool` + `PrimitiveExecutor`                             |
| Permission checking                        | `SafetyHarness.check_permission()` (fnmatch logic reused)       |
| Budget tracking                            | `budgets.py` (thread context)                                   |
| LLM thread spawning                        | `thread_directive.py`                                           |
| Thread waiting/aggregation                 | `orchestrator.py` (graph runs use same registry — `wait_threads` works out of the box) |
| Status tracking                            | `thread_registry.py` (graph runs register as rows — no schema changes) |
| Condition evaluation                       | `condition_evaluator.py` (used for edges AND hooks)             |
| Template interpolation                     | `interpolation.py` (used for actions AND hooks)                 |
| Hook format and layer ordering             | `hook_conditions.yaml` + `hooks_loader.py` (same merge pattern) |
| Tool signing                               | `SignTool`                                                      |
| Knowledge persistence                      | Knowledge write tools                                           |
| Space precedence                           | `DirectiveResolver` / tool resolution                           |
| Transcript signing                         | `TranscriptSigner` (checkpoint + JSON signing)                  |
| Streaming to transcript                    | `TranscriptSink` (JSONL + knowledge markdown)                   |
| Knowledge rendering                        | `Transcript.render_knowledge()` (signed knowledge entries)      |
| Result persistence                         | `registry.set_result()` with `{cost, outputs}`                  |
| Continuation chain management              | `registry.set_continuation()`, `set_chain_info()`, `get_chain()`|
| Thread resume                              | `orchestrator.resume_thread()` with integrity verification      |
| Thread hard termination                    | `orchestrator.kill_thread()` (SIGTERM/SIGKILL via PID)          |
| Structured outputs                         | `directive_return` + `<outputs>` directive sections              |

---

## 18. Examples

### Example 1: Data Processing Pipeline

```yaml
# .ai/tools/workflows/etl/process-sales.yaml
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
category: workflows/etl
description: "Extract sales data, transform, load into report"

grapconfig_schema:
  type: object
  properties:
    region:
      type: string
    quarter:
      type: string
  required: [region, quarter]

config:
  start: extract
  max_steps: 20

  nodes:
    extract:
      action:
        primary: execute
        item_type: tool
        item_id: data/query-database
        params:
          sql: "SELECT * FROM sales WHERE region = '${inputs.region}' AND quarter = '${inputs.quarter}'"
      assign:
        raw_data: "${result.rows}"
        row_count: "${result.row_count}"
      next:
        - to: transform
          when:
            path: "state.row_count"
            op: gt
            value: 0
        - to: empty_report

    transform:
      action:
        primary: execute
        item_type: tool
        item_id: data/aggregate-sales
        params:
          rows: "${state.raw_data}"
          group_by: "customer"
      assign:
        aggregated: "${result.aggregated}"
        top_customers: "${result.top_5}"
      next: generate_report

    generate_report:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: workflows/etl/write-sales-report
          inputs:
            aggregated: "${state.aggregated}"
            top_customers: "${state.top_customers}"
            region: "${inputs.region}"
            quarter: "${inputs.quarter}"
          limit_overrides:
            turns: 6
            spend: 0.05
      assign:
        report_path: "${result.outputs.report_path}"
      next: done

    empty_report:
      action:
        primary: execute
        item_type: tool
        item_id: rye/file-system/fs_write
        params:
          path: ".ai/data/reports/${inputs.region}-${inputs.quarter}-empty.md"
          content: "No sales data for ${inputs.region} Q${inputs.quarter}"
      assign:
        report_path: "${result.path}"
      next: done

    done:
      type: return
```

Invocation:

```python
rye_execute(
    item_type="tool",
    item_id="workflows/etl/process-sales",
    parameters={"region": "West", "quarter": "Q1"}
)
```

### Example 2: Multi-File Review with Fan-Out

```yaml
# .ai/tools/workflows/review/multi-file-review.yaml
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
category: workflows/review
description: "Review multiple files in parallel, aggregate verdicts"

config_schema:
  type: object
  properties:
    files:
      type: array
  required: [files]

config:
  start: spawn_reviews
  max_steps: 100

  nodes:
    spawn_reviews:
      type: foreach
      over: "${inputs.files}"
      as: current_file
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: workflows/review/review-single-file
          inputs:
            file: "${current_file}"
          async_exec: true
          limit_overrides:
            turns: 8
            spend: 0.05
      collect: thread_ids
      next: wait

    wait:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/orchestrator
        params:
          operation: wait_threads
          thread_ids: "${state.thread_ids}"
          timeout: 300
      assign:
        wait_results: "${result.results}"
      next: aggregate

    aggregate:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/orchestrator
        params:
          operation: aggregate_results
          thread_ids: "${state.thread_ids}"
      assign:
        all_results: "${result.results}"
      next: done

    done:
      type: return
```

---

## 19. Guardrails and Limits

### Graph-Level

| Guard       | Default | Source                             |
| ----------- | ------- | ---------------------------------- |
| `max_steps` | 100     | Graph tool YAML `config.max_steps` |
| `timeout`   | 600s    | Runtime YAML `config.timeout`      |

### Per-Node

Tool calls inherit all existing guards:

- Integrity verification (signature check before execution via chain verification)
- Capability enforcement (walker checks `_check_permission()` before every dispatch — same `fnmatch` logic as `SafetyHarness`)
- Hook-based error handling (`error` hooks with `error_loader.classify()` for transparent retry of transient failures — §6.1)
- Budget limits (child LLM threads inherit parent limits, capped by `min()`)
- Tool timeout (from runtime config)

### State Size

Graph state is a knowledge item on disk. No offload needed — there's no context window to protect. The final graph result is bounded by `tool_result_guard` when it returns to a calling LLM thread.

**Note:** LLM child threads do have context pressure and mitigate it via continuation handoff + `tool_result_guard`. When graphs call into LLM threads, prefer structured outputs (`<outputs>` + `directive_return`) over large freeform text to ensure reliable cross-process result retrieval. Freeform `result.result` is truncated to 4000 chars by `thread_directive.py`.

### Cycle Detection

The walker tracks visited `(node_id, step_count)` pairs. If a node is visited more than `max_steps` times total, the graph terminates with an error.

### Integrity and Observability

| Layer                    | Mechanism                                                    | Applies To                    |
| ------------------------ | ------------------------------------------------------------ | ----------------------------- |
| Tool chain integrity     | Ed25519 signature verification before execution              | All `rye_execute` calls       |
| Thread metadata          | `thread.json` signed via `sign_json()`, verified on read     | Parent context resolution     |
| Transcript integrity     | Checkpoint signing at turn boundaries (`TranscriptSigner`)   | LLM child threads             |
| Handoff/resume integrity | `TranscriptSigner.verify()` before reconstructing messages   | Continuation chains           |
| Streaming observability  | `TranscriptSink` writes `token_delta` to JSONL + knowledge   | Streaming LLM providers       |
| Knowledge rendering      | `transcript.render_knowledge()` at checkpoint cadence        | All LLM threads               |
| Registry chain metadata  | `continuation_of`, `continuation_thread_id`, `chain_root_id` | Continuation chain navigation |
| Cost tracking            | `registry.update_cost_snapshot()` per-turn                   | LLM threads                   |

---

## 20. What This Replaces in LangGraph Terms

| LangGraph                       | Rye State Graph                                                |
| ------------------------------- | -------------------------------------------------------------- |
| Python class defining a graph   | Signed YAML tool file                                          |
| `StateGraph(TypedDict)`         | `config_schema` in tool YAML                                   |
| `graph.add_node("name", fn)`    | `nodes.name.action` (action dict)                              |
| `graph.add_edge("a", "b")`      | `nodes.a.next: b`                                              |
| `graph.add_conditional_edges()` | `nodes.a.next: [{to: b, when: ...}]`                           |
| Compiled graph object           | Signed YAML tool resolved by runtime                           |
| `graph.invoke(state)`           | `rye_execute(item_id="...", params={...})`                     |
| State passed as TypedDict       | State persisted as signed knowledge item, status tracked in thread registry |
| No signing, no integrity        | Ed25519 signed, verified before execution                      |
| No permission model             | Fail-closed capabilities via thread context                    |
| Not shareable across projects   | Registry-shareable, space-precedence overridable               |
| Graph defined in code           | Graph defined in data — overridable without code changes       |
| LLM is the only execution mode  | Execution spectrum: pure graph, LLM + graph, directive + graph |
