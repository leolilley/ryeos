# Thread Lifecycle Events + Hook-Driven Handoff

> **Status:** Implemented. Three focused changes landed: (1) `thread_started` / `thread_continued` event split so hooks fire for continuation threads, (2) decoupled hardcoded summary from `handoff_thread()` so it's hook-driven, (3) `after_complete` hook dispatch. A fourth change — `inputs` parameter flow + `item_id` interpolation — solved the broken hardcoded knowledge IDs. Enables declared knowledge hooks for deterministic cross-thread context wiring. No RAG, no embeddings, no vector index.

---

## The Problem

Two things were broken in the thread lifecycle:

1. **No hooks fired for continuation threads.** When `resume_messages` was provided, `runner.py` skipped `run_hooks_context()` entirely. The continuation thread got no identity, no rules, no injected knowledge. Hook-based context injection didn't work for handoff continuations.

2. **Handoff summary was hardcoded.** `orchestrator.handoff_thread()` Phase 1 always spawned a `thread_summary` directive (LLM call, ~$0.005) on every handoff. The infrastructure forced this regardless of whether the directive wanted or needed it.

3. **`after_complete` hooks never fired.** Defined in `hook_conditions.yaml` (the `infra_completion_signal` hook) but no code called `harness.run_hooks("after_complete", ...)`. Dead code.

4. **Knowledge IDs couldn't be wired at plan time.** The orchestrator couldn't hardcode knowledge IDs like `agent/threads/implement_database/implement_database-1740200000` into hook declarations — timestamps aren't known until threads complete. Required a dynamic interpolation mechanism.

---

## What This Enables

With the event split, hook dispatch, and `inputs` interpolation working, directives wire up cross-thread dependencies declaratively:

```xml
<!-- implement_api directive — inject database schema decisions at startup -->
<hooks>
  <hook id="inject_database_context" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.database_thread_id}" />
    <description>Database schema decisions needed for API implementation</description>
  </hook>

  <hook id="summarize_on_handoff" event="after_complete">
    <condition path="cost.turns" op="gte" value="1" />
    <action primary="execute" item_type="tool" item_id="rye/agent/threads/thread_summary">
      <param name="thread_id">${thread_id}</param>
    </action>
    <description>Summarize thread if it did meaningful work</description>
  </hook>
</hooks>
```

The orchestrator knows the dependency graph. It wrote the wave plan. It passes resolved thread IDs as `inputs` at spawn time:

```python
spawn_params = {
    "directive_id": "implement_api",
    "inputs": {
        "database_thread_id": "implement_database/implement_database-1740200100",
        "scaffold_thread_id": "scaffold_project/scaffold_project-1740200000",
    },
}
```

The `${inputs.*}` references in hook declarations get interpolated at hook execution time via `interpolate_action()`. No RAG, no embedding, no vector search. Deterministic, zero API calls for recall, zero false positives.

---

## Changes

### 1. Event Split: `thread_started` vs `thread_continued`

**File: `safety_harness.py`**

`event` parameter added to `run_hooks_context()` — required, no default:

```python
async def run_hooks_context(
    self,
    context: Dict,
    dispatcher: Any,
    event: str,
) -> str:
    """Run context-injection hooks for a given event and collect context blocks.

    Unlike run_hooks(), this method:
    - Filters by the specified event (not hardcoded to thread_started)
    - Runs ALL matching hooks (no short-circuit)
    - Maps LoadTool results: result["data"]["content"] → context block
    - Returns concatenated context string (empty string if no hooks matched)
    """
    context_blocks = []
    for hook in self.hooks:
        if hook.get("event") != event:
            continue
        if not condition_evaluator.matches(context, hook.get("condition", {})):
            continue

        action = hook.get("action", {})
        interpolated = interpolation.interpolate_action(action, context)
        result = await dispatcher.dispatch(interpolated)

        if result and result.get("status") == "success":
            data = result.get("data", {})
            content = data.get("content") or data.get("body") or data.get("raw", "")
            if content:
                context_blocks.append(content.strip())

    return "\n\n".join(context_blocks)
```

