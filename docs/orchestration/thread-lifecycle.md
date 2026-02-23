```yaml
id: thread-lifecycle
title: "Thread Lifecycle"
description: How threads are created, executed, and finalized
category: orchestration
tags: [threads, lifecycle, states, registry]
version: "1.1.0"
```

# Thread Lifecycle

Every thread follows a deterministic lifecycle: generate an ID, register in the registry, load the directive, resolve limits and permissions, run the LLM loop, finalize spend and status.

## Thread States

```
created ──→ running ──→ completed
                    ├──→ error
                    ├──→ cancelled
                    └──→ continued
```

| State       | Meaning |
|-------------|---------|
| `created`   | Registered in registry, not yet executing |
| `running`   | LLM loop is active |
| `completed` | Finished successfully — result available |
| `error`     | Failed — error message in result |
| `cancelled` | Cancelled via `cancel_thread` operation |
| `continued` | Handed off to a new thread (context limit reached) |

## Thread ID Generation

Thread IDs are derived from the directive name and a Unix epoch timestamp:

```python
thread_id = f"{directive_name}-{int(time.time())}"
# Example: "agency-kiwi/discover_leads-1739820456"
```

This makes thread IDs human-readable (you can see which directive spawned them) and unique (epoch seconds prevent collisions for typical usage).

## The Full Execution Flow

`thread_directive.execute()` runs these steps in order:

### Step 1: Resolve parent context

The thread discovers its parent through three sources (first match wins):

1. Explicit `parent_thread_id` parameter (used by handoff/resume)
2. `RYE_PARENT_THREAD_ID` environment variable (set by parent threads)
3. No parent — this is a root thread

If a parent is declared but its `thread.json` doesn't exist, execution fails immediately.

### Step 2: Register thread

The thread is registered in the SQLite registry (`registry.db`) with status `created`, its directive name, and parent ID. The registry tracks all threads across the project.

### Step 3: Load directive

The directive is loaded via `DirectiveResolver`, searching project → user → system spaces. The `markdown/xml` parser extracts metadata (limits, permissions, model, inputs) from the XML fence and preserves the raw content for the LLM prompt.

For normal execution, `ExecuteTool` handles input validation and interpolation. For resume/handoff, `LoadTool` is used instead (no input validation needed since the directive ran before).

### Step 4: Resolve limits

Limits are resolved through a layered merge:

```
defaults (resilience.yaml) → directive metadata → limit_overrides → parent upper bounds
```

Parent limits **cap** all values via `min()`. A child can never exceed its parent's limits. Depth decrements by 1 per level — if the parent has `depth: 5`, the child gets `depth: 4`.

### Step 5: Check depth

