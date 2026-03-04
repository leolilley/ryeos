```yaml
id: tools-agent
title: "Agent System"
description: "The thread orchestration engine — managed LLM loops with budgets, safety controls, permissions, and event streaming"
category: standard-library/tools
tags: [tools, agent, threads, orchestration, llm, safety, permissions, budget]
version: "1.0.0"
```

# Agent System

**Namespace:** `rye/agent/`

The agent system is the largest subsystem in Rye OS. It provides **managed thread execution** — the ability to run directives autonomously with a full LLM loop, tool access, budget controls, permission enforcement, and event streaming.

A "thread" in Rye OS is an isolated execution context where an LLM reads a directive, calls tools, and produces a result — all within configurable limits.

---

## Architecture Overview

```
thread_directive (entry point)
  │
  ├── Loads directive → extracts model, limits, permissions, hooks
  ├── Resolves parent context (depth, capabilities, budget)
  ├── Creates SafetyHarness (limits, permissions, hooks, dynamic tool registration)
  ├── Resolves LLM provider (Anthropic/OpenAI via YAML config)
  ├── Reserves budget in hierarchical ledger
  │
  └── runner.run() — the LLM loop
        │
        ├── Build first message (hook context + directive prompt)
        │
        └── Loop:
              ├── Check limits (turns, tokens, spend, duration)
              ├── Check cancellation
              ├── Call LLM via provider adapter
              ├── Parse tool calls (native API or text-parsed)
              ├── Permission check each tool call
              ├── Dispatch tool calls via ToolDispatcher
              ├── Guard large results (truncate, dedupe, artifact store)
              ├── Run after_step hooks
              ├── Check context window limit → handoff if needed
              └── Repeat until: no tool calls (completion) | limit hit | error
```

---

## Entry Points

These are the two tools you call to use the agent system.

### `thread_directive`

**Item ID:** `rye/agent/threads/thread_directive`

Execute a directive in a managed thread with a full LLM loop. This is the primary entry point for autonomous directive execution.

#### Parameters

| Name              | Type    | Required | Default | Description                                                                        |
| ----------------- | ------- | -------- | ------- | ---------------------------------------------------------------------------------- |
| `directive_name`  | string  | ✅       | —       | Directive item_id to execute                                                       |
| `async`      | boolean | ❌       | `false` | Return immediately with thread_id (fork to background)                             |
| `inputs`          | object  | ❌       | `{}`    | Input parameters for the directive                                                 |
| `model`           | string  | ❌       | —       | Override the directive's model selection                                           |
| `limit_overrides` | object  | ❌       | —       | Override limits: `turns`, `tokens`, `spend`, `spawns`, `duration_seconds`, `depth` |

#### Execution Flow

1. **Generate thread ID** — `<directive_name>-<epoch_timestamp>`
2. **Resolve parent context** — checks `parent_thread_id` param, then `RYE_PARENT_THREAD_ID` env var
3. **Register thread** in the thread registry
4. **Load directive** — resolves across spaces (project → user → system), parses metadata
5. **Resolve limits** — merges: defaults (from `resilience.yaml`) → directive limits → param overrides → parent upper bounds (via `min()`)
6. **Check depth** — if `depth <= 0`, the thread cannot spawn (prevents unbounded recursion)
7. **Check parent spawn limit** — parent tracks how many children it has spawned
8. **Build safety harness** — with limits, hooks, permissions, capability tokens
9. **Reserve budget** — in the hierarchical budget ledger
10. **Resolve LLM provider** — maps model name/tier to provider config (Anthropic, OpenAI)
11. **Run** — either synchronously or fork to background (async)
12. **Finalize** — report spend, cascade to parent budget, update registry, write `thread.json`

#### Synchronous vs Async

- **Sync** (default): blocks until the thread completes, returns the full result
- **Async** (`async: true`): forks a child process via `os.fork()`, returns immediately with `thread_id` and `pid`. The child process daemonizes (`os.setsid()`) and runs to completion independently.

#### Output

```json
{
  "success": true,
  "thread_id": "my-directive-1708300000",
  "status": "completed",
  "directive": "my-directive",
  "result": "The directive's final output text...",
  "cost": {
    "turns": 5,
    "input_tokens": 12000,
    "output_tokens": 3000,
    "spend": 0.08
  }
}
```

#### Example