**File: `runner.py`**

`directive_body`, `previous_thread_id`, and `inputs` parameters added. The right event fires based on `resume_messages`:

```python
async def run(
    thread_id: str,
    user_prompt: str,
    harness: "SafetyHarness",
    provider: "ProviderAdapter",
    dispatcher: "ToolDispatcher",
    emitter: "EventEmitter",
    transcript: Any,
    project_path: Path,
    resume_messages: Optional[List[Dict]] = None,
    directive_body: str = "",
    previous_thread_id: Optional[str] = None,
    inputs: Optional[Dict] = None,
) -> Dict:
```

First-message construction uses the event split:

```python
try:
    if resume_messages:
        # Continuation mode: fire thread_continued hooks
        messages = list(resume_messages)
        hook_context = await harness.run_hooks_context(
            {
                "directive": harness.directive_name,
                "directive_body": directive_body,
                "model": provider.model,
                "limits": harness.limits,
                "previous_thread_id": previous_thread_id,
                "inputs": inputs or {},
            },
            dispatcher,
            event="thread_continued",
        )
        if hook_context and messages:
            # Inject context near the last user message, not at position 0.
            # insert(0) would disrupt the reconstructed conversation chronology
            # and push context far from the continuation ask.
            last_user_idx = len(messages) - 1
            for i in range(len(messages) - 1, -1, -1):
                if messages[i].get("role") == "user":
                    last_user_idx = i
                    break
            messages[last_user_idx]["content"] = (
                hook_context + "\n\n" + messages[last_user_idx]["content"]
            )
    else:
        # Fresh thread: fire thread_started hooks (identity, rules, knowledge)
        hook_context = await harness.run_hooks_context(
            {
                "directive": harness.directive_name,
                "directive_body": directive_body,
                "model": provider.model,
                "limits": harness.limits,
                "inputs": inputs or {},
            },
            dispatcher,
            event="thread_started",
        )
        first_message_parts = []
        if hook_context:
            first_message_parts.append(hook_context)
        first_message_parts.append(user_prompt)
        messages.append({"role": "user", "content": "\n\n".join(first_message_parts)})
```

**File: `thread_directive.py`**

Extracts `inputs` from params, builds clean directive body text, and passes `directive_body`, `previous_thread_id`, and `inputs` to runner:

```python
inputs = params.get("inputs", {})

# Build clean directive text
directive_body = directive.get("body", "").strip()
directive_desc = directive.get("description", "")
clean_directive_text = "\n".join(filter(None, [
    directive_name, directive_desc, directive_body
]))

# Pass to runner.run() (both sync and fork paths):
result = await runner.run(
    thread_id, user_prompt, harness, provider,
    dispatcher, emitter, transcript, proj_path,
    resume_messages=params.get("resume_messages"),
    directive_body=clean_directive_text,
    previous_thread_id=params.get("previous_thread_id"),
    inputs=inputs,
)
```

`previous_thread_id` is passed from `orchestrator.handoff_thread()` via spawn_params when creating the continuation thread. It's the old thread's ID — known at spawn time. `inputs` flows from the orchestrator's spawn call through `thread_directive` into `runner`, where it lands in the hook context dict.

### 2. Add `after_complete` Hook Dispatch

**File: `runner.py`**

Added to the `finally` block, after `render_knowledge_transcript()`:

```python
finally:
    cost["elapsed_seconds"] = time.monotonic() - start_time
    final = {
        **cost,
        "status": "completed" if cost.get("turns") else "error",
    }
    orchestrator.complete_thread(thread_id, final)

    transcript.render_knowledge_transcript(
        directive=harness.directive_name,
        status=final["status"],
        model=provider.model,
        cost=cost,
    )

    # Dispatch after_complete hooks (best-effort)
    try:
        await harness.run_hooks(
            "after_complete",
            {"thread_id": thread_id, "cost": cost, "project_path": str(project_path)},
            dispatcher,
            {"emitter": emitter, "transcript": transcript, "thread_id": thread_id},
        )
    except Exception:
        pass  # after_complete hooks must not break thread finalization
```