If resolved depth is less than 0 (i.e., the parent's depth was already exhausted), the thread returns an error immediately. This prevents infinite recursion.

### Step 6: Check spawns limit

If the thread has a parent, the orchestrator checks whether the parent has exceeded its `spawns` limit. If so, the thread returns an error. Otherwise, the parent's spawn count is incremented.

### Step 7: Build hooks and harness

Hooks are merged from five sources and sorted by layer:

| Layer | Source | Config Location | Purpose |
|-------|--------|-----------------|---------|
| 0 | User hooks | `~/.ai/config/agent/hooks.yaml` | Cross-project personal hooks |
| 1 | Directive hooks | Directive XML `<hooks>` block | Per-directive hooks |
| 2 | Builtin hooks | System `hook_conditions.yaml` | Error/limit/compaction defaults |
| 3 | Project hooks | `.ai/config/agent/hooks.yaml` | Project-wide hooks |
| 4 | Infra hooks | System `hook_conditions.yaml` | Infrastructure (emitter, checkpoint) |

User and project hooks use the same format as directive hooks — `id`, `event`, optional `condition`, and `action`. See [Hooks Configuration](#hooks-configuration) below.

The `SafetyHarness` is constructed with the resolved limits, merged hooks, directive permissions, and parent capabilities. Tool schemas are loaded from the primary tools directory and attached to the harness.

### Step 8: Reserve budget

The hierarchical budget ledger handles cost tracking:

- **Root threads:** `ledger.register(thread_id, max_spend)` — creates a top-level budget entry
- **Child threads:** `ledger.reserve(thread_id, spend_limit, parent_thread_id)` — atomically reserves budget from the parent's remaining allocation

If the parent has insufficient remaining budget, the reservation fails and the thread returns an error.

### Step 9: Build prompt and providers

The LLM prompt is built from the directive's raw content (the full markdown file minus the signature comment). The model is resolved from: `params.model` → `directive.model.id` → `directive.model.tier`. An `HttpProvider` is created with the resolved model configuration.

### Step 10: Write initial thread.json

The thread metadata file is written to `.ai/agent/threads/<thread_id>/thread.json`:

```json
{
  "thread_id": "agency-kiwi/discover_leads-1739820456",
  "directive": "agency-kiwi/discover_leads",
  "status": "running",
  "created_at": "2026-02-17T10:00:56+00:00",
  "updated_at": "2026-02-17T10:00:56+00:00",
  "model": "claude-3-5-haiku-20241022",
  "limits": {
    "turns": 10,
    "tokens": 200000,
    "spend": 0.10,
    "depth": 3,
    "spawns": 10
  },
  "capabilities": [
    "rye.execute.tool.scraping.gmaps.scrape_gmaps",
    "rye.load.knowledge.agency-kiwi.*"
  ]
}
```

The `thread.json` file is signed using canonical JSON serialization with a `_signature` field, protecting capabilities and limits from tampering.

### Step 11: Set parent env var

`RYE_PARENT_THREAD_ID` is set to this thread's ID so any child subprocesses (spawned via `async`) inherit the parent relationship.

### Step 12: Spawn or run

- **Synchronous** (default): Calls `runner.run()` directly and blocks until completion
- **Asynchronous** (`async: true`): `spawn_detached()` launches a subprocess that re-executes `thread_directive.py` with `--thread-id` and `--pre-registered` flags. The child rebuilds all state from scratch. Detached spawning uses the `rye-proc spawn` Rust binary for cross-platform support, with a POSIX `subprocess.Popen` fallback. The parent process returns immediately with `{"thread_id": "...", "status": "running"}`

### Step 13: Run LLM loop

See "The Runner's LLM Loop" below.

### Step 14: Finalize

After the LLM loop completes:

> **Note:** When `directive_return` was called during the LLM loop, the final result includes an `outputs` dict (the structured key-value pairs from the return call) alongside the raw `result` text.

1. Report actual spend to the ledger: `ledger.report_actual(thread_id, actual_spend)`
2. Cascade spend to parent: `ledger.cascade_spend(thread_id, parent_thread_id, actual_spend)`
3. Release budget reservation: `ledger.release(thread_id, final_status)`
4. Update registry status: `registry.update_status(thread_id, status)`
5. Store result in registry: `registry.set_result(thread_id, cost)`
6. Write final `thread.json` with cost and updated status

## The Runner's LLM Loop

`runner.run()` manages the core conversation loop. There is no system prompt — tools are passed via API tool definitions, and context framing is injected through hooks.

### First Message Construction

`run_hooks_context()` takes an explicit `event` parameter (required, no default) and dispatches hooks matching that event:

- **Fresh threads:** `run_hooks_context(event="thread_started")` fires `thread_started` hooks. Context includes `directive_body` and `inputs`. Hook context and the user prompt (full directive content) are concatenated into a single user message.

```python
messages = [{"role": "user", "content": f"{hook_context}\n\n{directive_prompt}"}]
```

- **Continuation threads** (when `resume_messages` is provided): `run_hooks_context(event="thread_continued")` fires `thread_continued` hooks instead. Context is injected near the last user message (not prepended). The context dict also includes `previous_thread_id` and `inputs`, available for interpolation.

### Turn Loop

Each turn follows this sequence:

1. **Check limits** — `harness.check_limits(cost)` tests turns, tokens, spend, duration. If exceeded, hooks evaluate the limit event. If no hook handles it, the thread terminates with a limit error.

2. **Check cancellation** — `harness.is_cancelled()` checks the `_cancelled` flag (set by `cancel_thread` operation). If cancelled, the thread terminates.

3. **LLM call** — If the provider supports streaming, `provider.create_streaming_completion()` is used with a `TranscriptSink` that writes `token_delta` events to the transcript JSONL and appends text to the knowledge markdown in real-time. Otherwise, `provider.create_completion()` is used. Errors trigger the error classification system and hooks. See [Per-Token Streaming](./streaming.md).

4. **Track tokens** — Input/output tokens and spend from the response are accumulated in the `cost` dict.

5. **Parse tool calls** — Native tool_use blocks are used if the provider supports them. Otherwise, `text_tool_parser.extract_tool_calls()` parses tool calls from the response text.

6. **No tool calls** — If the LLM responds with text only (no tool calls), the thread completes with the raw text as the result. For directives with `<outputs>`, the LLM should call `directive_return` via `rye_execute` instead, which provides structured key-value outputs that parent threads can consume programmatically. On the first turn with native tool_use, the runner nudges the model to use tools before accepting a text-only response.

7. **Dispatch each tool call:**
   - Resolve the tool name to an item_id via `tool_id_map`
   - Check permission via `harness.check_permission()` — denied calls return an error message to the LLM
   - If the inner `item_id` is `rye/agent/threads/directive_return`, the runner intercepts the call before dispatch. It validates that all required output fields (declared in `<outputs>`) are present. If fields are missing, an error is returned to the LLM to retry. If valid, the `directive_return` hook event fires, and the thread finalizes with structured `outputs` in the result.
   - Auto-inject parent context for child thread spawns (parent_thread_id, parent_depth, parent_limits, parent_capabilities)
   - Execute via `ToolDispatcher`
   - Guard result (bound large results, deduplicate, store artifacts)
   - Append result as a tool message

8. **Run after_step hooks** — Post-turn hooks evaluate (e.g., cost tracking, logging).

9. **Update cost snapshot** — The registry is updated with current cost data (best-effort).

10. **Check context limit** — If estimated token usage exceeds the threshold (default 0.9 of context window), trigger `handoff_thread` to continue in a new thread. The handoff no longer generates a summary — summarization is hook-driven via `after_complete` hooks declared by the directive.

### After-Complete Hook Dispatch

After the turn loop exits and `render_knowledge_transcript()` runs, the runner dispatches `after_complete` hooks in the `finally` block. This is best-effort (wrapped in `try/except`) — failures do not affect the thread's final status. This enables directives to declare hooks for post-completion actions like summarization.

## Thread Storage

Each thread creates a directory at `.ai/agent/threads/<thread_id>/` containing:

| File | Purpose |
|------|---------|
| `thread.json` | Signed thread metadata: ID, directive, status, model, cost, limits, capabilities |
| `transcript.jsonl` | Append-only event log with inline checkpoint signatures |

Thread transcripts are also exported as signed knowledge entries at `.ai/knowledge/threads/{thread_id}.md` for discoverability via `rye search knowledge`.

The thread registry (`registry.db`) and budget ledger (`budget_ledger.db`) are shared SQLite databases at `.ai/agent/threads/`.

## Thread Registry

The `ThreadRegistry` class provides these operations:

| Method | Purpose |
|--------|---------|
| `register(thread_id, directive, parent_id)` | Create thread entry with status `created` |
| `update_status(thread_id, status)` | Transition to a new state |
| `get_thread(thread_id)` | Get full thread record |
| `set_result(thread_id, result)` | Store final result (JSON serialized) |
| `update_cost_snapshot(thread_id, cost)` | Update cost columns mid-execution |
| `list_active()` | List all non-terminal threads |
| `list_children(parent_id)` | List children of a thread |
| `set_continuation(thread_id, continuation_thread_id)` | Mark thread as continued |
| `set_chain_info(thread_id, chain_root_id, continuation_of)` | Set chain metadata |
| `get_chain(thread_id)` | Get full continuation chain |

## Hooks Configuration

User and project hooks let you inject context, record learnings, or run directives on every thread — without modifying each directive individually.

### Config format

Create `.ai/config/agent/hooks.yaml` at the project level, or `~/.ai/config/agent/hooks.yaml` at the user level:

```yaml
hooks:
  - id: "inject_project_conventions"
    event: "thread_started"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/conventions"
    description: "Inject project conventions into every thread"

  - id: "inject_api_types"
    event: "thread_started"
    condition:
      path: "directive"
      op: "contains"
      value: "api"
    action:
      primary: "load"
      item_type: "knowledge"
      item_id: "project/api-types"
    description: "Inject API types for API-related directives only"

  - id: "record_learnings"
    event: "after_complete"
    condition:
      path: "cost.turns"
      op: "gte"
      value: 3
    action:
      primary: "execute"
      item_type: "directive"
      item_id: "project/record-learnings"
    description: "Record learnings after substantial threads"
```

### Available events

| Event | When it fires | Context available |
|-------|--------------|-------------------|
| `thread_started` | Before first LLM turn (fresh threads) | `directive`, `directive_body`, `model`, `limits`, `inputs` |
| `thread_continued` | Before first LLM turn (continuation threads) | `directive`, `directive_body`, `model`, `limits`, `previous_thread_id`, `inputs` |
| `after_step` | After each turn in the LLM loop | `cost`, `thread_id` |
| `after_complete` | In the `finally` block after the loop ends | `thread_id`, `cost`, `project_path` |
| `error` | When an LLM call or tool execution fails | `error`, `classification` |
| `limit` | When a limit is exceeded | `limit_code`, `current_value`, `current_max` |

### Hook actions

Actions use the same format as directive hooks:

- `primary: "load"` — Load a knowledge or directive item (for context injection)
- `primary: "execute"` — Execute a tool or directive
- `primary: "search"` — Search for items

### Conditions

Conditions use the same operators as the condition evaluator: `eq`, `ne`, `gt`, `gte`, `lt`, `lte`, `in`, `contains`, `regex`, `exists`. Combine with `any`, `all`, `not` for complex logic.

### Layer ordering

Lower layers run first. Within a layer, hooks run in definition order. A hook at any layer can use conditions to selectively fire based on context (directive name, cost, model, etc.).

## What's Next

- [Per-Token Streaming](./streaming.md) — Real-time token streaming to transcript and knowledge files
- [Spawning Children](./spawning-children.md) — How to spawn, wait, and collect results
- [Safety and Limits](./safety-and-limits.md) — How limits resolve and what happens when they're exceeded