```python
# Run a directive synchronously
rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive",
    parameters={"directive_name": "my-workflow", "inputs": {"target": "staging"}})

# Run asynchronously (returns immediately)
rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive",
    parameters={"directive_name": "long-running-task", "async": true})

# With model and limit overrides
rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "complex-analysis",
        "model": "claude-sonnet-4-20250514",
        "limit_overrides": {"turns": 20, "spend": 0.50}
    })
```

---

### `orchestrator`

**Item ID:** `rye/agent/threads/orchestrator`

Thread coordination: wait for threads, cancel them, check status, read transcripts, resume stopped threads, and navigate continuation chains.

#### Operations

| Operation           | Description                                                  |
| ------------------- | ------------------------------------------------------------ |
| `wait_threads`      | Wait for one or more threads to complete                     |
| `cancel_thread`     | Request graceful cancellation (sets flag, checked next turn) |
| `kill_thread`       | Force-kill a thread's process via SIGTERM/SIGKILL            |
| `get_status`        | Check a thread's current status                              |
| `list_active`       | List all currently running threads (in-process only)         |
| `aggregate_results` | Collect results from multiple threads                        |
| `get_chain`         | Get the continuation chain for a thread                      |
| `chain_search`      | Search across a thread's continuation chain transcripts      |
| `read_transcript`   | Read a thread's transcript (full or tail)                    |
| `resume_thread`     | Resume a stopped thread with a new user message              |
| `handoff_thread`    | Hand off a stopping thread to a new continuation thread      |

#### `wait_threads`

Waits for multiple threads concurrently. Resolves continuation chains (if thread A was continued as thread B, waits for B). Supports cross-process polling via registry.

```python
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "wait_threads",
        "thread_ids": ["task-a-170830", "task-b-170831"],
        "timeout": 300
    })
```

Default timeout is loaded from `coordination.yaml` (`wait_threads.default_timeout`, default 600s).

#### `cancel_thread` vs `kill_thread`

- **cancel** — sets a flag checked at the start of each turn. Graceful: the thread finishes its current turn and exits.
- **kill** — sends `SIGTERM` to the process, waits 3 seconds, then `SIGKILL`. For async threads that need to be force-stopped.

#### `resume_thread`

Resumes a completed/errored thread by:

1. Reconstructing the full conversation from the transcript
2. Appending the new user message
3. Spawning a new thread with the same directive
4. Linking old → new via the continuation chain

```python
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "resume_thread",
        "thread_id": "my-task-170830",
        "message": "Continue from where you left off, but also handle edge case X"
    })
```

#### `handoff_thread`

Automatic continuation when a thread's context window fills up:

1. Builds trailing messages within a token ceiling
2. Spawns a new thread with the same directive and `previous_thread_id`
3. Links old → new in the continuation chain

Summarization is hook-driven — if the directive declares an `after_complete` hook, it runs before the handoff. The new thread fires `thread_continued` hooks (not `thread_started`), enabling context re-injection.

This is usually triggered automatically by the runner when context usage exceeds the threshold (default 90%), but can be called manually.

#### `read_transcript`

```python
# Full transcript
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "read_transcript", "thread_id": "my-task-170830"})

# Last 50 lines
rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "read_transcript", "thread_id": "my-task-170830", "tail_lines": 50})
```

---

## The LLM Loop (`runner`)

**Item ID:** `rye/agent/threads/runner`

The runner is the core execution loop. It is not called directly — `thread_directive` invokes it.

### How It Works

**First message construction:**

- **Fresh threads:** `run_hooks_context(event="thread_started")` dispatches hooks that load knowledge items (identity, rules, context). Hook context + directive prompt are assembled into a single user message. The hook context includes `directive_body` and `inputs` for interpolation.
- **Continuation threads** (resume_messages provided): `run_hooks_context(event="thread_continued")` fires instead. Context is injected near the last user message. The hook context includes `previous_thread_id` and `inputs`, enabling `${inputs.*}` interpolation in hook actions.
- No system prompt is used — everything goes through user messages and tool definitions

**Each turn:**