This runs in async context in both sync and fork paths (runner.run() is called via asyncio.run() in the fork child). No separate fork-path dispatch needed.

### 3. Decouple Hardcoded Summary from `handoff_thread()`

**File: `orchestrator.py`**

Phase 1 (summary directive spawn) removed from `handoff_thread()`. The summary is now the completing thread's responsibility via hooks — if the directive declares an `after_complete` hook that triggers summarization, the summary gets written as a knowledge entry. The continuation thread loads it via a `thread_continued` hook.

Previous flow:

```
handoff_thread():
  1. Spawn thread_summary directive (hardcoded LLM call)  ← REMOVED
  2. Fill resume ceiling with trailing messages
  3. Build resume_messages (summary + trailing + continue message)
  4. Spawn continuation thread
  5. Link old → new in registry
```

Implemented flow:

```
handoff_thread():
  1. Fill resume ceiling with trailing messages
  2. Build resume_messages (trailing + continue message)
  3. Pass previous_thread_id to spawn params
  4. Spawn continuation thread
  5. Link old → new in registry
```

The summary config in `coordination.yaml` (`summary_directive`, `summary_model`, `summary_limit_overrides`, `summary_max_tokens`) stays as config that directives can reference in their hooks, but the infrastructure doesn't force-invoke it.

**Key change in spawn_params:**

```python
spawn_params = {
    "directive_id": directive_name,
    "resume_messages": resume_messages,
    "previous_thread_id": thread_id,
}
if parent_id:
    spawn_params["parent_thread_id"] = parent_id
```

Same change in `resume_thread` operation.

### 4. Event Definitions

**File: `events.yaml`**

`thread_continued` event type added:

```yaml
thread_continued:
  category: lifecycle
  criticality: critical
  description: "Continuation thread execution begins (handoff from prior thread)"
  payload_schema:
    type: object
    required: [directive, model, previous_thread_id]
    properties:
      directive: { type: string }
      model: { type: string }
      previous_thread_id: { type: string }
      limits: { type: object }
```

`thread_resumed` (same thread, new message via `resume_thread` operation) is already handled by the existing `resume_thread` flow in orchestrator.py — it reconstructs messages and spawns a new thread. No new event needed since it goes through the same `thread_started` or `thread_continued` path.

### 5. Hook Definitions

**File: `hook_conditions.yaml`**

The existing `thread_started` hooks (identity, rules) continue to work unchanged — they filter on `event: "thread_started"` and `run_hooks_context()` requires the caller to pass the event explicitly.

For `thread_continued`, directives declare what context they need. No new infra hooks needed — the existing identity/rules hooks could be duplicated for `thread_continued` if desired, but continuation threads already have reconstructed messages with that context.

### 6. Knowledge ID Interpolation

**File: `interpolation.py`**

`interpolate_action()` now interpolates the `item_id` field in addition to `params`:

```python
def interpolate_action(action: Dict, context: Dict) -> Dict:
    """Interpolate all ${...} in an action's interpolable fields.

    Interpolates: item_id, params.
    Preserves: primary, item_type.
    """
    result = dict(action)
    if "item_id" in result:
        result["item_id"] = interpolate(result["item_id"], context)
    if "params" in result:
        result["params"] = interpolate(result["params"], context)
    return result
```

This is what makes `${inputs.*}` work in hook declarations. The `inputs` dict is part of the hook context (set by `runner.py`), so when a hook declares:

```yaml
item_id: "agent/threads/${inputs.database_thread_id}"
```

…`interpolate_action()` resolves `${inputs.database_thread_id}` from the context at hook execution time. The orchestrator passes the resolved thread ID as an input at spawn time — no need to predict timestamps.

**File: `runner.py`**

