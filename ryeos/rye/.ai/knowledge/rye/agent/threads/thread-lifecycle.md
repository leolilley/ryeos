<!-- rye:signed:2026-02-21T05:56:40Z:53aeeee0e64abb9c3f69c641120d74110e97c363db99feb9265295ba510b7529:9HOBa3SnGM4j_9YfdUD6Qz6YKiW-N_1-7uqAN1oItbsTtm1EixC7UJNqqbJZJBRo98WFsQcburEClulU6-QnBQ==:9fbfabe975fa5a7f -->

```yaml
id: thread-lifecycle
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

Hooks merged from three sources, sorted by layer:
- **Layer 1** — Directive hooks (from XML)
- **Layer 2** — Builtin hooks (project `.ai/config/`)
- **Layer 3** — Infra hooks (system-level, always run)

`SafetyHarness` constructed with resolved limits, merged hooks, directive permissions, parent capabilities. Tool schemas attached.

### Step 8: Reserve budget

- **Root threads:** `ledger.register(thread_id, max_spend)`
- **Child threads:** `ledger.reserve(thread_id, spend_limit, parent_thread_id)` — atomic reservation from parent's remaining allocation

Insufficient parent budget → error, thread never starts.

### Step 9: Build prompt and providers

Prompt built from directive raw content (markdown minus signature). Model resolved from `params.model` → `directive.model.id` → `directive.model.tier`. `HttpProvider` created with resolved config.

### Step 10: Write initial thread.json

Written to `.ai/threads/<thread_id>/thread.json`:

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

`RYE_PARENT_THREAD_ID` set to this thread's ID so child processes inherit the parent relationship.

### Step 12: Fork or run

- **Synchronous** (default): `runner.run()` blocks until completion
- **Asynchronous** (`async_exec: true`): `os.fork()` → child detaches via `os.setsid()`, redirects stdio to `/dev/null`, runs loop, finalizes, calls `os._exit(0)`. Parent returns immediately with `{"thread_id": "...", "status": "running"}`

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

1. `run_hooks_context()` dispatches `thread_started` hooks → each loads a knowledge item (agent identity, rules)
2. Hook context + user prompt (directive content) concatenated into single user message:

```python
messages = [{"role": "user", "content": f"{hook_context}\n\n{directive_prompt}"}]
```

For resumed threads: pre-built `resume_messages` used instead.

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
8. **Run `after_step` hooks** — Post-turn hooks evaluate
9. **Update cost snapshot** — Registry updated with current cost (best-effort)
10. **Check context limit** — If token usage > threshold (default 0.9 of context window) → trigger `handoff_thread`

## Thread Storage

Each thread creates `.ai/threads/<thread_id>/`:

| File              | Purpose                                  |
|-------------------|------------------------------------------|
| `thread.json`     | Metadata: ID, directive, status, model, cost, limits, capabilities |
| `transcript.md`   | Full conversation log (EventEmitter)     |

Shared databases at `.ai/threads/`:
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