1. **Pre-turn limit check** — turns, tokens, spend, duration
2. **Cancellation check**
3. **LLM call** via provider adapter
4. **Token tracking** — input/output tokens and spend are accumulated
5. **Tool call parsing** — native API `tool_use` blocks, or text-parsed fallback for models without native tool use
6. **First-turn nudge** — if the model responds without tool calls on turn 1, it gets a reminder to use tools
7. **Permission check** — each tool call is checked against the directive's capability strings
8. **Tool dispatch** — calls routed through `tool_primary_map` → `ToolDispatcher` → `rye_execute`
9. **Result guarding** — large results are truncated, deduped, or stored as artifacts
10. **Post-turn hooks** — `after_step` hooks run
11. **Context limit check** — if context usage exceeds threshold, triggers automatic handoff (no summary — summarization is hook-driven)
12. **Loop or exit** — if no tool calls in the response, the thread completes with the LLM's text as the result

After the loop exits, `after_complete` hooks fire in the `finally` block (best-effort). This enables directives to run post-completion actions like summarization.

### Tool Call Flow

```
LLM response contains tool_use blocks
  │
  ├── Text-parsed mode: extract tool calls from plain text (models without native tool_use)
  │
  ├── For each tool call:
  │     ├── Map tool name to dispatch route via tool_primary_map
  │     ├── Check permission against directive capabilities
  │     ├── Auto-inject parent context for child thread spawns
  │     ├── Dispatch via ToolDispatcher
  │     ├── Clean result (strip envelope, signatures, metadata bloat)
  │     ├── Guard result (truncate large outputs, store artifacts)
  │     └── Append tool result to conversation
  │
  └── Continue loop
```

### Context Limit & Automatic Handoff

The runner monitors context window usage each turn. When usage exceeds the threshold (configurable in `coordination.yaml`, default 90%):

1. Emits `context_limit_reached` event
2. Calls `orchestrator.handoff_thread()` which:
   - Summarizes the current thread
   - Spawns a new continuation thread
   - Links them via the continuation chain
3. The current thread exits with status `continued`

Token estimation uses a rough `chars / 4` heuristic.

---

## Safety Harness

**Item ID:** `rye/agent/threads/safety_harness`

The `SafetyHarness` class manages thread safety. It does NOT execute anything — it checks limits, evaluates hook conditions, and enforces permissions.

### Limit Checking

Checked at the start of every turn:

| Limit              | Tracks                         | Default Source    |
| ------------------ | ------------------------------ | ----------------- |
| `turns`            | Number of LLM calls            | `resilience.yaml` |
| `tokens`           | `input_tokens + output_tokens` | `resilience.yaml` |
| `spend`            | Cumulative dollar spend        | `resilience.yaml` |
| `duration_seconds` | Wall-clock time since start    | `resilience.yaml` |

Limit resolution: `defaults (resilience.yaml) → directive limits → param overrides → parent caps (min())`.

### Permission System

Permissions use **capability strings** with `fnmatch` wildcard matching:

```
rye.<primary>.<item_type>.<item_id_dotted>
```

| Capability                           | Matches                           |
| ------------------------------------ | --------------------------------- |
| `rye.execute.tool.rye.file-system.*` | Any tool under `rye/file-system/` |
| `rye.search.directive.*`             | Search any directive              |
| `rye.execute.tool.rye.bash`          | Only the bash tool                |

**Rules:**

- If no capabilities are declared → **all actions denied** (fail-closed)
- Internal thread tools (`rye/agent/threads/internal/*`) are **always allowed**
- Child threads inherit parent capabilities unless they declare their own
- Item IDs use `/` separators, capabilities use `.` separators

### Hook System

Five layers of hooks, merged and sorted by layer:

| Layer | Source | Config Location | Purpose |
|-------|--------|-----------------|---------|
| 0 | User hooks | `~/.ai/config/agent/hooks.yaml` | Cross-project personal hooks |
| 1 | Directive hooks | Directive XML `<hooks>` block | Per-directive hooks |
| 2 | Builtin hooks | System `hook_conditions.yaml` | Error/limit/compaction defaults |
| 3 | Project hooks | `.ai/config/agent/hooks.yaml` | Project-wide hooks |
| 4 | Infra hooks | System `hook_conditions.yaml` | Infrastructure (emitter, checkpoint) |

**Two dispatch modes:**

- `run_hooks()` — for `error`, `limit`, `after_step` events. Returns a control action (retry, terminate, continue) or None.
- `run_hooks_context(event)` — for `thread_started` and `thread_continued` events. Loads knowledge items and returns concatenated context string. All matching hooks run (no short-circuit). The `event` parameter is required.