Both `thread_started` and `thread_continued` hook contexts include `"inputs": inputs or {}`, making `${inputs.*}` available for interpolation in all hook actions.

**File: `thread_directive.py`**

`inputs` extracted from params (`params.get("inputs", {})`) and passed through to `runner.run()`.

---

## What Stays the Same

- `render_knowledge_transcript()` in runner.py's `finally` block — renamed from `render_knowledge()`
- Thread registry, budget ledger, transcript signing — unchanged
- `_build_prompt()` — unchanged
- `check_limits()`, `run_hooks()` for error/limit/after_step — unchanged
- `thread_summary` directive — unchanged (still exists, just not force-invoked)
- Existing `thread_started` hooks (identity, rules) — unchanged behavior

---

## File Summary

| File                  | Change                                                                                                                                     | Lines |
| --------------------- | ------------------------------------------------------------------------------------------------------------------------------------------ | ----- |
| `safety_harness.py`   | `event` parameter on `run_hooks_context()` (required, no default)                                                                          | ~5    |
| `runner.py`           | Event split in first-message construction, `directive_body` + `previous_thread_id` + `inputs` params, `after_complete` dispatch in finally | ~30   |
| `thread_directive.py` | Build clean directive text, pass `directive_body` + `previous_thread_id` + `inputs` to runner (both sync and fork paths)                   | ~10   |
| `orchestrator.py`     | Remove Phase 1 summary spawn from `handoff_thread()`, add `previous_thread_id` to spawn_params (both handoff and resume paths)             | ~25   |
| `events.yaml`         | Add `thread_continued` event type                                                                                                          | ~10   |
| `interpolation.py`    | Interpolate `item_id` field (not just `params`) in `interpolate_action()`                                                                  | ~5    |
| **Total**             |                                                                                                                                            | ~85   |

No new files. No new tools. No new config. Six patches to existing files.

---

## Real Example: Track Blox Wave Build

The Track Blox build pipeline uses two hook systems working at different layers — **graph hooks** for the deterministic pipeline scaffold, and **directive hooks** for thread-level knowledge wiring.

### Two-Layer Hook Architecture

**Graph hooks** (YAML in `config.hooks`) fire at pipeline lifecycle points. The state graph defines the wave structure — which directives to spawn, in what order, with what inputs. Graph hooks handle pipeline-level concerns: logging progress, recording metrics, handling pipeline-wide errors.

