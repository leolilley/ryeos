<!-- rye:signed:2026-02-23T07:58:34Z:807f1fc62c27ff2fd688fe82f53360bf248a098f22c514324e4acf0ee7a0843d:lNuA11hJFBS4c4y2zdA442OO5veI3jesyBFzCBpowqjwgOe2DdIIoOWjXq0QHJIEQU3S8OpKwRFm07RZWws4Aw==:9fbfabe975fa5a7f -->

```yaml
name: thread-lifecycle
title: Thread Lifecycle
entry_type: reference
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - threads
  - lifecycle
  - orchestration
references:
  - limits-and-safety
  - prompt-rendering
  - spawning-patterns
  - "docs/orchestration/thread-lifecycle.md"
```

# Thread Lifecycle

Deterministic lifecycle: generate ID → register → load directive → resolve limits → run LLM loop → finalize.

## Thread States

```
created ──→ running ──→ completed
                    ├──→ error
                    ├──→ cancelled
                    └──→ continued
```

| State       | Meaning                                          |
|-------------|--------------------------------------------------|
| `created`   | Registered in registry, not yet executing        |
| `running`   | LLM loop is active                               |
| `completed` | Finished successfully — result available          |
| `error`     | Failed — error message in result                 |
| `cancelled` | Cancelled via `cancel_thread` operation           |
| `continued` | Handed off to a new thread (context limit reached)|

## Thread ID Format

```python
thread_id = f"{directive_name}-{int(time.time())}"
# Example: "agency-kiwi/discover_leads-1739820456"
```

Human-readable (shows directive) and unique (epoch seconds).

## Execution Steps (in order)

### Step 1: Resolve parent context

First match wins:
1. Explicit `parent_thread_id` parameter (handoff/resume)
2. `RYE_PARENT_THREAD_ID` environment variable (set by parent)
3. No parent → root thread

If parent declared but `thread.json` missing → **immediate failure**.

### Step 2: Register thread

Insert into SQLite registry (`registry.db`) with status `created`, directive name, parent ID.

### Step 3: Load directive

`DirectiveResolver` searches project → user → system spaces. The markdown_xml parser extracts metadata (limits, permissions, model, inputs) and preserves raw content for the LLM prompt.

- **Normal execution:** `ExecuteTool` handles input validation and interpolation
- **Resume/handoff:** `LoadTool` used instead (no input validation)

### Step 3.5: Reconstruct resume messages (continuation only)

If `previous_thread_id` is set and no `resume_messages` provided, the thread is a continuation:

1. **Verify transcript integrity** — signed checkpoint verification (strict or lenient per config)
2. **Reconstruct messages** — read previous thread's `transcript.jsonl`, rebuild trailing messages within `resume_ceiling_tokens` budget, trim to start with a `user` message
3. **Resolve continuation directive** — `directive.get("continuation_directive")` or default `rye/agent/continuation`. The directive is loaded and interpolated with `original_directive`, `original_directive_body`, `previous_thread_id`, and `continuation_message`. Its rendered body becomes the trailing user message in `resume_messages`
4. **Fallback** — if the continuation directive fails to load, the raw `continuation_message` string is used directly

### Step 4: Resolve limits

```
defaults (resilience.yaml) → directive metadata → limit_overrides → parent upper bounds
```

Parent limits **cap** all values via `min()`. A child can never exceed its parent. Depth decrements by 1 per level.

### Step 5: Check depth

If resolved depth < 0 → return error immediately. Prevents infinite recursion.

### Step 6: Check spawns limit

If thread has a parent, check parent's `spawns` limit. If exceeded → error. Otherwise increment parent's spawn count.

### Step 7: Build hooks and harness

Hooks merged from five sources, sorted by layer:

| Layer | Source | Location |
|-------|--------|----------|
| 0 | User hooks | `~/.ai/config/agent/hooks.yaml` |
| 1 | Directive hooks | Directive XML `<hooks>` block |
| 2 | Builtin hooks | System `hook_conditions.yaml` |
| 3 | Project hooks | `.ai/config/agent/hooks.yaml` |
| 4 | Infra hooks | System `hook_conditions.yaml` |

User/project hooks use same format as directive hooks: `id`, `event`, optional `condition`, `action`. User hooks are cross-project personal preferences. Project hooks are project-wide context injection and learning.

`SafetyHarness` constructed with resolved limits, merged hooks, directive permissions, parent capabilities. Tool schemas attached.

### Step 8: Reserve budget

- **Root threads:** `ledger.register(thread_id, max_spend)`
- **Child threads:** `ledger.reserve(thread_id, spend_limit, parent_thread_id)` — atomic reservation from parent's remaining allocation

Insufficient parent budget → error, thread never starts.

### Step 9: Build prompt and providers