**Hook condition evaluation** uses variables like `cost.current`, `loop_count`, `error.type`, etc., evaluated by `condition_evaluator.py`.

**Hook action interpolation** supports `${variable}` substitution via `interpolation.py`. Interpolation resolves `${...}` in both `item_id` and `params` fields, enabling patterns like `item_id: "agent/threads/${inputs.dependency_thread_id}"`.

---

## Adapters

### Provider Adapter (`provider_adapter`)

Base interface for LLM provider integration. Defines the contract:

- `create_completion(messages, tools)` → response with text, tool_calls, token counts, spend

### HTTP Provider (`http_provider`)

HTTP-based LLM provider supporting Anthropic and OpenAI APIs. Handles:

- Tool definition remapping (generic schema → provider-specific format)
- Streaming response parsing
- Token counting and spend calculation
- `tool_use` mode: `native` (API tool blocks) or `text_parsed` (parse from text)

### Provider Resolver (`provider_resolver`)

Resolves a model name or tier string to a concrete provider configuration:

1. Checks for exact model ID match in provider YAML configs
2. Checks for tier match (e.g., `fast` → `claude-3-5-haiku-*`, `general` → `claude-sonnet-4-*`)
3. Returns: `(resolved_model_name, provider_item_id, provider_config)`

### Tool Dispatcher (`tool_dispatcher`)

Routes tool calls from the LLM to `rye_execute`. The runner uses a `tool_primary_map` (built from `harness.available_tools`) to resolve flattened tool names to their item IDs and `_primary` action for dispatch.

---

## Persistence

All thread state is persisted to disk under `.ai/agent/threads/<thread_id>/`.

### Thread Registry (`persistence/thread_registry`)

Tracks all threads in `.ai/agent/threads/registry.json`:

- Registration (thread_id, directive, parent_id, timestamp)
- Status updates (created → running → completed/error/cancelled/continued)
- Continuation chain links (old_thread → new_thread)
- Cost snapshots (updated each turn)
- Spawn tracking (which threads spawned which)

### Transcript (`persistence/transcript`)

Records the full conversation to `.ai/agent/threads/<thread_id>/transcript.jsonl`:

- All LLM messages (user, assistant, tool results)
- Event markers (thread_started, thread_completed, etc.)
- Supports reconstruction of messages for resume/handoff

Also writes `capabilities.md` — a signed markdown snapshot of the thread's tool definitions (JSON fenced block) and capabilities tree (code fence). Written once before the LLM loop starts. Referenced from the knowledge transcript via `capabilities_ref`.

### State Store (`persistence/state_store`)

Persists arbitrary thread state between turns. Used by hooks and internal components.

### Artifact Store (`persistence/artifact_store`)

Stores large tool results outside the conversation context. When a tool result exceeds the size threshold, it's stored as an artifact and replaced with a reference in the conversation.

### Budget Ledger (`persistence/budgets`)

Hierarchical budget tracking in `.ai/agent/threads/budget_ledger.json`:

- **Register** — create a new budget entry with max spend
- **Reserve** — child threads reserve budget from parent
- **Report actual** — record actual spend after completion
- **Cascade** — propagate child spend up to parent
- **Release** — finalize budget entry on completion

---

## Events

### Event Emitter (`events/event_emitter`)

Emits structured lifecycle events with criticality levels:

| Event                   | Criticality | When                                   |
| ----------------------- | ----------- | -------------------------------------- |
| `cognition_in`          | normal      | Before LLM call (user/tool message)    |
| `cognition_out`         | normal      | After LLM response                     |
| `tool_call_result`      | normal      | After tool execution                   |
| `thread_started`        | critical    | Thread begins (triggers context hooks) |
| `thread_completed`      | critical    | Thread finishes successfully           |
| `thread_error`          | critical    | Thread fails                           |
| `thread_cancelled`      | critical    | Thread was cancelled                   |
| `thread_resumed`        | critical    | Thread was resumed via continuation    |
| `context_limit_reached` | critical    | Context window approaching capacity    |
| `limit`                 | normal      | A resource limit was hit               |

Events are written to the transcript and can trigger hooks.

### Streaming Tool Parser (`events/streaming_tool_parser`)

Parses streaming responses from LLM providers that emit tool calls incrementally.

---

## Internal Components

Low-level components inside `rye/agent/threads/internal/`:

| Component             | Purpose                                              |
| --------------------- | ---------------------------------------------------- |
| `budget_ops`          | Budget arithmetic (reserve, spend, cascade)          |
| `cancel_checker`      | Check cancellation flag                              |
| `classifier`          | Classify thread output for status determination      |
| `control`             | Control flow actions returned from hooks             |
| `cost_tracker`        | Track token and spend costs                          |
| `emitter`             | Internal event emission helpers                      |
| `limit_checker`       | Check resource limits against current cost           |
| `state_persister`     | Persist thread state between turns                   |
| `text_tool_parser`    | Parse tool calls from plain text (non-native models) |
| `thread_chain_search` | Search across continuation chain transcripts         |
| `tool_result_guard`   | Bound large results, dedupe, store artifacts         |

---

## Configuration (YAML)

Declarative configs in `rye/agent/threads/config/`:

| Config                      | Purpose                                                                        |
| --------------------------- | ------------------------------------------------------------------------------ |
| `events.yaml`               | Event definitions and criticality levels                                       |
| `error_classification.yaml` | Error types, categories, and retry policies                                    |
| `hook_conditions.yaml`      | Built-in hook condition definitions                                            |
| `coordination.yaml`         | Wait timeouts, continuation trigger threshold, resume token ceiling            |
| `resilience.yaml`           | Default limits (turns, tokens, spend, duration, depth, spawns), retry policies |
| `budget_ledger_schema.yaml` | JSON schema for the budget ledger file                                         |

---

## LLM Providers

YAML configs in `rye/agent/providers/`:

| Config           | Provider           | Details                                                    |
| ---------------- | ------------------ | ---------------------------------------------------------- |
| `anthropic.yaml` | Anthropic (Claude) | Model tiers, endpoints, context windows, `tool_use` format |
| `openai.yaml`    | OpenAI (GPT)       | Model tiers, endpoints, context windows                    |

Provider configs define:

- Model name → tier mapping (fast, general, orchestrator, reasoning)
- API endpoint and authentication
- Context window sizes
- Tool use format (native vs text_parsed)
- Token pricing for spend calculation

---

## Capability System

Controls in `rye/agent/permissions/`:

### Capability Tokens (`capability_tokens.py`)

Creates and validates capability tokens — scoped permission grants that threads carry.

### Capability YAML Files

Define what each capability string allows:

| File            | Domain                          |
| --------------- | ------------------------------- |
| `primary.yaml`  | Core primary tool capabilities  |
| `agent.yaml`    | Agent/thread tool capabilities  |
| `fs.yaml`       | File system capabilities        |
| `db.yaml`       | Database capabilities           |
| `git.yaml`      | Git capabilities                |
| `mcp.yaml`      | MCP client capabilities         |
| `net.yaml`      | Network capabilities            |
| `process.yaml`  | Process/subprocess capabilities |
| `registry.yaml` | Registry capabilities           |

---

## Thread Lifecycle

```
1. CREATED     → thread_directive called, thread registered
2. RUNNING     → runner.run() executing the LLM loop
3. COMPLETED   → LLM responded without tool calls (task done)
   ERROR       → limit hit, permission denied, LLM error, or exception
   CANCELLED   → cancel requested and processed
   CONTINUED   → context limit reached, handed off to new thread
   KILLED      → force-killed via orchestrator.kill_thread
```

### Continuation Chains

When a thread runs out of context window space:

```
Thread A (turns 1-15) → status: continued
  └── Thread B (turns 16-30) → status: continued
        └── Thread C (turns 31-40) → status: completed
```

The orchestrator resolves chains: `get_chain(A)` returns `[A, B, C]`. `wait_threads(A)` automatically follows the chain and waits for C.

### Parent-Child Relationships

Threads can spawn child threads (via `rye_execute` calling `thread_directive`):

```
Root Thread (depth=3)
  ├── Child A (depth=2, inherits parent caps)
  │     └── Grandchild (depth=1)
  └── Child B (depth=2)
```

- **Depth** decrements by 1 per level. At `depth=0`, no more children can be spawned.
- **Spawn limit** caps how many children a single parent can create.
- **Capabilities** inherit from parent unless the child directive declares its own.
- **Budget** cascades: child spend is propagated up to parent.
- **Limits** are capped: `min(child_limit, parent_limit)` for each dimension.