**Directive hooks** (XML in each directive's `<metadata>`) fire at thread lifecycle points. Each child directive declares what knowledge it needs at startup (`thread_started`), what to summarize on completion (`after_complete`), and what to re-inject after a context-limit handoff (`thread_continued`).

The graph decides **what** runs and **when**. The directive hooks decide **what context each thread sees**.

#### The Pipeline Graph

The state graph defines the deterministic wave scaffold:

```yaml
# .ai/tools/track-blox/workflows/build_pipeline.yaml
version: "1.0.0"
tool_type: graph
executor_id: rye/core/runtimes/state_graph_runtime
description: "Track Blox build pipeline — scaffold → database+scraper → api → dashboard"

config:
  start: scaffold
  max_steps: 20

  # Graph hooks — YAML format, pipeline lifecycle events
  hooks:
    - event: graph_started
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo 'Pipeline started: Track Blox build'"

    - event: after_step
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo 'Completed node: ${state._current_node}'"

    - event: graph_completed
      action:
        primary: execute
        item_type: tool
        item_id: rye/bash/bash
        params:
          command: "echo 'Pipeline finished'"

  nodes:
    scaffold:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: track-blox/scaffold_project
      assign:
        scaffold_thread_id: "${result.thread_id}"
      next: wave_1_fan_out

    wave_1_fan_out:
      type: foreach
      over:
        - directive: track-blox/implement_database
        - directive: track-blox/implement_scraper
      as: task
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: "${task.directive}"
          inputs:
            scaffold_thread_id: "${state.scaffold_thread_id}"
          async: true
      collect: wave_1_thread_ids
      next: wave_1_wait

    wave_1_wait:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/orchestrator
        params:
          operation: wait_threads
          thread_ids: "${state.wave_1_thread_ids}"
          timeout: 300
      assign:
        database_thread_id: "${result.results.0.thread_id}"
        scraper_thread_id: "${result.results.1.thread_id}"
      next: implement_api

    implement_api:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: track-blox/implement_api
          inputs:
            scaffold_thread_id: "${state.scaffold_thread_id}"
            database_thread_id: "${state.database_thread_id}"
      assign:
        api_thread_id: "${result.thread_id}"
      next: implement_dashboard

    implement_dashboard:
      action:
        primary: execute
        item_type: tool
        item_id: rye/agent/threads/thread_directive
        params:
          directive_name: track-blox/implement_dashboard
          inputs:
            scaffold_thread_id: "${state.scaffold_thread_id}"
            api_thread_id: "${state.api_thread_id}"
      assign:
        dashboard_thread_id: "${result.thread_id}"
      next: done

    done:
      type: return
```

The graph nodes spawn thread directives and pass dependency thread IDs through state interpolation (`${state.*}`). Graph hooks fire at **pipeline** transitions — `graph_started`, `after_step`, `graph_completed`. But the knowledge wiring happens inside the directives themselves via **directive hooks**.

#### Where Each Hook System Acts

| Concern                      | Hook System    | Format             | Event               |
| ---------------------------- | -------------- | ------------------ | -------------------- |
| Pipeline progress logging    | Graph hook     | YAML `config.hooks`| `after_step`         |
| Pipeline error handling      | Graph hook     | YAML `config.hooks`| `error`              |
| Pipeline completion          | Graph hook     | YAML `config.hooks`| `graph_completed`    |
| Thread knowledge injection   | Directive hook | XML `<hooks>`      | `thread_started`     |
| Thread summarization         | Directive hook | XML `<hooks>`      | `after_complete`     |
| Continuation re-injection    | Directive hook | XML `<hooks>`      | `thread_continued`   |

Both use the same underlying infrastructure (`condition_evaluator`, `interpolation`), but serve different purposes at different layers.

---

### Wave-by-Wave Walkthrough

The graph provides the scaffold. Each wave below shows what happens at the **directive hook** layer when the graph node spawns a thread.

### Wave 0: scaffold_project

Starts fresh. `thread_started` fires — identity and rules hooks inject as normal. Thread completes, `after_complete` fires. `render_knowledge_transcript()` has written the transcript knowledge entry at `.ai/knowledge/agent/threads/scaffold_project/scaffold_project-1740200000.md`. The orchestrator now has this path.

### Wave 1: implement_database + implement_scraper (parallel)

The orchestrator spawns both directives, passing the scaffold thread ID as an input:

```python
# Orchestrator spawns implement_database
spawn_params = {
    "directive_id": "implement_database",
    "inputs": {
        "scaffold_thread_id": "scaffold_project/scaffold_project-1740200000",
    },
}
```

```xml
<!-- implement_database directive hooks (written by orchestrator) -->
<hooks>
  <hook id="inject_scaffold" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.scaffold_thread_id}" />
  </hook>
</hooks>
```

Both threads start. `thread_started` fires — identity, rules, and the declared scaffold knowledge all inject into the first message. `${inputs.scaffold_thread_id}` resolves to `scaffold_project/scaffold_project-1740200000` at hook execution time. Both threads see the project structure decisions. Zero API calls for context wiring. Deterministic.

Both complete. Knowledge transcript entries written. Orchestrator has both paths.

### Wave 2: implement_api

The orchestrator knows the API needs the database schema. It wires both Wave 0 and Wave 1 outputs via inputs:

```python
spawn_params = {
    "directive_id": "implement_api",
    "inputs": {
        "scaffold_thread_id": "scaffold_project/scaffold_project-1740200000",
        "database_thread_id": "implement_database/implement_database-1740200100",
    },
}
```

```xml
<hooks>
  <hook id="inject_scaffold" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.scaffold_thread_id}" />
  </hook>
  <hook id="inject_database" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.database_thread_id}" />
  </hook>
</hooks>
```

The API thread starts with column names, types, constraints already in context. No embedding, no similarity search, no false positives. The orchestrator knew the dependency and wired it.

### Wave 3: implement_dashboard (hits context limit + continuation)

The orchestrator spawns implement_dashboard with dependency inputs:

```python
spawn_params = {
    "directive_id": "implement_dashboard",
    "inputs": {
        "scaffold_thread_id": "scaffold_project/scaffold_project-1740200000",
        "api_thread_id": "implement_api/implement_api-1740200200",
    },
}
```

The directive declares hooks for startup context, opt-in summarization, and continuation re-injection:

```xml
<!-- implement_dashboard directive hooks -->
<hooks>
  <!-- thread_started: inject dependency context -->
  <hook id="inject_scaffold" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.scaffold_thread_id}" />
  </hook>

  <hook id="inject_api" event="thread_started">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.api_thread_id}" />
  </hook>

  <!-- after_complete: opt-in summarization -->
  <hook id="summarize_on_complete" event="after_complete">
    <condition path="cost.turns" op="gte" value="1" />
    <action primary="execute" item_type="tool" item_id="rye/agent/threads/thread_summary">
      <param name="thread_id">${thread_id}</param>
    </action>
    <description>Summarize thread if it did meaningful work</description>
  </hook>

  <!-- thread_continued: re-inject critical context after handoff -->
  <hook id="reinject_api_context" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.api_thread_id}" />
    <description>API endpoints and types needed for remaining dashboard views</description>
  </hook>

  <hook id="reinject_previous_summary" event="thread_continued">
    <action primary="execute" item_type="knowledge" item_id="agent/threads/${inputs.previous_summary_id}" />
    <description>Summary of what was built before the handoff</description>
  </hook>
</hooks>
```

**Thread A starts.** `thread_started` fires — scaffold and API knowledge inject. Builds 3 of 5 views, hits 90% context.

**Context limit triggers `handoff_thread()`.** No hardcoded summary spawn. But the directive declared `summarize_on_complete` on `after_complete`, so:

1. `after_complete` fires → runs `thread_summary` tool → writes summary as knowledge entry at `agent/threads/implement_dashboard/implement_dashboard-1740200300`
2. `handoff_thread()` packs trailing messages into `resume_messages`, spawns continuation thread B with:

```python
spawn_params = {
    "directive_id": "implement_dashboard",
    "resume_messages": resume_messages,
    "previous_thread_id": "implement_dashboard/implement_dashboard-1740200300",
    "inputs": {
        "scaffold_thread_id": "scaffold_project/scaffold_project-1740200000",
        "api_thread_id": "implement_api/implement_api-1740200200",
        "previous_summary_id": "implement_dashboard/implement_dashboard-1740200300",
    },
}
```

**Thread B starts.** `thread_continued` fires (not `thread_started`). Two hooks match:

- `reinject_api_context` loads the API thread's knowledge → thread B has endpoint definitions, types, route structure
- `reinject_previous_summary` loads Thread A's summary → thread B knows which 3 views are done and what 2 remain

Thread B has: reconstructed trailing turns + API context + summary of prior work. It picks up where Thread A left off with full awareness of what was built.

### What About Unknown Relevance?

The scraper thread discovered that Roblox rate limits at ~100 req/min. The orchestrator didn't think to wire that into `implement_api`. The API thread doesn't know about it.

For V1, this is fine. The orchestrator can be prompted to search for related threads before wiring dependencies (a deliberate search step at the start of each wave). Or the rate limit discovery gets surfaced when the API thread hits the same limit and debugs it.

If this becomes a recurring pain point — repeatedly wishing "the agent should have known X from that old thread" — that's when RAG earns its place. Build for a problem you're experiencing, not predicting.