Prompt built from directive raw content (markdown minus signature). Model resolved from `params.model` → `directive.model.id` → `directive.model.tier`. `HttpProvider` created with resolved config.

### Step 10: Write initial thread.json

Written to `.ai/agent/threads/<thread_id>/thread.json`:

```json
{
  "thread_id": "agency-kiwi/discover_leads-1739820456",
  "directive": "agency-kiwi/discover_leads",
  "status": "running",
  "model": "claude-3-5-haiku-20241022",
  "limits": {"turns": 10, "tokens": 200000, "spend": 0.10, "depth": 3, "spawns": 10},
  "capabilities": ["rye.execute.tool.scraping.gmaps.scrape_gmaps"]
}
```

### Step 11: Set parent env var

`RYE_PARENT_THREAD_ID` set to this thread's ID so spawned child processes inherit the parent relationship.

### Step 12: Spawn or run

- **Synchronous** (default): `runner.run()` blocks until completion
- **Asynchronous** (`async: true`): `spawn_detached()` launches a child subprocess via `lillux-proc spawn` (hard dependency, no fallbacks). Child runs `__main__` with `--thread-id` and `--pre-registered` flags. Parent returns immediately with `{"thread_id": "...", "status": "running"}`

### Step 13: Run LLM loop

See "Runner's LLM Loop" below.

### Step 14: Finalize

1. Report actual spend: `ledger.report_actual(thread_id, actual_spend)`
2. Cascade spend to parent: `ledger.cascade_spend(thread_id, parent_thread_id, actual_spend)`
3. Release budget: `ledger.release(thread_id, final_status)`
4. Update registry status: `registry.update_status(thread_id, status)`
5. Store result: `registry.set_result(thread_id, cost)`
6. Write final `thread.json` with cost and updated status

## Runner's LLM Loop

No system prompt. Tools via API tool definitions. Context framing via hooks.

### First Message Construction

`run_hooks_context()` takes a required `event` parameter (no default) and dispatches the matching hooks:

- **Fresh threads:** `thread_started` hooks fire. `directive_body` and `inputs` available in hook context. Hook context + user prompt concatenated into single user message:

```python
messages = [{"role": "user", "content": f"{hook_context}\n\n{directive_prompt}"}]
```

- **Continuation threads:** `thread_continued` hooks fire, context injected near last user message. `previous_thread_id` and `inputs` available in hook context.

### Turn Loop

Each turn:

1. **Check limits** — `harness.check_limits(cost)` tests turns, tokens, spend, duration. Exceeded → hooks evaluate → if unhandled, terminate with limit error
2. **Check cancellation** — `harness.is_cancelled()` checks `_cancelled` flag
3. **LLM call** — `provider.create_completion(messages, tools)`. Errors → error classification + hooks
4. **Track tokens** — Input/output tokens and spend accumulated in `cost` dict
5. **Parse tool calls** — Native `tool_use` blocks or `text_tool_parser.extract_tool_calls()`
6. **No tool calls** — Text-only response → thread completes with text as result. First turn with native `tool_use`: nudge model before accepting text-only
7. **Dispatch each tool call:**
   - Resolve tool name to item_id via `tool_id_map`
   - `harness.check_permission()` — denied → error message to LLM
   - Auto-inject parent context for child spawns
   - Execute via `ToolDispatcher`
   - Guard result (bound large results, deduplicate, store artifacts)
   - Append tool message
8. **Run `after_step` hooks** — Post-turn hooks evaluate. `after_complete` hooks fire in the `finally` block after the loop ends (best-effort, won't break finalization).
9. **Update cost snapshot** — Registry updated with current cost (best-effort)
10. **Check context limit** — If token usage > threshold (default 0.9 of context window) → trigger `handoff_thread`. Handoff no longer generates a summary — summarization is hook-driven.

## Thread Storage

Each thread creates `.ai/agent/threads/<thread_id>/`:

| File              | Purpose                                  |
|-------------------|------------------------------------------|
| `thread.json`     | Metadata: ID, directive, status, model, cost, limits, capabilities |
| `transcript.md`   | Full conversation log (EventEmitter)     |

Shared databases at `.ai/agent/threads/`:
- `registry.db` — thread registry (SQLite)
- `budget_ledger.db` — hierarchical budget tracking (SQLite)

## Thread Registry Operations

| Method                   | Purpose                                |
|--------------------------|----------------------------------------|
| `register()`             | Create entry with status `created`     |
| `update_status()`        | Transition to new state                |
| `get_thread()`           | Get full thread record                 |
| `set_result()`           | Store final result (JSON)              |
| `update_cost_snapshot()` | Update cost mid-execution              |
| `list_active()`          | List non-terminal threads              |
| `list_children()`        | List children of a thread              |
| `set_continuation()`     | Mark as continued                      |
| `set_chain_info()`       | Set chain metadata                     |
| `get_chain()`            | Get full continuation chain            |
