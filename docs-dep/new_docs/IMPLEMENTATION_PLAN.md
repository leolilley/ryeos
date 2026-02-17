# Implementation Plan: Thread Orchestration, Resilience, and App Bundling

Full implementation plan covering all three design docs in dependency order.

**Source docs:**

- [thread-orchestration-internals.md](concepts/thread-orchestration-internals.md) — 5 gaps (parallel dispatch, managed subprocess, push-based coordination, token propagation, hierarchical budgets)
- [thread-resilience-and-recovery.md](concepts/thread-resilience-and-recovery.md) — checkpointing, retry, escalation, cancellation, orphan recovery
- [app-bundling-and-orchestration.md](concepts/app-bundling-and-orchestration.md) — verified loader, bundle manifests, node runtime, app tools

**Run tests:**

```bash
source .venv/bin/activate && python -m pytest tests/rye_tests/ -v
```

**Test style:** Contract tests — verify data structures, file formats, and protocol logic using `tmp_path`, `json`, `asyncio`. Do NOT import heavy implementation modules. Use `importlib.util` for direct module loading when needed. See `tests/rye_tests/test_agent_threads_future.py` for the canonical pattern.

**Source root for threads:** `rye/rye/.ai/tools/rye/agent/threads/`

---

## Pre-Phase: Legacy Cleanup (No Backwards Compat)

Clean upgrade. No aliases, no fallbacks, no shims. Do this BEFORE any feature work.

### Cleanup 1: Kill `paused` status — replace with `suspended` everywhere

`paused` is used in `conversation_mode.py` (lines 56, 92, 258, 288), `AGENT_THREADS_IMPLEMENTATION.md` (lines 108, 121), and tests (`test_agent_threads_future.py` lines 64, 70, 79, 92, 97). The new vocabulary uses `suspended` with a `suspend_reason` in `state.json`.

**Files to modify:**

- `rye/rye/.ai/tools/rye/agent/threads/conversation_mode.py` — replace all `"paused"` → `"suspended"`, add `suspend_reason: "approval"` to state.json writes (user is awaiting input — this maps to the `approval` suspend reason)
- `rye/rye/.ai/tools/rye/agent/threads/AGENT_THREADS_IMPLEMENTATION.md` — replace `"paused"` → `"suspended"`
- `tests/rye_tests/test_agent_threads_future.py` — replace `"paused"` → `"suspended"` in all test data

**No alias. No fallback. `paused` ceases to exist.**

### Cleanup 2: Kill legacy hook event names `on_error` and `on_limit`

`safety_harness.py` line 122 defines `CHECKPOINTS = ["before_step", "after_step", "on_error", "on_limit"]`. The canonical vocabulary uses `error` and `limit` (no `on_` prefix). The doc explicitly marks `on_error` and `on_limit` as "legacy alias."

**Files to modify:**

- `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py` — rename `on_error` → `error`, `on_limit` → `limit` in `CHECKPOINTS` and all references
- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — update any hook event string literals that use `on_error`/`on_limit`
- Any directive XML that references `on_error`/`on_limit` → update to `error`/`limit`

**No alias. Old names are dead.**

### Cleanup 3: Remove out-of-process polling fallback from `wait_threads`

The docs describe a "Strategy 2: poll registry for out-of-process threads" fallback in `wait_threads`. All thread spawning uses in-process `asyncio.Task`. Cross-process dispatch is not a current use case.

**Design decision:** `wait_threads` requires in-process tasks only. If a `thread_id` has no active `asyncio.Task` and no completion event, `wait_threads` returns an error for that thread (`{"status": "error", "error": "No active task for thread_id"}`). No polling loop. No `asyncio.sleep`. No `poll_interval`.

This also removes the `poll_ids` partitioning logic, the `terminal_states` polling set, and the `while True` + `asyncio.sleep(poll_interval)` loop from the implementation.

### Cleanup 4: Remove magic default child budget `0.5`

`thread-orchestration-internals.md` line 990: `child_max = child_limits.get("spend", 0.5)`. A silent default budget is a foot-gun — if a directive doesn't declare limits, it silently gets $0.50 which may be too much or too little.

**Design decision:** Child directives MUST declare `max_spend` in their `<limits>` metadata. If missing, `spawn_thread` returns an error: `"Child directive '{name}' missing max_spend in limits. Declare <limits max_spend=\"...\"> in directive metadata."` No magic defaults.

### Cleanup 5: Remove `fallback` from node runtime ENV_CONFIG

`app-bundling-and-orchestration.md` defines `"fallback": "node"` and `"fallback": "npx"` in ENV_CONFIG. Fallback binary resolution is implicit behavior — if the configured interpreter isn't found, silently using a different one is dangerous (wrong version, wrong env).

**Design decision:** `ENV_CONFIG.interpreter` requires `var` to be set and resolvable. If `RYE_NODE` is not set and the interpreter binary isn't found on PATH, fail with a clear error: `"Node interpreter not found. Set RYE_NODE or ensure 'node' is on PATH."` No silent fallback to a different binary.

### Cleanup 6: Remove `checkpoint_on_error()` legacy path

`safety_harness.py` line 671 defines `checkpoint_on_error()`, and `thread_directive.py` line 1269 calls it only for LLM failures. The resilience doc replaces this with the unified `classify_error()` → `evaluate_hooks()` → default behavior cascade.

**Design decision:** Remove `checkpoint_on_error()` entirely. All error handling goes through the new cascade: `classify_error()` → `evaluate_hooks(event)` → default behavior. One path, no legacy detour.

### Cleanup 7: `increment_spawn_count()` — currently never called

`safety_harness.py` has `increment_spawn_count()` but it's never called anywhere. The docs note this.

**Design decision:** Wire it in during A4/A5 (token propagation + budget). Remove the dead method body until then — or mark it with a `raise NotImplementedError("Wired in Phase A5")` so it's obviously incomplete rather than silently unused.

---

## Phase A: Thread Orchestration Internals (Doc 1)

Must complete before Phase B. Provides the runtime primitives everything else depends on.

### A0. Canonical Vocabulary Alignment (Statuses, Hook Events, Transcript Events)

**Why first:** Every subsequent step writes/reads thread statuses, emits hook events, and writes transcript events. Pin all vocabulary once so later phases don't invent inconsistent names.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_registry.py` — accept `running | completed | error | suspended | cancelled` as valid statuses
- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — write these statuses (not `paused`), use canonical transcript event names

**Decision:** Migrate `paused` → `suspended` everywhere (done in Cleanup 1). No alias — `paused` ceases to exist in all code paths including `conversation_mode.py`.

**Vocabulary to define (as constants in `thread_registry.py` or a shared `thread_constants.py`):**

Thread statuses:

- `THREAD_STATUSES = {"running", "completed", "error", "suspended", "cancelled"}`
- `TERMINAL_STATUSES = {"completed", "error", "cancelled"}` (suspended is NOT terminal)
- `SUSPEND_REASONS = {"limit", "error", "budget", "approval"}` (`approval` = awaiting explicit user input, including conversation-mode pauses)

Hook events (policy checkpoints — harness fires, hooks evaluate):

- `HOOK_EVENTS = {"before_step", "after_step", "error", "limit", "after_complete", "context_window_pressure"}`

Transcript events (audit records — emitted by infrastructure, default behaviors, or hook directives):

- `CRITICAL_TRANSCRIPT_EVENTS` — events written synchronously: `thread_started`, `thread_completed`, `thread_suspended`, `thread_resumed`, `thread_cancelled`, `step_start`, `step_finish`, `cognition_out`, `tool_call_start`, `tool_call_result`, `error_classified`, `retry_succeeded`, `limit_escalation_requested`, `child_thread_started`, `child_thread_failed`, `context_compaction_start`, `context_compaction_end`
- `DROPPABLE_TRANSCRIPT_EVENTS` — events that may use fire-and-forget async emission: `cognition_out_delta`, `cognition_reasoning`, `tool_call_progress`

Event emission helper:

- Define `emit_droppable()` — centralized fire-and-forget helper with `asyncio.create_task` + done callback to consume exceptions. Falls back to drop (not sync write) if no event loop is running. See [Transcript Events](concepts/thread-orchestration-internals.md#transcript-events) for the implementation pattern.

**Test file:** `tests/rye_tests/test_orchestration_internals.py`

**Tests to write:**

```
class TestThreadStatusVocabulary:
    test_valid_statuses_are_defined           — THREAD_STATUSES set contains exactly 5 values
    test_terminal_statuses_subset             — TERMINAL_STATUSES = {completed, error, cancelled} ⊂ THREAD_STATUSES (suspended is NOT terminal — it can resume)
    test_suspend_reasons_are_defined          — SUSPEND_REASONS set contains {limit, error, budget, approval}
    test_state_json_suspend_reason_schema     — state.json with suspend_reason field validates correctly

class TestHookEventVocabulary:
    test_hook_events_defined                  — HOOK_EVENTS set contains exactly 6 values (including context_window_pressure)
    test_context_window_pressure_payload      — payload includes tokens_used (int), max_tokens (int), pressure_ratio (float 0-1)

class TestTranscriptEventVocabulary:
    test_critical_events_defined              — CRITICAL_TRANSCRIPT_EVENTS contains all critical event names
    test_droppable_events_defined             — DROPPABLE_TRANSCRIPT_EVENTS contains {cognition_out_delta, cognition_reasoning, tool_call_progress}
    test_no_overlap                           — critical and droppable sets are disjoint
    test_emit_droppable_no_loop               — emit_droppable with no running event loop does not raise (silently drops)
    test_emit_droppable_with_loop             — emit_droppable with running loop creates a task
```

**Acceptance:**

- `THREAD_STATUSES`, `TERMINAL_STATUSES`, `SUSPEND_REASONS` defined as frozen sets
- `HOOK_EVENTS` includes `context_window_pressure`
- `CRITICAL_TRANSCRIPT_EVENTS` and `DROPPABLE_TRANSCRIPT_EVENTS` defined as frozen sets, disjoint
- `emit_droppable()` helper implemented with done callback (no "Task exception never retrieved" spam)
- All sets importable from `thread_constants.py` (or `thread_registry.py`)

Provider capability discovery (streaming via sinks):

- Provider YAML configs may include a `stream` section with sink configuration. The HTTP primitive supports streaming through sinks — if the provider config has `stream`/`sink` configuration, streaming is available; if absent, it is not.
- Code MUST check `provider_config.get("stream")` or `provider_config.get("config", {}).get("stream")` — NEVER hardcode provider name checks like `if provider == "anthropic"`.
- When streaming is configured, `cognition_out_delta` and `cognition_reasoning` events are emitted via `emit_droppable`. When not configured, only the final `cognition_out` critical event is emitted.
- Update `rye/rye/.ai/tools/rye/agent/providers/anthropic_messages.yaml` to add a `stream` section (initially disabled or absent — streaming support is opt-in per provider config).

---

### A1. Push-Based Coordination: Completion Events + `wait_threads`

**Why next:** Every subsequent phase (parallel spawn, resilience, failure propagation) depends on event-driven coordination.

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/wait_threads.py` — tool that blocks until child threads reach terminal state

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/spawn_thread.py` — add module-level `_completion_events` dict, `get_completion_event()`, `signal_completion()`, `_active_tasks` dict

**Test file:** `tests/rye_tests/test_orchestration_internals.py` (append)

**Tests to write:**

```
class TestCompletionEvents:
    test_get_completion_event_creates_new      — first call creates asyncio.Event; second call returns same
    test_signal_completion_sets_event           — after signal_completion(tid), event.is_set() is True
    test_signal_completion_missing_id_no_error  — signaling unknown tid doesn't raise

class TestWaitThreadsResultSchema:
    test_result_schema_all_completed           — {"success": True, "threads": {tid: {"status": "completed", "cost": {...}}}, "total_cost": 0.0, "elapsed_seconds": ...}
    test_result_schema_one_error               — success is False when any thread has status "error"
    test_result_schema_timeout                  — threads not done within timeout get {"status": "timeout"}
    test_completion_event_fires_on_suspended    — suspended causes wait_threads to return (completion event fires on suspend, but suspended is NOT a terminal status)
    test_completion_event_fires_on_cancelled    — cancelled counts as terminal (wait returns)
    test_unknown_thread_id_returns_error        — thread_id with no active task → {"status": "error", "error": "No active task for thread_id"}

class TestWaitThreadsFailFast:
    test_fail_fast_returns_on_first_error       — returns immediately, failed_thread field set
    test_cancel_siblings_writes_poison_files    — cancel.requested created for non-completed siblings
    test_cancel_siblings_schema                 — poison file contains {requested_at, reason} JSON
```

**Acceptance:**

- `wait_threads.execute(thread_ids=[...])` returns stable result schema
- All coordination via `asyncio.Event` (zero polling, in-process only)
- Unknown `thread_id` with no active task → immediate error result for that thread (no fallback polling)
- `fail_fast=True` + `cancel_siblings_on_failure=True` creates `cancel.requested` files atomically (`.tmp` → rename)

---

### A2. Async Thread Spawning (`execute_directive` Mode)

**Why next:** Needed for parallel waves. Depends on A1 completion events.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/spawn_thread.py` — add `execute_directive: bool` parameter, `asyncio.Task` creation, pre-create completion event, lazy import of `thread_directive.execute`, emit `child_thread_started` transcript event on parent's transcript

**Tests to write:**

```
class TestSpawnThreadAsyncMode:
    test_spawn_returns_immediately              — returns {thread_id, status: "spawned", mode: "async_task"} without blocking
    test_spawn_result_schema                    — required fields: thread_id, status, mode
    test_completion_event_precreated            — event exists before task starts (no race)
    test_active_tasks_registry                  — spawned task tracked in _active_tasks dict
    test_signal_completion_always_fires         — simulate success/error/exception → event always set (finally block)
    test_child_thread_started_emitted           — child_thread_started event written to parent's transcript (audit, not coordination)
```

**Acceptance:**

- `spawn_thread(execute_directive=True)` returns immediately
- Completion event is created BEFORE the task starts (avoids race with `wait_threads`)
- `signal_completion()` fires in the `finally` block regardless of outcome
- `child_thread_started` audit event written to parent's transcript (critical, sync)

---

### A3. Parallel Tool Dispatch

**Why next:** The core concurrency mechanism. Depends on A1/A2 for wave orchestration.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — add `_dispatch_tool_calls_parallel()`, replace serial for-loop in `_run_tool_use_loop()`
- `rye/rye/.ai/tools/rye/agent/threads/core_helpers.py` — same parallel dispatch in `run_llm_loop()` if it has a duplicate serial loop

**Tests to write:**

```
class TestParallelToolDispatch:
    test_groups_by_item_id                      — tool calls with different item_ids go to different groups
    test_same_item_id_sequential                — tool calls targeting same item_id stay ordered
    test_results_in_original_order              — results list maintains input ordering regardless of completion order
    test_concurrency_cap_enforced               — MAX_PARALLEL_GROUPS (e.g., 25) is respected
    test_single_tool_call_no_gather             — single call doesn't needlessly use asyncio.gather
    test_transcript_events_emitted              — tool_call_start and tool_call_result events written per call (critical, sync)
    test_progress_callback_passed               — on_progress callback constructed and passed to _execute_tool_call
    test_progress_throttled                     — on_progress emits at most 1 event/sec per call_id
    test_progress_uses_emit_droppable           — progress events use fire-and-forget emission (not blocking writes)
```

**Acceptance:**

- Different `item_id` tool calls execute concurrently via `asyncio.gather()`
- Same `item_id` calls execute sequentially within their group
- Results array maintains original ordering
- A `MAX_PARALLEL_GROUPS` constant caps concurrency
- `max_concurrent_groups` defaults to 25 (defined in `parallel_dispatch` tool CONFIG_SCHEMA); overridable via directive metadata. See [data-driven-thread-system-index.md](concepts/data-driven-thread-system-index.md) for system vs tool config distinction.
- `on_progress` callback passed to tools; throttled to max 1 event/sec; uses `emit_droppable`
- `tool_call_start`/`tool_call_result` are critical (sync write); `tool_call_progress` is droppable (async)

---

### A4. Capability Token Propagation

**Why next:** Security-critical. Must be in place before hierarchical cost enforcement.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`:
  - `_execute_tool_call()` gains `parent_token` parameter; injects `_token` into params for directive executions
  - `execute()` restricts self-minting to root-only (`_parent_thread_id is None`)
  - Add `_is_directive_execution(item_id, params)` helper

**Tests to write:**

```
class TestCapabilityTokenPropagation:
    test_directive_execution_detected           — _is_directive_execution returns True for thread_directive item_id and item_type=directive
    test_non_directive_not_detected             — _is_directive_execution returns False for regular tools
    test_token_injected_for_directive           — when parent_token set + directive detected, params["_token"] is populated
    test_token_not_injected_for_regular_tool    — regular tool call params don't get _token
    test_child_without_token_rejected           — execute() with _parent_thread_id but no _token returns permission_denied
    test_root_self_mints_token                  — execute() without _parent_thread_id self-mints from permissions
    test_token_dict_serialization               — token with to_dict() method serializes; dict token passes through
```

**Acceptance:**

- Child directive invocations always contain `_token` in parameters
- Child execution without `_token` when `_parent_thread_id` is set → `{"status": "permission_denied", "error": "..."}`
- Root execution without `_parent_thread_id` self-mints from declared permissions

---

### A5. Hierarchical Cost Enforcement (Budget Ledger)

**Why next:** Depends on A4 (token propagation tells us parent-child relationships). Required by Phase B escalation.

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/budget_ledger.py` — `BudgetLedger` class with SQLite backend

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_registry.py` — add `budget_ledger` table to schema init
- `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py` — add `_budget_ledger` reference, check in `check_limits()`
- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — `register_budget()` on startup, `reserve()` before spawning, `report_actual()` on completion

**Tests to write:**

```
class TestBudgetLedgerSchema:
    test_table_created_idempotently             — calling init twice doesn't error
    test_required_columns                       — thread_id, parent_thread_id, reserved_spend, actual_spend, max_spend, status, updated_at

class TestBudgetReservation:
    test_reserve_within_budget_succeeds         — parent max=$3, reserve $0.80 → True
    test_reserve_exceeding_budget_fails         — parent max=$1, reserve $0.60 twice → second returns False
    test_multiple_children_aggregate             — 3 children reserving $0.80 each against $3 budget → all succeed, remaining=$0.60
    test_reserve_requires_explicit_max_spend     — child without declared max_spend in limits → error (no magic $0.50 default)

class TestBudgetActualReporting:
    test_report_actual_releases_reservation     — after report_actual, reserved_spend=0, actual_spend=value
    test_report_actual_clamps_to_reserved       — actual > reserved → clamped to reserved (no inflation)
    test_completed_child_frees_budget            — parent remaining increases after child completes under budget

class TestBudgetRemaining:
    test_remaining_calculation                   — max - own_actual - children_reserved - children_actual
    test_remaining_none_when_no_budget           — no max_spend → returns None (unconstrained)

class TestBudgetLimitEvent:
    test_hierarchical_budget_exceeded_code       — when remaining ≤ 0, produces event with code "hierarchical_budget_exceeded"
```

**Acceptance:**

- SQLite `budget_ledger` table ([data-driven-budget-ledger.md](concepts/data-driven-budget-ledger.md)) with `BEGIN IMMEDIATE` transactions for atomicity
- `reserve()` returns `False` when reservation would exceed remaining
- `report_actual()` clamps to reserved amount (prevents child over-reporting)
- `check_remaining()` aggregates own + children's spend correctly

---

### A6. Managed Subprocess Primitive

**Why next:** Required by app bundling (Phase C) but independent of A1-A5. Can be parallelized.

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/managed_subprocess.py` — start/stop/status/logs for long-lived processes

**Tests to write:**

```
class TestManagedSubprocessHandleSchema:
    test_handle_file_location                   — .ai/threads/{thread_id}/processes/{handle_id}.json
    test_handle_required_fields                 — pid, pgid, command, started_at, status, handle_id
    test_handle_atomic_write                    — written via .tmp → rename

class TestManagedSubprocessActions:
    test_start_returns_handle_id                — start action returns {handle_id, status, pid}
    test_status_returns_running_info            — status action returns {running: bool, pid, uptime}
    test_stop_result_schema                     — stop returns {stopped: bool, exit_code}
    test_logs_returns_tail_lines                — logs action returns last N lines

class TestOutputCapture:
    test_ring_buffer_max_lines                  — buffer respects maxlen, oldest lines dropped
    test_tail_returns_last_n                    — tail(5) on 10-line buffer returns last 5
    test_file_and_buffer_synchronized           — both contain same lines

class TestManagedSubprocessConfig:
    test_config_schema_valid                    — CONFIG_SCHEMA has required fields: action, handle_id, command, args, cwd, env, ready_pattern, ready_timeout, thread_id
```

**Acceptance:**

- Handle file persisted at `.ai/threads/{tid}/processes/{handle_id}.json`
- Atomic writes (`.tmp` → rename)
- `OutputCapture` ring buffer with configurable max_lines
- Stop uses SIGTERM → grace period → SIGKILL sequence

---

## Phase B: Thread Resilience and Recovery (Doc 2)

Depends on Phase A being complete. Builds the production-grade resilience layer.

### B1. Checkpoint-Based State Persistence

**Why first in Phase B:** Every resilience mechanism (retry, escalation, resume, orphan recovery) needs `state.json` to exist.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`:
  - Add `save_state()` calls: (1) after registration/before LLM loop, (2) before each LLM call, (3) after each LLM response, (4) after tool execution
  - Add `thread_started` transcript event emission in `execute()` after registration (critical, sync)
  - Add `thread_completed` transcript event emission in `execute()` before returning (critical, sync)
- `rye/rye/.ai/tools/rye/agent/threads/core_helpers.py` — ensure `save_state()` and `save_state_dict()` do atomic writes (`.tmp` → rename)

**Test file:** `tests/rye_tests/test_thread_resilience.py`

**Tests to write:**

```
class TestCheckpointPersistence:
    test_state_json_schema                      — required fields: directive, inputs, cost, limits, hooks, required_caps
    test_cost_fields                             — cost dict contains: turns, tokens, input_tokens, output_tokens, spawns, spend, duration_seconds
    test_limits_fields                           — limits dict contains: turns, tokens, spend, duration
    test_atomic_write                            — state.json written via .tmp → rename (no partial JSON on crash)
    test_state_survives_simulated_crash          — write state, verify readable after "crash" (just read it back)
    test_state_combined_with_transcript          — state.json + transcript.jsonl together sufficient for reconstruction
```

**Acceptance:**

- `state.json` exists for any thread that has started at least one turn
- Writes are atomic (`.tmp` → rename)
- Schema matches doc spec exactly

---

### B2. Error Classification

**Why next:** Retry logic (B3) depends on error categories.

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/error_classifier.py` — `ErrorCategory` enum, `ERROR_PATTERNS` dict, `classify_error()` function

**Tests to write:**

```
class TestErrorCategory:
    test_enum_values                             — TRANSIENT, RATE_LIMITED, QUOTA, LIMIT_HIT, BUDGET, PERMANENT, CANCELLED

class TestClassifyError:
    test_429_is_rate_limited                     — status_code=429 → RATE_LIMITED with retry_after=30
    test_503_is_transient                        — status_code=503 → TRANSIENT
    test_502_is_transient                        — status_code=502 → TRANSIENT
    test_500_is_transient                        — status_code=500 → TRANSIENT
    test_408_is_transient                        — status_code=408 → TRANSIENT
    test_connection_reset_is_transient           — "connection reset" message → TRANSIENT
    test_socket_timeout_is_transient             — "socket timeout" message → TRANSIENT
    test_overloaded_is_transient                 — "overloaded" message → TRANSIENT
    test_rate_limit_message_is_rate_limited      — "rate limit exceeded" → RATE_LIMITED
    test_quota_exceeded_is_quota                 — "quota exceeded" → QUOTA
    test_invalid_api_key_is_permanent            — "invalid api key" → PERMANENT
    test_model_not_found_is_permanent            — "model not found" → PERMANENT
    test_content_policy_is_permanent             — "content policy" → PERMANENT
    test_unknown_error_defaults_permanent        — unrecognized → PERMANENT (fail-safe)
    test_429_with_retry_after_header             — retry_after from error dict takes precedence over default 30
    test_dict_error_extraction                   — classify_error({"status_code": 429, "error": "..."}) works
    test_exception_error_extraction              — classify_error(Exception("...")) works
    test_string_error_extraction                 — classify_error("connection reset") works
```

**Acceptance:**

- Pure function, no side effects, no imports of heavy modules
- Deterministic mapping for all documented patterns
- Unknown errors default to `PERMANENT` (fail-safe, don't retry unknowns)

---

### B3. Retry Loop with Backoff + HarnessAction.RETRY + Context Pressure Check

**Why next:** Depends on B2 error classification.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py`:
  - **Add `StreamingToolParser` class** — accumulates partial tool calls from stream chunks, yields when complete definition arrives
  - Replace `_run_tool_use_loop()` with `_run_tool_use_loop_streaming()` — uses streaming parser, batches tools (100ms delay or 5 tools)
  - Replace `_call_llm()` with `_stream_llm_with_retry()` — streaming with retry support
  - Add `_check_context_pressure()` — called near `check_limits()` after each LLM turn, emits `context_window_pressure` hook event with hysteresis
  - Add compaction result application — when `context_window_pressure` hook returns a `compaction` payload, reseed message list
  - Add `cognition_out_delta` emission for text chunks (droppable, via `emit_droppable`)
  - Add `cognition_reasoning` emission for reasoning blocks (droppable, via `emit_droppable`)
  - Tool calls execute **inline during streaming** — not waiting for full response
- `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py` — ensure `evaluate_hooks()` returns `HarnessAction` properly

**Tests to write:**

```
class TestRetryPolicy:
    test_max_retries_constant                    — MAX_RETRIES = 3
    test_backoff_base_constant                   — BACKOFF_BASE = 2.0
    test_backoff_max_constant                    — BACKOFF_MAX = 120.0
    test_transient_backoff_exponential           — attempt 0: 2s, attempt 1: 4s, attempt 2: 8s
    test_backoff_capped_at_max                   — never exceeds BACKOFF_MAX
    test_rate_limited_uses_retry_after           — uses retry_after from metadata, not exponential
    test_quota_uses_fixed_delay                  — QUOTA errors use 60s delay
    test_permanent_no_retry                      — PERMANENT errors return immediately
    test_limit_hit_no_retry                      — LIMIT_HIT errors return immediately (escalate)
    test_budget_no_retry                         — BUDGET errors return immediately (escalate)
    test_cancelled_no_retry                      — CANCELLED errors return immediately

class TestHarnessActionRetry:
    test_retry_action_continues_loop             — when hook returns RETRY, loop continues (doesn't exit)
    test_abort_action_exits_with_error           — ABORT returns success=False, abort=True
    test_fail_action_exits_with_error            — FAIL returns success=False
    test_continue_falls_to_default               — CONTINUE triggers default behavior (suspend + escalation for limits)

class TestContextWindowPressure:
    test_pressure_ratio_computed                  — pressure_ratio = tokens_used / max_tokens (float 0-1)
    test_hysteresis_only_fires_on_crossing        — event fires when ratio crosses threshold (0.79→0.81), not when stable above
    test_cooldown_after_compaction                — after compaction hook runs, no re-trigger for at least 1 turn
    test_compaction_payload_applied               — hook returns {"compaction": {"summary": "...", "prune_before_turn": N}} → messages reseeded
    test_no_compaction_without_hook               — if no hook matches context_window_pressure, default behavior is no-op (continue)

class TestStreamingToolParser:
    test_xml_format_single_tool                  # <tool_use>...</tool_use> → yields tool_complete
    test_xml_format_multiple_tools               # Multiple tools in sequence → yields each when complete
    test_partial_accumulation                    # Incomplete tool → no yield; closing tag → yields
    test_text_between_tools                      # Text before/between/after tools → yields text events
    test_json_format_single_tool                 # JSON {"type": "tool_use", ...} → yields tool_complete
    test_json_format_array                       # JSON array of tools → yields each
    test_mixed_text_and_tools                    # Interleaved text and tool definitions
    test_malformed_tool_handling                 # Invalid XML/JSON → error event, continues parsing
    test_buffer_overflow_protection              # Tool definition > 1MB → error, clear buffer

class TestStreamingToolExecution:
    test_tool_executes_during_stream             # Tool complete mid-stream → executes immediately
    test_tool_result_before_stream_ends          # Fast tool → result appears before cognition_out
    test_multiple_tools_batched                  # 3 tools complete in 50ms → batched in one dispatch
    test_batch_size_threshold                    # 5 tools ready → dispatch immediately (don't wait for timer)
    test_batch_delay_timer                       # 1 tool ready → waits 100ms for more → dispatches
    test_timer_cancelled_on_batch_size           # Timer running, 5th tool arrives → timer cancelled, dispatch now
    test_groups_by_item_id                       # Same as parallel dispatch: different item_id → concurrent
    test_same_item_id_sequential                 # Same as parallel dispatch: same item_id → sequential
    test_results_matched_to_calls                # tool_call_result matched to tool_call_start by call_id

class TestStreamingTranscriptEvents:
    test_interleaved_events_order                # delta → delta → tool_start → delta → tool_result → cognition_out
    test_cognition_out_always_last               # cognition_out timestamp > all deltas and tool events
    test_tool_progress_during_stream             # tool_call_progress events interleave with deltas
    test_droppable_events_non_blocking           # emit_droppable doesn't slow down stream processing

class TestStreamingReplay:
    test_reconstruct_from_interleaved            # Replay from transcript with interleaved events → correct state
    test_accumulate_deltas_to_full_text          # cognition_out_delta events + cognition_out → complete message
    test_match_tool_results_by_id                # Multiple tools, results arrive out-of-order → matched correctly

class TestRetryWithStreaming:
    test_retry_on_stream_failure                 # Stream dies mid-response → retry with exponential backoff
    test_partial_tool_calls_discarded_on_retry   # Incomplete tool definitions not executed on retry
    test_completed_tools_not_re_executed         # Tools that completed before stream failure → not retried
    test_retry_reconstructs_context              # Retry includes results from pre-failure tools in context

class TestStreamingDeltaEmission:
    test_delta_emitted_when_stream_configured    # Text chunks → cognition_out_delta via emit_droppable
    test_no_delta_without_stream_config          # No stream config → no deltas, only final cognition_out
    test_no_hardcoded_provider_checks            # Gating uses provider_config.get("stream")
    test_reasoning_emitted_when_available        # cognition_reasoning via emit_droppable when present
    cognition_out_always_emitted                 # Final cognition_out emitted regardless of streaming
```

**Acceptance:**

- `StreamingToolParser` accumulates partial tool definitions, yields `tool_complete` when closing tag/brace arrives
- Tools execute as soon as their definition is complete, not waiting for full LLM response
- Tool batching: execute immediately when 5+ tools ready, or after 100ms delay if fewer
- `_stream_llm_with_retry()` retries up to `MAX_RETRIES` for transient errors
- Exponential backoff with cap
- `HarnessAction.RETRY` from hooks causes the loop to continue
- Transcript events interleave: `cognition_out_delta` + `tool_call_start` + `tool_call_result` + `cognition_out`
- `cognition_out` always emitted after stream ends, contains complete text
- Replay from interleaved transcript correctly reconstructs conversation state
- `context_window_pressure` event emitted with hysteresis when `pressure_ratio` crosses threshold
- Compaction result applied to message list when hook returns `compaction` payload
- Default behavior for `context_window_pressure` with no matching hook: **no-op** (continue — compaction is opt-in)
- No hardcoded provider name checks — all gating is data-driven from provider config

---

### B4. Limit Escalation (Suspend + Approval)

**Why next:** Depends on B1 (state persistence) and A5 (budget ledger). The suspend flow.

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — add `_handle_limit_hit()` function
  - Emit `thread_suspended` transcript event (critical, sync) in `_handle_limit_hit()` before `limit_escalation_requested`
- `rye/rye/.ai/tools/rye/agent/threads/safety_harness.py` — add suspended status support
- `rye/rye/.ai/tools/rye/agent/threads/thread_registry.py` — accept `suspended` status

**Tests to write:**

```
class TestLimitEscalation:
    test_escalation_json_schema                  — required: type, thread_id, directive, limit_code, current_value, current_max, proposed_max, current_cost, message, approval_request_id
    test_escalation_atomic_write                 — .tmp → rename
    test_spend_exceeded_message_format           — contains current spend, limit, turns, tokens
    test_turns_exceeded_message_format           — contains turns used, limit, spend
    test_tokens_exceeded_message_format          — contains tokens used, limit, spend
    test_proposed_limit_doubles_current          — proposed_max = current_max * 2
    test_limit_code_to_key_mapping               — spend_exceeded→spend, turns_exceeded→turns, tokens_exceeded→tokens, spawns_exceeded→spawns

class TestLimitEscalationStatusTransition:
    test_thread_suspended_on_limit_hit           — registry status becomes "suspended"
    test_thread_suspended_event_emitted          — thread_suspended transcript event written (critical, sync) with {directive, suspend_reason, cost}
    test_state_json_preserved_on_suspend         — state.json exists and has current cost
    test_escalation_file_location                — .ai/threads/{thread_id}/escalation.json
```

**Acceptance:**

- On limit hit with no matching hook → thread transitions to `suspended`
- `thread_suspended` transcript event emitted (critical, sync) — separate from `limit_escalation_requested` (which is default-behavior)
- `escalation.json` written atomically
- Approval request follows existing approval-flow file format

---

### B5. Resume Thread Tool

**Why next:** Depends on B4 (escalation creates suspended threads to resume).

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/resume_thread.py`

**Tests to write:**

```
class TestResumeThread:
    test_rejects_non_suspended_thread            — running → error; completed → error
    test_rejects_missing_thread                  — nonexistent thread_id → error
    test_rejects_missing_state_json              — suspended but no state.json → error
    test_bump_and_resume_updates_limits          — spend: 1.0 → 2.0 in state.json
    test_bump_and_resume_clears_escalation       — escalation.json deleted after resume
    test_cancel_action_cancels_thread            — action=cancel → thread status becomes cancelled
    test_resume_updates_status_to_running        — registry status becomes "running" (and thread.json if present)
    test_resume_config_schema                    — CONFIG_SCHEMA has thread_id, action, new_limits
    test_resume_transcript_event                 — thread_resumed event written with previous_status and new_limits

class TestResumeThreadBudgetSync:
    test_spend_bump_updates_budget_ledger        — budget_ledger.update_max_spend called when spend bumped
```

**Acceptance:**

- Only `suspended` threads can be resumed
- `bump_and_resume` updates `state.json` limits AND budget ledger
- `escalation.json` cleared on resume
- `thread_resumed` transcript event emitted

---

### B6. Cancellation

**Why next:** Independent of B4/B5 but rounds out the status state machine.

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/cancel_thread.py`

**Modify:**

- `rye/rye/.ai/tools/rye/agent/threads/thread_directive.py` — check for `cancel.requested` at each loop checkpoint

**Tests to write:**

```
class TestCancelThread:
    test_cancel_creates_poison_file              — cancel.requested exists after cancel call
    test_poison_file_schema                      — {requested_at: ISO, reason: string}
    test_poison_file_atomic_write                — .tmp → rename
    test_cancel_result_schema                    — {success: True, thread_id, status: "cancel_requested"}

class TestCancellationDetection:
    test_loop_detects_cancel_file                — cancel.requested present → loop exits
    test_cancel_file_cleaned_up                  — cancel.requested deleted after detection
    test_state_saved_on_cancel                   — state.json preserved (work recoverable)
    test_thread_cancelled_event_emitted          — transcript gets thread_cancelled event
    test_thread_status_cancelled                 — final status is "cancelled"

class TestCancellationManagedProcesses:
    test_managed_processes_stopped_on_cancel     — processes dir cleaned up
```

**Acceptance:**

- Poison-file based, cooperative cancellation
- Thread checks at each loop checkpoint
- State saved before exiting (work recoverable)
- All managed subprocesses stopped on cancel

---

### B7. Orphan Detection and Recovery

**Why next:** Last resilience feature. Depends on B1 (checkpointing), B5 (resume), B6 (cancel).

**Create:**

- `rye/rye/.ai/tools/rye/agent/threads/orphan_detector.py`

**Tests to write:**

```
class TestOrphanDetection:
    test_running_thread_with_active_task_not_orphan  — in-process task exists → not orphaned
    test_running_thread_no_task_stale_orphan          — no task + stale transcript → orphaned
    test_running_thread_recent_activity_not_orphan   — no task but recent transcript → not orphaned
    test_stale_threshold_default                      — STALE_THRESHOLD_SECONDS = 300
    test_orphan_info_schema                           — {thread_id, directive, last_activity, age_seconds, has_state, cost, recoverable}
    test_recoverable_true_when_state_exists           — has_state=True → recoverable=True
    test_recoverable_false_when_no_state              — has_state=False → recoverable=False

class TestOrphanRecovery:
    test_recover_resume_sets_suspended               — action=resume → status becomes suspended
    test_recover_resume_requires_state               — no state.json → error
    test_recover_mark_error                           — action=mark_error → status becomes error
    test_recover_mark_cancelled                       — action=mark_cancelled → status becomes cancelled

class TestOrphanScanTool:
    test_scan_action_returns_orphan_list             — action=scan → {orphans: [...], count, recoverable}
```

**Acceptance:**

- Orphan = running in registry + no in-process task + stale transcript
- Recovery marks thread as `suspended` (for resume) or `error`/`cancelled` (terminal)
- Scan returns structured list with cost and recoverability info

---

### B8. Hierarchical Failure Propagation

**Mostly delivered by A1 (`wait_threads` fail_fast).** This step validates the integration.

**Tests to write (add to `test_thread_resilience.py`):**

```
class TestHierarchicalFailurePropagation:
    test_wait_fail_fast_returns_on_error             — already covered in A1
    test_child_failure_transcript_event               — child writes child_thread_failed to parent transcript
    test_child_failure_event_schema                   — {child_thread_id, directive, error, cost}
    test_signal_flow_event_then_transcript            — event fires first (coordination), transcript second (audit)
```

**Acceptance:**

- Child errors → completion event fires → `wait_threads` returns immediately
- Audit trail: `child_thread_failed` event in parent transcript
- Coordination and audit are separate (event for speed, transcript for durability)

---

## Phase C: App Bundling and Orchestration (Doc 3)

Depends on Phases A + B being complete. Extends the signing/executor layer.

### C1. Verified Loader for Dynamic Dependencies

**Why first in Phase C:** Security gate. Must be in place before any app code runs.

**Create/Modify:**

- New function `verify_dependency(path, project_path)` in the signing module (near `verify_item` in `rye/core/` or `lilux/primitives/signing.py`)
- Modify `PrimitiveExecutor._execute_builtin()` to call `verify_dependency()` before loading modules
- Modify any `importlib.util.spec_from_file_location` calls in thread tools to use a `VerifiedModuleLoader` wrapper

**Test file:** `tests/rye_tests/test_app_bundling.py`

**Tests to write:**

```
class TestVerifyDependency:
    test_signed_file_passes                       — signed .py file under .ai/tools/ → returns content hash
    test_unsigned_file_rejected                    — unsigned file → raises IntegrityError
    test_path_outside_allowed_roots_rejected       — file outside .ai/tools/ → raises IntegrityError
    test_symlink_rejected                          — symlinked file → raises IntegrityError
    test_allowed_roots_include_project_user_system — three roots: project .ai/tools, user tools, system tools
    test_all_file_types_supported                  — .py, .js, .sh, .yaml, .json — verify_item handles all via MetadataManager

class TestVerifiedModuleLoader:
    test_loads_verified_module                     — signed module loads and executes
    test_rejects_unverified_module                 — unsigned module blocked before exec_module
    test_returns_module_object                     — loaded module has expected attributes
```

**Acceptance:**

- `verify_dependency()` checks: resolved path, allowed roots, no symlinks, valid signature
- Works for all file types the signature format system supports
- All `importlib` dynamic loads in `.ai/tools/` go through verification

---

### C2. Bundler Core Tool + Manifest System

**Why next:** Covers app assets that can't have inline signatures. Provides the bundle lifecycle tool.

**Design decision:** Bundles are NOT a 4th `ItemType`. The bundler is a core tool at `.ai/tools/rye/core/bundler/bundler.py` with action-dispatch, same pattern as `registry.py` and `system.py`. See [bundler-tool-architecture.md](concepts/bundler-tool-architecture.md) for full rationale.

**Create:**

- `rye/rye/.ai/tools/rye/core/bundler/bundler.py` — core tool with actions: `create`, `verify`, `inspect`, `list`
- Manifest location: `.ai/bundles/{bundle_id}/manifest.yaml`

**Actions:**

| Action    | What It Does                                                                                      |
| --------- | ------------------------------------------------------------------------------------------------- |
| `create`  | Walk `.ai/` for files matching `bundle_id` prefix, compute SHA256s, generate + sign manifest YAML |
| `verify`  | Load manifest, verify Ed25519 signature, check SHA256 of every referenced file against disk       |
| `inspect` | Parse manifest, return metadata + file inventory without verification                             |
| `list`    | Find all `manifest.yaml` files under `.ai/bundles/`                                               |

**Tests to write:**

```
class TestManifestSchema:
    test_manifest_yaml_structure                  — has bundle (id, version, created, entrypoint) + files dict
    test_file_entry_has_sha256                    — each file entry has sha256 field
    test_signed_files_marked                      — files with inline signatures have inline_signed: true
    test_manifest_itself_signed                   — first line has rye:signed: prefix
    test_entrypoint_references_directive          — entrypoint has item_type + item_id

class TestBundlerCreate:
    test_walks_directive_tool_knowledge_dirs       — collects files from all 3 item type dirs + plans + lockfiles
    test_sha256_computed_correctly                — sha256 matches actual file content
    test_idempotent_generation                    — same files → same manifest (excluding signature timestamp)
    test_writes_to_bundles_directory              — manifest written to .ai/bundles/{bundle_id}/manifest.yaml
    test_excludes_threads_directory               — .ai/threads/ not included
    test_excludes_node_modules                    — node_modules/ not included
    test_excludes_pycache                         — __pycache__/ not included

class TestBundlerVerify:
    test_valid_manifest_passes                    — all hashes match → {status: ok, files_ok: N}
    test_tampered_file_detected                   — modified file → files_tampered list populated
    test_missing_file_detected                    — deleted file → files_missing list populated
    test_manifest_signature_verified              — invalid manifest signature → rejected
    test_inline_signed_files_double_checked       — items with inline_signed: true get verify_item() too

class TestBundlerInspect:
    test_returns_bundle_metadata                  — bundle.id, version, entrypoint, description
    test_returns_file_inventory                   — list of files with path, sha256, inline_signed, type
    test_returns_file_counts_by_type              — files_by_type: {directive: N, tool: N, knowledge: N, asset: N}

class TestBundlerList:
    test_finds_all_manifests                      — discovers manifest.yaml files under .ai/bundles/
    test_returns_bundle_summary                   — each entry has bundle_id, version, entrypoint
    test_empty_when_no_bundles                    — returns empty list, not error
```

**Acceptance:**

- Bundler tool follows `execute(action, project_path, params)` dispatch pattern
- Manifest YAML with SHA256 for every file in the bundle
- Manifest signed with Ed25519 (inline signature on line 1)
- `verify` action: manifest signature checked, then per-file SHA256 (eager, checks all files)
- Runtime verification (called by tools at load time): lazy, checks files only when accessed
- Manifest canonical path: `.ai/bundles/{bundle_id}/manifest.yaml`

---

### C3. Node Runtime + App Tools

**Why next:** The execution layer for bundled apps. Depends on A6 (managed subprocess).

**Create:**

- `rye/rye/.ai/tools/rye/core/runtimes/node_runtime.py` (if not already exists) — thin config wrapper for node/npm subprocess execution
- `rye/rye/.ai/tools/apps/task-manager/dev_server.py` — managed subprocess: npm run dev
- `rye/rye/.ai/tools/apps/task-manager/test_runner.py` — subprocess: npm test
- `rye/rye/.ai/tools/apps/task-manager/build.py` — subprocess: npm run build

**Tests to write:**

```
class TestNodeRuntimeConfig:
    test_env_config_schema                        — interpreter type, search_paths, var (no fallback — see Cleanup 5)
    test_config_schema                            — command, args, timeout
    test_default_timeout                          — 300 seconds

class TestAppToolSchemas:
    test_dev_server_config_schema                 — action enum: install/start/stop/status; port: integer
    test_test_runner_config_schema                — suite: string; watch: boolean
    test_build_config_schema                      — target enum: client/server/all

class TestAppToolIntegration:
    test_dev_server_start_returns_handle          — start action returns managed process handle
    test_dev_server_stop_uses_handle              — stop requires handle_id
    test_test_runner_default_suite_all            — default suite is "all"
    test_build_default_target_all                 — default target is "all"
```

**Acceptance:**

- Node runtime config matches doc spec (ENV_CONFIG + CONFIG + CONFIG_SCHEMA)
- Dev server integrates with `managed_subprocess` for start/stop
- All tools have valid CONFIG_SCHEMA

---

### C4. Orchestrator Directives + Git Integration + Registry Sharing

**Why last:** Depends on everything above. The capstone integration.

**Tests to write:**

```
class TestBundleStructure:
    test_bundle_includes_directives               — .ai/directives/apps/{name}/ exists
    test_bundle_includes_tools                     — .ai/tools/apps/{name}/ exists
    test_bundle_includes_knowledge                 — .ai/knowledge/apps/{name}/ exists
    test_bundle_includes_plans                     — .ai/plans/{name}/ exists
    test_bundle_includes_lockfile                  — .ai/lockfiles/apps_{name}_*.lock.yaml exists
    test_bundle_excludes_threads                   — .ai/threads/ NOT included
    test_bundle_excludes_node_modules              — node_modules/ NOT included

class TestOrchestratorDirectiveSchema:
    test_directive_has_required_metadata           — name, version, description, model, limits, permissions
    test_limits_include_max_spawns                 — max_spawns declared for orchestrators
    test_permissions_include_spawn_wait            — managed_subprocess, wait_threads, spawn_thread capabilities
    test_wave_steps_have_parallel_attribute        — parallel="true" on wave steps (advisory to LLM; actual parallelism via spawn_thread + wait_threads)

class TestGitIntegration:
    test_commit_message_format                    — "feat({name}): {plan} — {summary}"
    test_per_plan_commits                          — each implement_feature gets its own commit
```

**Acceptance:**

- Bundle directory structure matches doc spec
- Orchestrator directive XML validates with required metadata
- Git commits follow convention: `feat({name}): {plan} — {summary}`

---

## Phase D: Bundle Registry (Push / Pull / Search / Load / Sign)

Depends on Phase C (manifest signing system). Extends the existing registry with first-class bundle support.

### Current State

**Supabase schema already has:**

- `bundles` table: `id`, `bundle_id` (unique), `name`, `description`, `author_id`, `is_official`, `items` (jsonb), `download_count`, `created_at`, `updated_at`
- `ratings` and `reports` tables already accept `item_type = 'bundle'`
- `favorites` table already accepts `item_type = 'bundle'`
- Individual item tables: `directives`, `tools`, `knowledge` with `*_versions` tables

**What's missing:**

- No `bundle_versions` table (bundles aren't versioned)
- No manifest storage (the `items` jsonb is an unstructured list, not a signed manifest with SHA256 hashes)
- No `visibility` or `namespace` columns on `bundles`
- No bundle-level search (name/description only, no full-text vector)
- No API endpoints for bundles (push/pull/search/delete)
- No client-side bundle transport actions in `registry.py`

**Note:** `ratings`, `reports`, and `favorites` tables accept `item_type = 'bundle'`. This is a **registry entity type** for social features, NOT a core `ItemType` used by MCP search/load/sign/execute. See [bundler-tool-architecture.md](concepts/bundler-tool-architecture.md#supabase-schema-notes).

### D0. Schema Migration: Extend `bundles` Table + Add `bundle_versions`

**Why first:** Everything else depends on the schema.

**Migration SQL:**

```sql
-- Add missing columns to bundles
ALTER TABLE bundles
  ADD COLUMN IF NOT EXISTS namespace TEXT NOT NULL DEFAULT 'public',
  ADD COLUMN IF NOT EXISTS category TEXT NOT NULL DEFAULT '',
  ADD COLUMN IF NOT EXISTS visibility TEXT NOT NULL DEFAULT 'private'
    CHECK (visibility IN ('public', 'unlisted', 'private')),
  ADD COLUMN IF NOT EXISTS latest_version TEXT,
  ADD COLUMN IF NOT EXISTS tags TEXT[] DEFAULT '{}',
  ADD COLUMN IF NOT EXISTS search_vector TSVECTOR;

-- Create bundle_versions table (mirrors *_versions pattern)
CREATE TABLE IF NOT EXISTS bundle_versions (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  bundle_id UUID NOT NULL REFERENCES bundles(id) ON DELETE CASCADE,
  version TEXT NOT NULL CHECK (is_valid_semver(version)),
  manifest_yaml TEXT NOT NULL,
  manifest_hash TEXT NOT NULL,
  manifest_signature TEXT NOT NULL,
  changelog TEXT,
  is_latest BOOLEAN DEFAULT false,
  created_at TIMESTAMPTZ DEFAULT now(),
  UNIQUE (bundle_id, version)
);

-- Create bundle_version_items (each file in the bundle)
CREATE TABLE IF NOT EXISTS bundle_version_items (
  id UUID PRIMARY KEY DEFAULT gen_random_uuid(),
  bundle_version_id UUID NOT NULL REFERENCES bundle_versions(id) ON DELETE CASCADE,
  item_type TEXT NOT NULL CHECK (item_type IN ('directive', 'tool', 'knowledge', 'asset')),
  item_path TEXT NOT NULL,
  content TEXT,
  content_hash TEXT NOT NULL,
  has_inline_signature BOOLEAN DEFAULT false,
  size_bytes INTEGER,
  created_at TIMESTAMPTZ DEFAULT now()
);

CREATE INDEX IF NOT EXISTS idx_bundle_versions_bundle_id ON bundle_versions(bundle_id);
CREATE INDEX IF NOT EXISTS idx_bundle_version_items_version_id ON bundle_version_items(bundle_version_id);
CREATE INDEX IF NOT EXISTS idx_bundles_namespace ON bundles(namespace);
CREATE INDEX IF NOT EXISTS idx_bundles_visibility ON bundles(visibility);
```

**Test file:** `tests/rye_tests/test_bundle_registry.py`

**Tests to write:**

```
class TestBundleSchemaConsistency:
    test_bundles_has_namespace_column           — matches directives/tools/knowledge pattern
    test_bundles_has_visibility_column          — matches directives/tools/knowledge pattern
    test_bundle_versions_has_manifest_yaml      — signed manifest stored
    test_bundle_versions_has_manifest_hash      — integrity hash stored
    test_bundle_version_items_has_content_hash  — per-file SHA256
    test_item_type_includes_asset              — assets (JSX, CSS, images) supported
```

---

### D1. Server-Side Bundle API Endpoints

**Why next:** The API is the transport. Client and tests depend on it.

**Create/Modify:**

- `services/registry-api/registry_api/models.py` — add `PushBundleRequest`, `PushBundleResponse`, `PullBundleResponse`, `BundleSearchResultItem`
- `services/registry-api/registry_api/main.py` — add `/v1/bundle/push`, `/v1/bundle/pull/{bundle_id}`, `/v1/bundle/search`, `/v1/bundle/delete/{bundle_id}`, `/v1/bundle/sign/{bundle_id}`

**API Design:**

#### `POST /v1/bundle/push` (auth required)

Request:

```json
{
  "bundle_id": "leolilley/apps/task-manager",
  "name": "task-manager",
  "description": "CRUD task manager with React + Express + SQLite",
  "version": "1.0.0",
  "manifest_yaml": "# rye:signed:...\nbundle:\n  id: apps/task-manager\n  ...",
  "items": [
    {
      "item_type": "directive",
      "item_path": "directives/apps/task-manager/build_crud_app.md",
      "content": "# Build CRUD App\n...",
      "content_hash": "a1b2c3d4..."
    },
    {
      "item_type": "tool",
      "item_path": "tools/apps/task-manager/dev_server.py",
      "content": "# rye:signed:...\n...",
      "content_hash": "5e6f7a8b..."
    },
    {
      "item_type": "asset",
      "item_path": "apps/task-manager/client/src/App.jsx",
      "content": "import React...",
      "content_hash": "9c0d1e2f..."
    }
  ],
  "changelog": "Initial release",
  "visibility": "private"
}
```

Response:

```json
{
  "status": "published",
  "bundle_id": "leolilley/apps/task-manager",
  "version": "1.0.0",
  "items_count": 3,
  "manifest_hash": "...",
  "signature": {
    "timestamp": "...",
    "hash": "...",
    "registry_username": "leolilley"
  }
}
```

Server-side flow:

1. Verify namespace matches authenticated user
2. Verify manifest signature (Ed25519)
3. Verify each item's `content_hash` matches `sha256(content)`
4. For items with inline signatures, verify those too
5. Re-sign manifest with registry provenance (`|registry@username`)
6. Upsert `bundles` row, create `bundle_versions` row, create `bundle_version_items` rows
7. Return response

#### `GET /v1/bundle/pull/{bundle_id}` (public or auth)

Returns the full bundle: manifest + all items. Respects visibility.

#### `GET /v1/bundle/search` (public or auth)

Query params: `query`, `namespace`, `category`, `limit`, `offset`, `include_mine`
Searches `bundles.name`, `bundles.description` with visibility filtering.

#### `POST /v1/bundle/sign/{bundle_id}` (auth required)

Re-signs an existing bundle version with the registry key. Used when the registry key rotates or when a bundle owner wants to refresh signatures.

#### `DELETE /v1/bundle/delete/{bundle_id}` (auth required)

Deletes bundle and all versions/items. Ownership check.

**Tests to write:**

```
class TestPushBundleRequestSchema:
    test_required_fields                        — bundle_id, name, version, manifest_yaml, items
    test_bundle_id_format                       — namespace/category/name (3+ segments)
    test_items_require_content_hash             — each item must have content_hash
    test_item_type_includes_asset               — "asset" is valid alongside directive/tool/knowledge
    test_version_semver_format                  — must match X.Y.Z

class TestPushBundleResponseSchema:
    test_published_response                     — status, bundle_id, version, items_count, manifest_hash, signature

class TestPullBundleResponseSchema:
    test_includes_manifest                      — manifest_yaml field present
    test_includes_all_items                     — items array with content + content_hash per item
    test_items_preserve_paths                   — item_path matches what was pushed

class TestBundleSearchResponseSchema:
    test_result_includes_bundle_fields          — bundle_id, name, description, version, author, download_count
    test_visibility_filtering                   — private bundles hidden from non-owners
```

---

### D2. Client-Side Bundle Transport in `registry.py`

**Why next:** Depends on D1 API and C2 bundler tool. The registry handles transport only — local bundle operations are handled by the bundler core tool (C2).

**Design decision:** Local bundle semantics (`create`, `verify`, `inspect`, `list`) belong in the bundler tool. Only network transport (`push_bundle`, `pull_bundle`) belongs in the registry tool. See [bundler-tool-architecture.md](concepts/bundler-tool-architecture.md#responsibility-split).

**Modify:**

- `rye/rye/.ai/tools/rye/core/registry/registry.py` — add `push_bundle`, `pull_bundle` actions only

**Actions to add to `ACTIONS` list:**

```python
ACTIONS = [
    # ... existing ...
    # Bundle transport
    "push_bundle",
    "pull_bundle",
]
```

#### `push_bundle` — Upload bundle to registry

```python
async def _push_bundle(
    bundle_id: str,
    version: str = None,
    visibility: str = "private",
    project_path: str = None,
) -> Dict[str, Any]:
    """Push a local bundle to the registry.

    Requires manifest to exist at .ai/bundles/{bundle_id}/manifest.yaml.
    Uses bundler tool's manifest creation/verification logic internally.

    Flow:
    1. Load manifest from .ai/bundles/{bundle_id}/manifest.yaml
    2. Verify manifest signature and file hashes (via bundler verify logic)
    3. Read version from manifest (or use version param override)
    4. Collect all referenced files + their content
    5. POST to /v1/bundle/push with manifest + all items
    6. Server re-signs manifest with registry provenance (|registry@username)

    Args:
        bundle_id: Local bundle identifier (e.g., "apps/task-manager")
        version: Override version (default: use manifest version)
        visibility: public/private/unlisted
    """
```

#### `pull_bundle` — Download and extract bundle

```python
async def _pull_bundle(
    bundle_id: str,
    version: str = None,
    destination: str = None,
    verify: bool = True,
    project_path: str = None,
) -> Dict[str, Any]:
    """Pull a bundle from registry and extract to local .ai/ directory.

    Flow:
    1. GET /v1/bundle/pull/{bundle_id}?version=...
    2. Verify manifest signature (Ed25519, including registry provenance)
    3. For each item: verify content_hash matches sha256(content)
    4. For items with inline signatures: verify those too
    5. Write files to .ai/ preserving directory structure
    6. Write manifest to .ai/bundles/{bundle_id}/manifest.yaml
    7. Return extraction report

    Args:
        bundle_id: Registry identifier (namespace/category/name)
        version: Specific version or "latest"
        destination: Override destination (default: project .ai/)
        verify: Verify all signatures (default True)
    """
```

**Note:** Remote bundle discovery uses the existing `search` action with query parameters — the server-side bundle search endpoint returns bundles alongside items. No separate `search_bundle` action needed.

**Tests to write:**

```
class TestPushBundleClient:
    test_requires_manifest_exists                — error if no manifest.yaml at expected path
    test_verifies_manifest_before_push           — invalid manifest → error before network call
    test_collects_all_manifest_files             — reads all files referenced in manifest
    test_sends_manifest_plus_items               — POST body includes manifest_yaml + items array
    test_result_schema                           — {status, bundle_id, version, items_count, signature}

class TestPullBundleClient:
    test_extracts_to_destination                — files written preserving directory structure
    test_writes_manifest_to_bundles_dir          — manifest written to .ai/bundles/{bundle_id}/manifest.yaml
    test_verifies_manifest_signature            — invalid signature → error
    test_verifies_content_hashes                — tampered file → error
    test_verifies_inline_signatures             — signed items get inline signature check
    test_pull_result_schema                     — {status, bundle_id, version, items_extracted, destination}
```

---

### D3. Bundler ↔ Registry Integration

**Why:** Connects the bundler core tool (C2) to the registry transport (D2). The bundler's manifest logic is imported as library functions by the registry tool — not called via MCP tool dispatch.

**Integration points:**

- `push_bundle` in `registry.py` imports and calls bundler library functions to load + verify the local manifest before uploading
- `pull_bundle` in `registry.py` imports bundler verification logic to check the downloaded manifest + files
- Server-side `push` endpoint uses its own manifest verification (independent of client-side bundler, but same SHA256 + Ed25519 checks)
- The bundler tool's `create` action must be called **before** `push_bundle` — the registry does not auto-generate manifests

**Bundler library functions** (importable from `bundler.py` or a `bundler_lib.py` helper):

- `load_manifest(bundle_id, project_path) -> dict` — parse manifest YAML
- `verify_manifest(bundle_id, project_path) -> dict` — verify signature + hashes
- `collect_bundle_files(manifest, project_path) -> list` — read all files referenced in manifest

**Tests to write:**

```
class TestBundleManifestRoundTrip:
    test_push_then_pull_preserves_manifest      — push a bundle, pull it, manifests match
    test_push_then_pull_preserves_hashes        — all content_hash values survive round-trip
    test_tampered_item_rejected_on_pull         — modify a pulled file → bundler verify fails
    test_registry_re_signs_manifest             — pulled manifest has |registry@username suffix
    test_push_requires_manifest_exists          — push_bundle without prior bundler create → error
```

---

## Execution Summary

| Phase   | Steps       | New Files                                                                             | Modified Files                                                                                                                        | Test File                         |
| ------- | ----------- | ------------------------------------------------------------------------------------- | ------------------------------------------------------------------------------------------------------------------------------------- | --------------------------------- |
| **Pre** | Cleanup 1–7 | none                                                                                  | `conversation_mode.py`, `safety_harness.py`, `thread_directive.py`, `AGENT_THREADS_IMPLEMENTATION.md`, `test_agent_threads_future.py` | existing tests (update)           |
| **A**   | A0–A6       | `thread_constants.py`, `wait_threads.py`, `budget_ledger.py`, `managed_subprocess.py` | `thread_directive.py`, `spawn_thread.py`, `core_helpers.py`, `safety_harness.py`, `thread_registry.py`                                | `test_orchestration_internals.py` |
| **B**   | B1–B8       | `error_classifier.py`, `resume_thread.py`, `cancel_thread.py`, `orphan_detector.py`   | `thread_directive.py`, `safety_harness.py`, `thread_registry.py`, `wait_threads.py`, `core_helpers.py`                                | `test_thread_resilience.py`       |
| **C**   | C1–C4       | `verify_dependency`, `bundler.py` (core tool), `node_runtime.py`, app tools           | `PrimitiveExecutor`, signing module, subprocess primitive                                                                             | `test_app_bundling.py`            |
| **D**   | D0–D3       | `bundle_versions` + `bundle_version_items` tables, bundle API endpoints               | `registry_api/models.py`, `registry_api/main.py`, `registry/registry.py`                                                              | `test_bundle_registry.py`         |

**Total new test files:** 4
**Total new source files:** ~13
**Total modified source files:** ~11 (some modified in multiple phases)

**Order of execution:** Pre (cleanups 1–7) → A0 → A1 → A2 → A3 → A4 → A5 → A6 → B1 → B2 → B3 → B4 → B5 → B6 → B7 → B8 → C1 → C2 → C3 → C4 → D0 → D1 → D2 → D3

Pre-phase cleanups can all be done in parallel (independent files).
A6 (managed subprocess) can be done in parallel with A1–A5 since it has no dependencies on them.
B2 (error classifier) can be done in parallel with B1 since it's a pure function with no file dependencies.
D0 (schema migration) can be done in parallel with C3/C4 since it only touches Supabase, not source code.
