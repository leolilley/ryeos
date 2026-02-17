# Thread Orchestration Internals

How the agent thread system evolves from single-thread execution to full wave-based orchestration with parallel dispatch, managed subprocesses, hierarchical budgets, and capability confinement.

This document is the implementation companion to [app-bundling-and-orchestration.md](app-bundling-and-orchestration.md). That document describes _what_ the orchestration does. This document describes _how_ the thread runtime makes it work — and what needs to change to get there.

## Related Documents

- **[thread-streaming-execution.md](thread-streaming-execution.md)** — Streaming inline tool execution (replaces batch-after-response)
- **[thread-coordination-events.md](thread-coordination-events.md)** — Push-based coordination via `asyncio.Event` (replaces polling)
- **[data-driven-thread-system-index.md](data-driven-thread-system-index.md)** — Data-driven configuration for all thread behavior

## Current State

The thread system today provides:

- **Single-thread execution** — `thread_directive.py` runs one LLM loop per directive, runs tool calls sequentially, writes transcript events, and tracks cost via `SafetyHarness`
- **Conversation continuation** — `conversation_mode.py` resumes a suspended thread by reconstructing messages from `transcript.jsonl` and restoring harness state from `state.json`
- **Registry persistence** — `thread_registry.py` uses SQLite with WAL mode for concurrent reads, tracks thread status and parent-child relationships
- **Channel coordination** — `thread_channels.py` provides round-robin and on-demand turn-taking between threads in a shared channel
- **Telemetry aggregation** — `thread_telemetry.py` aggregates cost metrics across threads after the fact
- **Hook execution** — hooks are triggered at checkpoints (before_step, after_step, error, limit) and run child directives with attenuated capability tokens

**Removed (see [thread-coordination-events.md](thread-coordination-events.md)):**
- ~~Transcript watching / polling~~ — replaced with push-based `asyncio.Event` coordination

These are solid primitives for audited single-thread execution. But the app orchestration pattern requires five capabilities that don't exist yet.

### Dual-Path Architecture

All orchestration uses a **dual-path architecture** that separates coordination from audit:

1. **Coordination path** (`asyncio.Event`) — instant, in-process signals for thread completion. Used by `wait_threads` to block until children finish. Zero polling, zero token cost.
2. **Audit path** (transcript JSONL) — durable, append-only records for replay, debugging, and post-hoc analysis. Written by infrastructure, default behaviors, and hook directives.

The key invariant: **coordination signals never flow through the transcript.** Mixing the two creates a system where notifications exist but nobody reads them during execution. Transcript events are emitted at strategic checkpoints for auditability — they are not a coordination mechanism. See [Transcript Events](#transcript-events) for the canonical event vocabulary and emission patterns.

## The Five Gaps

### Gap 1: Streaming Parallel Tool Dispatch

**See [thread-streaming-execution.md](thread-streaming-execution.md) for full details on streaming inline execution.**

**Problem.** Tool calls execute in batch-after-response mode with sequential execution.

In the old `_run_tool_use_loop()`, tool calls execute after the complete LLM response arrives, in a sequential `for` loop:

In `_run_tool_use_loop()`, tool calls execute in a sequential `for` loop:

```python
# thread_directive.py, line 806
for tc in parsed["tool_calls"]:
    tr = await _execute_tool_call(tc, tool_map, project_path)
```

If the LLM returns `[rye_execute(plan_db_schema), rye_execute(plan_api_routes)]` in one turn, they run one after the other. Wave 1 cannot be parallel.

`thread_tool.py` has `threading.Thread` support but its tool entrypoint always passes `target_func=None` — it validates and registers but never creates concurrent execution. The new design replaces `threading.Thread` with `asyncio.Task` for in-process concurrency — all coordination uses asyncio primitives.

**Impact.** Without parallel dispatch, the orchestrator degrades to sequential child execution. A 3-wave plan with 2 children per wave takes 6 sequential LLM sessions instead of 4 (2 parallel + 1 + 1).

> **opencode reference:** opencode uses streaming inline execution — tools execute as soon as their definition arrives in the LLM stream. We adopt the same approach: tools don't wait for the full response, they execute immediately via `StreamingToolParser`. Parallel dispatch via `asyncio.gather()` still applies, grouping by `item_id`.

### Gap 2: Long-Running Subprocess Management

**Problem.** The subprocess primitive is fire-and-wait with a hard timeout.

`node_runtime.py` sets `timeout: 300` (5 minutes). The subprocess primitive awaits completion — no handle is returned, no background execution is possible. A dev server (`npm run dev`) gets killed after 300 seconds.

There is no concept of:

- Starting a process and getting a handle back
- Querying whether a background process is still running
- Stopping a managed process by handle
- Persisting process identity (pid/pgid) across thread turns

**Impact.** The orchestrator cannot start a dev server, run integration tests against it, and then stop it. The entire "run dev server → test → iterate" loop described in the bundling doc is blocked.

> **opencode reference:** opencode's `bash.ts` uses `detached: true` + process group kill (`Shell.killTree()` sends SIGTERM then SIGKILL after 200ms) + abort signal integration. Our `managed_subprocess` builds on these same primitives but adds handle persistence and status polling, which opencode lacks.

### Gap 3: Push-Based Coordination

**See [thread-coordination-events.md](thread-coordination-events.md) for full implementation details.**

**Problem.** No efficient mechanism for threads to signal completion to waiting parents.

The orchestrator needs to "execute Wave 1, wait, execute Wave 2". Without push-based coordination, this requires either:
- Token-expensive polling: LLM repeatedly checks status
- Latency-expensive sleeping: tool waits fixed intervals

**Solution: event-driven coordination via `asyncio.Event`.**

Each child thread gets a completion `asyncio.Event`. The child sets it in its `finally` block (whether success, error, cancellation, or suspension). `wait_threads` awaits these events directly — zero polling, zero latency, immediate failure notification.

**Key properties:**
- All threads run in-process as `asyncio.Task`s
- No out-of-process fallback (returns error if no active task)
- Completion guaranteed via `finally` block
- Transcript events are for audit only, never used for coordination

### Gap 4: Capability Token Propagation

**Problem.** LLM-child thread directives bypass the capability attenuation chain.

The hook execution path correctly propagates `_token`:

```python
# thread_directive.py, _execute_hook(), line 904
params = { ..., "_token": token_data, "_parent_thread_id": parent_thread_id }
executor.execute(item_id="rye/agent/threads/thread_directive", parameters=params)
```

But the LLM tool-use path does not:

```python
# thread_directive.py, _execute_tool_call(), line 683
executor = PrimitiveExecutor(project_path=project_path)
result = await executor.execute(item_id=item_id, parameters=params)
# ⚠️ params contains only LLM tool input — no _token injected
```

When the child directive's `execute()` runs without `_token`, it self-mints a fresh token from its own declared permissions (line 972–974). The child gets capabilities derived from _its own_ declarations, not the intersection with the parent. This defeats hierarchical confinement.

**Impact.** A parent directive with `rye.execute.tool.apps_task-manager_*` could thread a child that self-mints `rye.execute.tool.*` — wider than the parent's own capabilities. The attenuation invariant is violated.

### Gap 5: Hierarchical Cost Enforcement

**Problem.** Cost tracking is per-thread with no real-time aggregation.

Each `SafetyHarness` has its own `CostTracker`. `increment_spawn_count()` exists but is never called. `ThreadTelemetry` aggregates costs after the fact, but there's no transactional check before allowing a new child thread or a new LLM turn to execute.

The orchestrator's `max_spend=$3.00` applies only to its own LLM turns. If it creates 4 children that each spend $1.00, the total is $4.00 but no limit was enforced.

**Impact.** An orchestrator cannot guarantee that the total cost across its entire thread tree stays within budget. Cost overruns are detected only after they happen.

---

## Implementation Plan

### Data-Driven Configuration

All thread behavior is driven from YAML configuration files. See:
- **[data-driven-thread-events.md](data-driven-thread-events.md)** — Event type definitions
- **[data-driven-error-classification.md](data-driven-error-classification.md)** — Error patterns & retry policies  
- **[data-driven-hooks.md](data-driven-hooks.md)** — Hook conditions & actions

**Configuration Location:**
- System defaults: `rye/rye/.ai/tools/rye/agent/threads/config/`
- Project overrides: `.ai/config/thread_*.yaml`

> **Import convention:** Code snippets in this doc use bare module names (`from thread_directive import ...`, `from thread_tool import ...`) because all thread tools live in the same directory (`rye/rye/.ai/tools/rye/agent/threads/`) and are loaded via `importlib` at runtime. These are not Python package imports.

### 1. Streaming Parallel Tool Dispatch

**See [thread-streaming-execution.md](thread-streaming-execution.md) for detailed implementation.**

#### Design

Replace batch-after-response with streaming inline execution. Tools execute as soon as their definition arrives in the LLM stream, not after the full response completes. Multiple tools accumulate in a buffer, then dispatch in parallel via `asyncio.gather()` grouped by `item_id`. that classifies tool calls as independent or dependent, then runs independent calls concurrently via `asyncio.gather`.

Independence is determined by a simple rule: **two tool calls are independent if they target different item_ids** (different tools or different directive invocations). The LLM cannot know about filesystem conflicts — that's handled by git branches per child thread. For safety, add a per-turn concurrency cap (configurable in provider YAML or directive metadata) so a single response can't create unbounded parallelism.

#### Changes

**`thread_directive.py` — `_run_tool_use_loop()`**

Replace the serial for-loop with a parallel dispatch function:

```python
async def _dispatch_tool_calls_parallel(
    tool_calls: List[Dict],
    tool_map: Dict[str, str],
    project_path: Path,
    harness: SafetyHarness,
    transcript: Optional[Any],
    thread_id: str,
    directive_name: str,
    parent_token: Optional[Any] = None,
) -> List[Dict]:
    """Execute tool calls concurrently where safe.

    Independence rule: calls to different item_ids run in parallel.
    Calls to the same item_id run sequentially (preserve ordering).

    Returns list of tool results in original call order.
    """
    import time

    # Group by target item_id
    groups: Dict[str, List[Tuple[int, Dict]]] = {}
    for idx, tc in enumerate(tool_calls):
        target = tool_map.get(tc["name"], tc["name"])
        groups.setdefault(target, []).append((idx, tc))

    results = [None] * len(tool_calls)

    async def run_group(calls: List[Tuple[int, Dict]]):
        """Run calls within a group sequentially (same target)."""
        for idx, tc in calls:
            call_id = tc.get("id", "")

            # Critical audit event — written synchronously
            if transcript and thread_id:
                try:
                    transcript.write_event(thread_id, "tool_call_start", {
                        "directive": directive_name,
                        "tool": tc["name"],
                        "call_id": call_id,
                        "input": tc["input"],
                    })
                except Exception:
                    pass

            # Build optional progress callback for long-running tools.
            # Progress is non-blocking by default; transcript persistence
            # uses emit_droppable (fire-and-forget) and is throttled
            # (max 1/sec or coarse milestones) to avoid JSONL bloat.
            _last_progress_time = [0.0]
            def on_progress(pct: float, message: str = ""):
                now = time.time()
                if now - _last_progress_time[0] < 1.0:
                    return  # Throttle: max 1 event/sec
                _last_progress_time[0] = now
                if transcript and thread_id:
                    emit_droppable(transcript, thread_id, "tool_call_progress", {
                        "call_id": call_id,
                        "progress": pct,
                        "message": message,
                    })

            start_time = time.time()
            tr = await _execute_tool_call(
                tc, tool_map, project_path,
                parent_token=parent_token,
                on_progress=on_progress,
            )
            duration_ms = int((time.time() - start_time) * 1000)

            # Critical audit event — written synchronously
            if transcript and thread_id:
                try:
                    transcript.write_event(thread_id, "tool_call_result", {
                        "directive": directive_name,
                        "call_id": call_id,
                        "output": str(tr.get("result", ""))[:1000],
                        "error": tr.get("error") if tr.get("is_error") else None,
                        "duration_ms": duration_ms,
                    })
                except Exception:
                    pass

            results[idx] = tr

    # Run groups concurrently
    await asyncio.gather(*(run_group(calls) for calls in groups.values()))

    return results
```

**`core_helpers.py` — `run_llm_loop()`**

Same change — replace the serial for-loop with a call to the parallel dispatcher.

**`thread_tool.py` — actually thread**

The `thread_tool` tool should gain an `execute_directive` mode where it:

1. Validates and registers the thread
2. Creates an `asyncio.Task` wrapping `thread_directive.execute()`
3. Returns the thread_id immediately (non-blocking from the caller's perspective)
4. The task runs in the background event loop

This requires the orchestrator's event loop to be shared with child tasks. Since `thread_directive.execute()` is already async, this is natural:

```python
async def thread_tool(
    thread_id: str,
    directive_name: str,
    ...
    execute_directive: bool = False,
    directive_inputs: Optional[Dict] = None,
    parent_token: Optional[Any] = None,  # Injected by runtime, NOT by LLM
    ...
    **params,  # Contains _token and _thread_id from executor context
) -> Dict[str, Any]:
    # parent_token and parent_thread_id are read from executor-injected params,
    # NOT from LLM tool input. The LLM never sees or supplies these values.
    parent_token = parent_token or params.get("_token")
    parent_thread_id = params.get("_thread_id") or params.get("_parent_thread_id")
    ...
    if execute_directive:
        # Lazy import to avoid circular dependency.
        # At runtime, thread_directive is in the same package directory.
        from thread_directive import execute as td_execute

        async def _run():
            try:
                return await td_execute(
                    directive_name=directive_name,
                    inputs=directive_inputs or {},
                    project_path=project_path,
                    _token=parent_token,
                    _parent_thread_id=parent_thread_id,
                )
            finally:
                signal_completion(sanitized_id)

        get_completion_event(sanitized_id)  # Pre-create event before task starts
        task = asyncio.create_task(_run(), name=sanitized_id)
        # Completion event already created by get_completion_event() above
        _active_tasks[sanitized_id] = task

        return {
            "success": True,
            "thread_id": sanitized_id,
            "status": "threaded",
            "mode": "async_task",
        }
    ...
```

#### Task Registry (in-process)

A module-level dict tracks active asyncio tasks:

```python
# thread_tool.py (module level)
_active_tasks: Dict[str, asyncio.Task] = {}
_completion_events: Dict[str, asyncio.Event] = {}

def get_task(thread_id: str) -> Optional[asyncio.Task]:
    return _active_tasks.get(thread_id)

def get_completion_event(thread_id: str) -> asyncio.Event:
    """Get or create a completion event for a thread.

    The event is set when the thread reaches any terminal state
    (completed, error, suspended, cancelled). wait_threads awaits
    these events instead of polling.
    """
    if thread_id not in _completion_events:
        _completion_events[thread_id] = asyncio.Event()
    return _completion_events[thread_id]

def signal_completion(thread_id: str) -> None:
    """Signal that a thread has reached a terminal state.

    Called from thread_directive.execute()'s finally block.
    Wakes up any wait_threads calls blocked on this thread.
    """
    event = _completion_events.get(thread_id)
    if event:
        event.set()

def remove_task(thread_id: str) -> None:
    _active_tasks.pop(thread_id, None)
    _completion_events.pop(thread_id, None)
```

This is process-scoped — tasks and events only exist while the orchestrator process is alive. In-process events provide zero-latency coordination within a single orchestration run. The SQLite registry provides persistence for post-hoc queries and orphan detection, not for coordination.

The key invariant: **`signal_completion()` is called from `thread_directive.execute()`'s `finally` block**, ensuring the event fires regardless of how the thread terminates (success, error, exception, cancellation, suspension). This is the push side of the coordination — `wait_threads` is the pull side.

---

### 2. Managed Subprocess Primitive

#### Design

A new tool — `rye/agent/threads/managed_subprocess.py` — that wraps the subprocess primitive with lifecycle management. It starts a process, returns a handle, and supports stop/status/logs queries.

The tool stores process metadata in the thread's directory:

```
.ai/threads/{thread_id}/processes/
    {handle_id}.json    ← pid, pgid, command, started_at, status
```

#### Interface

```python
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "action": {
            "type": "string",
            "enum": ["start", "stop", "status", "logs"],
        },
        "handle_id": {
            "type": "string",
            "description": "Process handle (required for stop/status/logs)",
        },
        "command": {
            "type": "string",
            "description": "Command to run (required for start)",
        },
        "args": {
            "type": "array",
            "items": {"type": "string"},
        },
        "cwd": {
            "type": "string",
            "description": "Working directory",
        },
        "env": {
            "type": "object",
            "description": "Environment variables",
        },
        "ready_pattern": {
            "type": "string",
            "description": "Regex pattern in stdout that signals readiness (e.g., 'ready on port')",
        },
        "ready_timeout": {
            "type": "integer",
            "description": "Seconds to wait for ready_pattern before returning handle anyway",
            "default": 30,
        },
        "thread_id": {
            "type": "string",
            "description": "Thread that owns this process",
        },
    },
    "required": ["action"],
}
```

#### Process Lifecycle

```
start(command, ready_pattern) → handle_id
    1. Spawn subprocess with Popen (not asyncio.create_subprocess — we need pgid)
    2. os.setpgrp() in preexec_fn for clean group kill
    3. Store pid/pgid/command/status in {handle_id}.json
    4. If ready_pattern provided: poll stdout until pattern matches or timeout
    5. Return handle_id + initial status

status(handle_id) → { running: bool, pid, uptime, exit_code }
    1. Load {handle_id}.json
    2. os.kill(pid, 0) to check if alive
    3. Return current state

logs(handle_id, tail=50) → last N lines of stdout+stderr
    1. Read from captured output buffer or log file
    2. Return tail lines

stop(handle_id) → { stopped: bool, exit_code }
    1. Load {handle_id}.json
    2. os.killpg(pgid, SIGTERM)
    3. Wait up to 5s for exit
    4. If still alive: os.killpg(pgid, SIGKILL)
    5. Update {handle_id}.json with exit status
    6. Return result

Crash recovery: on startup, scan process handles owned by running threads. If pid
is dead or belongs to a different executable, mark the handle stale and clean up
the log/handle files to avoid leaking stale state.
```

#### Integration with Safety Harness

The managed subprocess tool requires capability `rye.execute.tool.managed_subprocess`. The harness tracks managed processes as a resource — when a thread completes or errors, its on_complete hook should stop all managed processes owned by that thread.

This is enforced by a cleanup hook in the orchestrator directive. To avoid relying solely on hooks, the managed subprocess tool should also perform best-effort cleanup on thread cancellation (cancel.requested) and on resume failures.

```xml
<hooks>
  <hook event="after_complete">
    <directive>cleanup_managed_processes</directive>
    <inputs>
      <thread_id>{thread_id}</thread_id>
    </inputs>
  </hook>
</hooks>
```

#### Output Capture

Stdout and stderr are tee'd to both an in-memory ring buffer (last 1000 lines) and a file at `.ai/threads/{thread_id}/processes/{handle_id}.log`. The log file enables post-mortem debugging. The ring buffer enables real-time `logs(tail=50)` without file I/O.

```python
class OutputCapture:
    """Thread-safe tee to ring buffer + file."""

    def __init__(self, log_path: Path, max_lines: int = 1000):
        self._buffer = collections.deque(maxlen=max_lines)
        self._log_file = open(log_path, "a", encoding="utf-8")
        self._lock = threading.Lock()

    def write(self, line: str) -> None:
        with self._lock:
            self._buffer.append(line)
            self._log_file.write(line)
            self._log_file.flush()

    def tail(self, n: int = 50) -> List[str]:
        with self._lock:
            return list(self._buffer)[-n:]

    def close(self) -> None:
        self._log_file.close()
```

---

### 3. Wait / Join Primitive

#### Design

A new tool — `rye/agent/threads/wait_threads.py` — that blocks in Python until specified threads complete. This runs at the tool execution layer, not the LLM layer, so it consumes zero LLM turns while waiting.

#### Interface

```python
__tool_type__ = "python"
__executor_id__ = "rye/core/runtimes/python_runtime"
__category__ = "rye/agent/threads"

CONFIG_SCHEMA = {
    "type": "object",
    "properties": {
        "thread_ids": {
            "type": "array",
            "items": {"type": "string"},
            "description": "Thread IDs to wait for",
        },
        "timeout": {
            "type": "number",
            "description": "Max seconds to wait (0 = no limit)",
            "default": 600,
        },
        "require_all": {
            "type": "boolean",
            "description": "If true, wait for ALL threads. If false, return when ANY completes.",
            "default": true,
        },
        "fail_fast": {
            "type": "boolean",
            "description": "Return immediately when any thread errors",
            "default": false,
        },
        "cancel_siblings_on_failure": {
            "type": "boolean",
            "description": "Cancel remaining threads when one fails (requires fail_fast)",
            "default": false,
        },
    },
    "required": ["thread_ids"],
}
```

#### Implementation

```python
async def execute(
    thread_ids: List[str],
    timeout: float = 600,
    require_all: bool = True,
    fail_fast: bool = False,
    cancel_siblings_on_failure: bool = False,
    **params,
) -> Dict[str, Any]:
    """Wait for threads to reach terminal state.

    Push-based coordination via asyncio.Event:
    Each child thread sets an asyncio.Event on completion. This function
    awaits those events directly — zero polling, zero wasted tokens.

    All threads must be in-process. Unknown thread_ids with no active
    task return an immediate error result (no polling fallback).

    Returns aggregated results: per-thread status, cost, errors.
    """
    project_path = Path(params.get("_project_path", Path.cwd()))
    start_time = time.time()

    from thread_tool import (
        get_task, get_completion_event,
    )

    results = {}
    registry = ThreadRegistry(project_path / ".ai" / "threads" / "registry.db")

    # All threads must be in-process (no polling fallback)
    in_process = {}
    for tid in thread_ids:
        task = get_task(tid)
        if task is not None:
            in_process[tid] = get_completion_event(tid)
        else:
            results[tid] = {"status": "error", "error": f"No active task for thread {tid}"}

    # Await completion events (zero polling)
    if in_process:
        async def wait_for_thread(tid: str, event: asyncio.Event):
            await event.wait()
            task = get_task(tid)
            if task and task.done():
                try:
                    task_result = task.result()
                    return tid, {
                        "status": task_result.get("status", "completed"),
                        "cost": task_result.get("cost", {}),
                        "error": task_result.get("error"),
                    }
                except Exception as e:
                    return tid, {"status": "error", "error": str(e)}
            # Task not done but event set = suspended/cancelled
            status = registry.get_status(tid)
            return tid, {
                "status": status.get("status", "error") if status else "error",
                "cost": json.loads(status.get("total_usage_json", "{}")) if status else {},
            }

        waiters = [wait_for_thread(tid, ev) for tid, ev in in_process.items()]

        if require_all and not fail_fast:
            # Wait for all, with timeout
            done, pending = await asyncio.wait(
                [asyncio.create_task(w) for w in waiters],
                timeout=timeout if timeout > 0 else None,
            )
            for coro in done:
                tid, result = coro.result()
                results[tid] = result
            for coro in pending:
                coro.cancel()
                # Extract tid from the pending coroutine
                results[next(t for t in in_process if t not in results)] = {"status": "timeout"}
        else:
            # Return as each completes (for fail_fast or require_any)
            for coro in asyncio.as_completed(
                [asyncio.create_task(w) for w in waiters],
                timeout=timeout if timeout > 0 else None,
            ):
                try:
                    tid, result = await coro
                    results[tid] = result

                    # Fail fast: check if this child errored
                    if fail_fast and result.get("status") == "error":
                        if cancel_siblings_on_failure:
                            for sibling_id in in_process:
                                if sibling_id != tid and sibling_id not in results:
                                    cancel_path = project_path / ".ai" / "threads" / sibling_id / "cancel.requested"
                                    cancel_path.parent.mkdir(parents=True, exist_ok=True)
                                    tmp_path = cancel_path.with_suffix(".tmp")
                                    tmp_path.write_text(json.dumps({
                                        "requested_at": datetime.now(timezone.utc).isoformat(),
                                        "reason": f"Sibling thread {tid} failed",
                                    }))
                                    tmp_path.rename(cancel_path)
                                    results[sibling_id] = {"status": "cancelled", "reason": "sibling_failure"}
                        break

                    if not require_all:
                        break
                except asyncio.TimeoutError:
                    for tid in in_process:
                        if tid not in results:
                            results[tid] = {"status": "timeout"}
                    break

    # Aggregate
    total_cost = sum(r.get("cost", {}).get("spend", 0) for r in results.values())
    all_success = all(r.get("status") == "completed" for r in results.values())

    return {
        "success": all_success,
        "threads": results,
        "total_cost": total_cost,
        "elapsed_seconds": time.time() - start_time,
    }
```

#### How the Orchestrator Uses It

From the LLM's perspective, a wave execution looks like this (a single tool-use turn, not a polling loop):

```
Turn 1: LLM calls rye_execute(thread_tool, {directive: plan_db_schema, execute_directive: true})
        LLM calls rye_execute(thread_tool, {directive: plan_api_routes, execute_directive: true})
        → Both return immediately with thread_ids

Turn 2: LLM calls rye_execute(wait_threads, {thread_ids: ["plan_db_schema-...", "plan_api_routes-..."]})
        → Blocks in Python until both complete
        → Returns aggregated results (status, cost, errors) in one response

Turn 3: LLM inspects results, executes Wave 2
```

Three LLM turns for the entire wave, regardless of how long the children take. Compare to polling: 3 + N turns where N is the number of poll cycles.

---

### 4. Capability Token Propagation

#### Design

The fix has two parts:

1. `_execute_tool_call` must inject the parent's capability token into child directive invocations
2. The token must be attenuated based on the child directive's declared permissions before injection

#### Changes

**`thread_directive.py` — `_execute_tool_call()`**

The function signature gains `harness` and `parent_token` parameters:

```python
async def _execute_tool_call(
    tool_call: Dict,
    tool_map: Dict[str, str],
    project_path: Path,
    *,
    parent_token: Optional[Any] = None,
    on_progress: Optional[Callable] = None,
) -> Dict:
    from rye.executor import PrimitiveExecutor

    name = tool_call["name"]
    item_id = tool_map.get(name)
    if not item_id:
        return {
            "id": tool_call["id"],
            "result": {"success": False, "error": f"Unknown tool: {name}"},
            "is_error": True,
        }

    tool_input = tool_call["input"]
    if isinstance(tool_input, dict):
        params = dict(tool_input)
    elif isinstance(tool_input, str):
        try:
            params = json.loads(tool_input)
        except (json.JSONDecodeError, TypeError):
            params = {"raw_input": tool_input}
    else:
        params = {}

    # Token propagation: inject parent token for directive execution
    # The child's execute() will attenuate based on its own declared permissions
    if parent_token is not None:
        # Only inject for directive execution (rye_execute calling thread_directive)
        # The item_id check ensures we only propagate to directive runner, not to
        # arbitrary tools (which don't understand _token)
        if _is_directive_execution(item_id, params):
            if hasattr(parent_token, "to_dict"):
                params["_token"] = parent_token.to_dict()
            elif isinstance(parent_token, dict):
                params["_token"] = parent_token

    executor = PrimitiveExecutor(project_path=project_path)
    result = await executor.execute(
        item_id=item_id,
        parameters=params,
        use_lockfile=True,
    )

    return {
        "id": tool_call["id"],
        "result": result.data if result.success else {"success": False, "error": result.error},
        "is_error": not result.success,
    }


def _is_directive_execution(item_id: str, params: Dict) -> bool:
    """Check if this tool call will execute a directive.

    Two cases:
    1. Direct call to thread_directive tool
    2. Call to rye_execute primary tool with item_type=directive
    """
    if "thread_directive" in item_id:
        return True
    if params.get("item_type") == "directive":
        return True
    return False
```

**`thread_directive.py` — callers of `_execute_tool_call`**

Both `_run_tool_use_loop()` and the parallel dispatcher must pass the token:

```python
# In _run_tool_use_loop / _dispatch_tool_calls_parallel:
tr = await _execute_tool_call(tc, tool_map, project_path, parent_token=harness.parent_token)
```

**`thread_directive.py` — `execute()`**

The self-minting at line 972 must be restricted to root-level invocations only:

```python
token = params.get("_token")
if token is None:
    # Only self-mint for root-level execution (no parent thread)
    if params.get("_parent_thread_id") is None:
        token = _mint_token_from_permissions(permissions, directive_name)
        harness.parent_token = token
    else:
        # Child execution without token = permission denied
        return {
            "status": "permission_denied",
            "error": f"Directive '{directive_name}' invoked as child but no capability token provided. "
                     f"Child directives must inherit capabilities from their parent.",
        }
```

#### Attenuation at the Child

When the child's `execute()` receives `_token`, the child **immediately attenuates** the token to the intersection of parent capabilities and child's declared permissions. This attenuated token becomes the child's effective token. If the child spawns grandchildren, it attenuates again from its own effective token. The harness's `check_permissions()` validates that the child's declared permissions are a subset of the attenuated token.

The invariant: **capabilities can only narrow going down the tree, never widen.**

```
orchestrator token: {rye.execute.tool.*, rye.search.*, rye.load.*}
    ├── child 1 (declares: rye.execute.tool.apps_task-manager_*)
    │   → attenuated: {rye.execute.tool.apps_task-manager_*}
    │   → check_permissions() passes (subset of parent)
    │
    └── child 2 (declares: rye.execute.tool.*, rye.sign.*)
        → attenuated: {rye.execute.tool.*}
        → rye.sign.* dropped (not in parent)
        → check_permissions() warns about dropped caps but proceeds
```

---

### 5. Hierarchical Cost Enforcement

#### Design

A shared cost ledger backed by the SQLite registry, with atomic budget checks before each child thread and each LLM turn.

The approach: **budget reservation**. Before threading a child, the orchestrator reserves a portion of its remaining budget. The child runs within that reserved budget. When the child completes, actual spend replaces the reservation.

#### Schema Extension

Add a `budget_ledger` table to the registry:

```sql
CREATE TABLE IF NOT EXISTS budget_ledger (
    thread_id TEXT NOT NULL,
    parent_thread_id TEXT,
    reserved_spend REAL NOT NULL DEFAULT 0.0,
    actual_spend REAL NOT NULL DEFAULT 0.0,
    max_spend REAL,
    status TEXT NOT NULL DEFAULT 'active',
    updated_at TEXT NOT NULL,
    PRIMARY KEY (thread_id),
    FOREIGN KEY (parent_thread_id) REFERENCES budget_ledger(thread_id)
);

CREATE INDEX IF NOT EXISTS idx_budget_parent
    ON budget_ledger(parent_thread_id);
```

#### Budget Operations

```python
class BudgetLedger:
    """Hierarchical budget enforcement via SQLite.

    Invariant: For any thread T with children C1..Cn:
        T.actual_spend + sum(Ci.reserved_spend for active Ci)
        + sum(Ci.actual_spend for completed Ci) <= T.max_spend

    All operations use transactions for atomicity.
    """

    def __init__(self, db_path: Path):
        self.db_path = db_path

    def register_budget(
        self, thread_id: str, parent_id: Optional[str], max_spend: float
    ) -> None:
        """Register a thread's budget. Called at thread creation."""
        ...

    def reserve(
        self, parent_id: str, child_id: str, amount: float
    ) -> bool:
        """Atomically reserve budget for a child.

        Returns False if reservation would exceed parent's remaining budget.
        Remaining = max_spend - actual_spend - sum(active children's reserved_spend)
        """
        conn = self._get_connection()
        try:
            # Single transaction: read parent budget, check remaining, insert child
            # Use IMMEDIATE with WAL to avoid long EXCLUSIVE locks across readers.
            conn.execute("BEGIN IMMEDIATE")

            parent = conn.execute(
                "SELECT max_spend, actual_spend FROM budget_ledger WHERE thread_id = ?",
                (parent_id,)
            ).fetchone()

            if not parent or parent["max_spend"] is None:
                conn.execute("ROLLBACK")
                return True  # No budget constraint

            # Sum active children's reservations
            children_reserved = conn.execute(
                "SELECT COALESCE(SUM(reserved_spend), 0) as total "
                "FROM budget_ledger WHERE parent_thread_id = ? AND status = 'active'",
                (parent_id,)
            ).fetchone()["total"]

            # Sum completed children's actual spend
            children_actual = conn.execute(
                "SELECT COALESCE(SUM(actual_spend), 0) as total "
                "FROM budget_ledger WHERE parent_thread_id = ? AND status != 'active'",
                (parent_id,)
            ).fetchone()["total"]

            remaining = parent["max_spend"] - parent["actual_spend"] - children_reserved - children_actual

            if amount > remaining:
                conn.execute("ROLLBACK")
                return False

            # Insert child reservation
            now = datetime.now(timezone.utc).isoformat()
            conn.execute(
                "INSERT INTO budget_ledger (thread_id, parent_thread_id, reserved_spend, max_spend, status, updated_at) "
                "VALUES (?, ?, ?, ?, 'active', ?)",
                (child_id, parent_id, amount, amount, now),
            )
            conn.execute("COMMIT")
            return True
        except Exception:
            conn.execute("ROLLBACK")
            raise
        finally:
            conn.close()

    def report_actual(self, thread_id: str, actual_spend: float) -> None:
        """Update actual spend and release reservation.

        Called when a child thread completes. Replaces reserved_spend
        with actual_spend, freeing any unused reservation back to the parent.
        """
        conn = self._get_connection()
        try:
            # Clamp actual_spend to reserved_spend (prevent child over-reporting)
            now = datetime.now(timezone.utc).isoformat()
            row = conn.execute(
                "SELECT reserved_spend FROM budget_ledger WHERE thread_id = ?",
                (thread_id,),
            ).fetchone()
            reserved = row["reserved_spend"] if row else 0.0
            applied = min(actual_spend, reserved)
            conn.execute(
                "UPDATE budget_ledger SET actual_spend = ?, reserved_spend = 0, "
                "status = 'completed', updated_at = ? WHERE thread_id = ?",
                (applied, now, thread_id),
            )
            conn.commit()
        finally:
            conn.close()

    def check_remaining(self, thread_id: str) -> Optional[float]:
        """Get remaining budget for a thread (considering children)."""
        conn = self._get_connection()
        try:
            row = conn.execute(
                "SELECT max_spend, actual_spend FROM budget_ledger WHERE thread_id = ?",
                (thread_id,)
            ).fetchone()

            if not row or row["max_spend"] is None:
                return None

            children_reserved = conn.execute(
                "SELECT COALESCE(SUM(reserved_spend), 0) as total "
                "FROM budget_ledger WHERE parent_thread_id = ? AND status = 'active'",
                (thread_id,)
            ).fetchone()["total"]

            children_actual = conn.execute(
                "SELECT COALESCE(SUM(actual_spend), 0) as total "
                "FROM budget_ledger WHERE parent_thread_id = ? AND status != 'active'",
                (thread_id,)
            ).fetchone()["total"]

            return row["max_spend"] - row["actual_spend"] - children_reserved - children_actual
        finally:
            conn.close()
```

#### Integration Points

**`thread_directive.py` — `execute()`**

After minting/receiving a token, register the budget:

```python
ledger = BudgetLedger(project_path / ".ai" / "threads" / "registry.db")
ledger.register_budget(
    thread_id=thread_id,
    parent_id=params.get("_parent_thread_id"),
    max_spend=limits.get("spend"),
)
```

Before threading each child (in `_execute_tool_call` when it's a directive execution):

```python
child_limits = child_directive.get("limits", {})
child_max = child_limits.get("spend")
if child_max is None:
    return {
        "status": "error",
        "error": f"Child directive '{child_directive_name}' missing max_spend in limits. "
                 f"Declare <limits max_spend=\"...\"> in directive metadata.",
    }

if not ledger.reserve(parent_thread_id, child_thread_id, child_max):
    return {
        "status": "budget_exceeded",
        "error": f"Cannot reserve ${child_max:.2f} for child '{child_directive_name}': "
                 f"parent budget exhausted. Remaining: ${ledger.check_remaining(parent_thread_id):.2f}",
    }
```

On child completion:

```python
ledger.report_actual(thread_id, harness.cost.spend)
```

Note: `report_actual` must only accept values <= the reservation (min(reserved, actual))
to prevent a malicious or buggy child from inflating spend and draining the parent budget.

**`safety_harness.py` — `check_limits()`**

Add a ledger check alongside the existing per-thread checks:

```python
# After checking local spend limit
if self._budget_ledger:
    remaining = self._budget_ledger.check_remaining(self._thread_id)
    if remaining is not None and remaining <= 0:
        return {
            "name": "limit",
            "code": "hierarchical_budget_exceeded",
            "remaining": remaining,
        }
```

**`safety_harness.py` — `increment_spawn_count()`**

Actually call this when threading children:

```python
# In _execute_tool_call, after confirming it's a directive execution:
harness.increment_spawn_count()
```

---

## Comparative Analysis: opencode Patterns

Lessons from studying [opencode](https://github.com/sst/opencode)'s agent runtime, mapped to Rye OS equivalents.

### Parallel Execution

opencode's `batch.ts` tool uses `Promise.all()` with a hard cap of 25 concurrent calls and disallows recursive batching (a batch tool call cannot itself invoke batch). Our `_dispatch_tool_calls_parallel` should similarly cap concurrency and forbid recursive thread-within-thread unless explicitly allowed. The grouping-by-`item_id` strategy already limits fan-out somewhat, but an absolute cap (e.g., `MAX_PARALLEL_CALLS = 25`) prevents degenerate cases where a single turn returns dozens of independent tool calls.

### Cancellation via Signal Propagation

opencode threads a single `AbortSignal` through LLM calls, tool calls, subprocess thread, and even retry sleeps — `cancel()` is just `abort.abort()`. Our poison-file approach (described in the resilience doc) is more filesystem-oriented but serves the same purpose. The key insight is that the signal must reach retry sleeps and managed subprocesses, not just the tool-use loop checkpoint. Specifically:

- **Retry sleeps**: `asyncio.sleep()` in retry loops should be wrapped with a cancellation check so a poison file or abort signal interrupts the wait immediately.
- **Managed subprocesses**: `managed_subprocess.stop()` should be called when the owning thread is cancelled, not just when it completes normally.
- **LLM streaming**: if the LLM provider supports mid-stream cancellation, the abort signal should propagate there too.

### Streaming Tool Execution

opencode streams LLM responses via Vercel AI SDK's `fullStream` async iterator, handling tool calls inline during streaming rather than batch-after-response. This reduces latency (tool execution begins before the full response is received) but adds complexity (partial tool-call parsing, interleaved events). Our batch-after-response approach is simpler and better suited to the transcript-based audit model where each event is a discrete record. Streaming tool execution would fragment transcript events and complicate replay.

### Session Status Simplicity

opencode uses only 3 states: `idle | busy | retry`. All detail (retry attempt, next retry time, error message) is metadata on the status object rather than separate enum values. This contrasts with our expanding status enum. Recommendation: keep registry status to `running | completed | error | suspended | cancelled` and put detail in structured metadata. See the Canonical Vocabulary section below for the authoritative status definitions.

---

## How It All Fits Together

### Orchestration Flow (Complete)

```
1. User calls: rye_execute(directive=build_crud_app, inputs={...})

2. thread_directive.execute():
   a. Load directive, validate metadata
   b. Mint root token from permissions
   c. Register budget in ledger (max_spend=$3.00)
   d. Create harness, register thread
   e. Emit transcript: thread_started {directive, model, limits}
   f. Start LLM loop

3. LLM Turn 1 — Scaffold:
   a. LLM calls rye_execute(scaffold_project, inputs={...})
   b. _execute_tool_call injects _token (attenuated)
   c. Budget ledger reserves $0.20 for scaffold child
   d. scaffold_project runs (sequential, within same turn)
   e. Child completes → ledger.report_actual($0.08)
   f. LLM receives scaffold result

4. LLM Turn 2 — Wave 1 (parallel):
   a. LLM calls thread_tool(plan_db_schema, execute_directive=true)
      → Budget reserves $0.80, token attenuated, task created
      → Emit transcript: child_thread_started {child_id, directive}
   b. LLM calls thread_tool(plan_api_routes, execute_directive=true)
      → Budget reserves $0.80, token attenuated, task created
      → Emit transcript: child_thread_started {child_id, directive}
   c. Both return immediately with thread_ids

5. LLM Turn 3 — Wait for Wave 1:
   a. LLM calls wait_threads([db_schema_id, api_routes_id])
   b. wait_threads awaits completion events (zero polling, zero LLM turns burned)
   c. Both children complete → ledger.report_actual for each
   d. Returns: {threads: {db_schema: {status: completed, cost: $0.45}, ...}}
   e. Remaining budget: $3.00 - $0.08 - $0.45 - $0.52 = $1.95

6. LLM Turn 4 — Wave 2:
   a. LLM calls thread_tool(plan_react_ui, execute_directive=true)
      → Budget reserves $0.80
   b. LLM calls wait_threads([react_ui_id])
   c. Completes → ledger.report_actual($0.61)

7. LLM Turn 5 — Wave 3 + Dev Server:
   a. LLM calls managed_subprocess(action=start, command="npm run dev",
      ready_pattern="ready on port \\d+", cwd="task-manager/")
      → Returns handle_id, waits for ready_pattern
   b. LLM calls thread_tool(plan_integration, execute_directive=true)
   c. LLM calls wait_threads([integration_id])
   d. LLM calls managed_subprocess(action=stop, handle_id=...)

8. LLM Turn 6 — Final:
   a. LLM returns final summary
   b. Emit transcript: thread_completed {status, cost}
      (If the thread suspends instead: emit thread_suspended {directive, suspend_reason, cost})
   c. thread_directive.execute() completes
   d. Budget ledger finalized: total actual = $2.14
   e. signal_completion() fires (coordination path)
```

### Thread Hierarchy (Runtime View)

```
build_crud_app-1739012630 (root, sonnet, budget=$3.00)
│
├── [hook: scaffold_project]       sequential, $0.08
│
├── [task: plan_db_schema]         ┐
│   └── SafetyHarness(budget=$0.80)│ Wave 1 — asyncio tasks
├── [task: plan_api_routes]        ┘   running concurrently
│   └── SafetyHarness(budget=$0.80)│
│                                  │
├── [task: plan_react_ui]          Wave 2 — after join
│   └── SafetyHarness(budget=$0.80)
│
├── [managed: npm-run-dev]         handle-based subprocess
│   └── pid=12345, pgid=12345
│
├── [task: plan_integration]       Wave 3 — with dev server running
│   └── SafetyHarness(budget=$0.80)
│
└── [managed: npm-run-dev → stopped]
```

### Data Flow for Budget Enforcement

```
                    ┌──────────────────────────────────────┐
                    │  budget_ledger (SQLite, exclusive tx) │
                    │                                      │
                    │  orchestrator: max=$3, actual=$0.08   │
                    │    ├── db_schema: reserved=$0.80      │
                    │    ├── api_routes: reserved=$0.80     │
                    │    └── remaining: $1.32               │
                    │                                      │
                    │  When db_schema completes:            │
                    │    db_schema: actual=$0.45, res=$0    │
                    │    → remaining increases by $0.35     │
                    └──────────────────────────────────────┘
```

The `BEGIN IMMEDIATE` transaction in `reserve()` prevents races: if two children try to reserve simultaneously, one blocks until the other commits, ensuring the remaining budget check is always consistent.

---

## New Files Summary

| File                                      | Purpose                                    | Dependencies                          |
| ----------------------------------------- | ------------------------------------------ | ------------------------------------- |
| `rye/agent/threads/managed_subprocess.py` | Start/stop/status for long-lived processes | subprocess, os, threading             |
| `rye/agent/threads/wait_threads.py`       | Blocking join for child threads            | asyncio, ThreadRegistry, thread_tool |
| `rye/agent/threads/budget_ledger.py`      | Hierarchical budget enforcement            | sqlite3, ThreadRegistry               |

## Modified Files Summary

| File                  | Changes                                                                                                                                       |
| --------------------- | --------------------------------------------------------------------------------------------------------------------------------------------- |
| `thread_directive.py` | Add `parent_token` param to `_execute_tool_call`, restrict self-minting to root-only, integrate budget ledger, call `increment_spawn_count()` |
| `thread_directive.py` | Add `_dispatch_tool_calls_parallel()`, replace serial for-loop in `_run_tool_use_loop`                                                        |
| `core_helpers.py`     | Same parallel dispatch change in `run_llm_loop`                                                                                               |
| `thread_tool.py`     | Add `execute_directive` mode with asyncio.Task creation, module-level `_active_tasks` registry                                                |
| `safety_harness.py`   | Add budget ledger reference, add hierarchical budget check in `check_limits()`                                                                |
| `thread_registry.py`  | Add `budget_ledger` table to schema init                                                                                                      |

## Capability Requirements

The orchestrator directive needs these additional capabilities declared:

```xml
<permissions>
  <!-- Existing -->
  <execute resource="rye" action="tool.*" />
  <execute resource="rye" action="search.*" />

  <!-- New -->
  <execute resource="rye" action="tool.managed_subprocess" />
  <execute resource="rye" action="tool.wait_threads" />
  <execute resource="rye" action="tool.thread_tool" />
</permissions>
```

Children inherit only the subset of these capabilities that their own directives declare.

## Testing Strategy

Each gap has independent test coverage:

1. **Parallel dispatch** — thread 3 tool calls, verify they overlap in time (measure wall-clock vs sum of durations)
2. **Managed subprocess** — start a `sleep 60` process, verify status, stop it, verify exit
3. **Wait/join** — thread 2 threads with known durations, wait, verify results arrive after both complete
4. **Token propagation** — thread child with narrower permissions, verify it cannot call tools outside its scope; verify child without `_token` from parent is rejected
5. **Budget enforcement** — set parent budget to $1.00, try to reserve $0.60 twice, verify second reservation fails; verify completed child frees reservation

---

## Comparative Analysis: opencode Patterns

Architectural patterns from the [opencode](https://github.com/opencode-ai/opencode) codebase (TypeScript/Bun) that informed the design above.

### Parallel Execution

opencode's `batch.ts` tool executes up to 25 tool calls via `Promise.all()`, disallowing recursive batching. Each call gets independent part tracking and timing. Our `_dispatch_tool_calls_parallel` uses `asyncio.gather()` with grouping by target `item_id` — same pattern, different runtime. Both cap concurrency and track per-call results.

### Cancellation via Signal Propagation

opencode threads a single `AbortSignal` through LLM calls, tool calls, subprocess thread, and retry sleeps. `cancel()` is `abort.abort()` — one call, everything stops. For subprocess cleanup, `Shell.killTree()` sends SIGTERM to the process group, waits 200ms, then SIGKILL.

Our poison-file approach (`cancel.requested`) is more durable (survives process crashes, works across unrelated processes) but less immediate. The hybrid: check poison files at loop checkpoints AND pass an `asyncio.Event` to retry sleeps and managed subprocess waits for faster interruption.

### Streaming vs. Batch Tool Execution

opencode streams LLM responses via Vercel AI SDK's `fullStream` async iterator, handling tool calls inline during streaming. Tool parts update in real-time as the model generates.

Our batch-after-response approach (parse complete response, then execute tools) is simpler and better suited to the transcript-based audit model where each event is a discrete JSONL record. The tradeoff: slightly higher latency, but cleaner state boundaries.

When a provider's config includes a `stream` section (sink-based streaming via the HTTP primitive), `cognition_out_delta` events are optionally emitted during the LLM call for real-time UI feedback. These are droppable events (fire-and-forget via `emit_droppable`). The complete `cognition_out` event is always emitted after the full response arrives, regardless of streaming support. See [Transcript Events](#transcript-events).

### Session Status Simplicity

opencode uses only 3 session states: `idle | busy | retry`. All detail (retry attempt count, next retry timestamp, error message) is structured metadata on the status object, not encoded in the status string.

This informed our decision to keep registry status to 5 values (`running | completed | error | suspended | cancelled`) with detail in `state.json` metadata rather than compound statuses like `suspended:limit`.

### Cost Tracking

opencode tracks cost per-step using `Decimal.js` for precision, handling provider-specific token counting (Anthropic excludes cached tokens from `inputTokens`; others include them). Cache read/write tokens and reasoning tokens are tracked separately.

Our `SafetyHarness.CostTracker` follows the same per-turn model. The hierarchical budget ledger (Gap 5) goes beyond opencode — they have no cross-session budget enforcement.

### Compaction and Pruning

opencode implements two-stage context management: (1) `prune()` erases old tool call outputs while keeping a recent window of ~40k tokens, and (2) compaction creates a dedicated summary message when token usage approaches the context limit. The pruned data stays on disk — only the model's input is reduced.

**Rye OS takes a different approach: compaction is not hardcoded in the harness.** It is achieved through optional directive-based hooks, consistent with the project's data-driven philosophy. The harness emits a `context_window_pressure` hook event when token usage approaches the limit; a user-defined hook directive does the actual summarization.

```xml
<hook event="context_window_pressure" when="pressure_ratio > 0.8">
  <directive>compaction_summarizer</directive>
</hook>
```

The harness pre-computes a `pressure_ratio` (0–1) in the event payload so hook `when` expressions stay simple (no arithmetic in the evaluator). The event fires with hysteresis — only when the ratio _crosses_ the threshold, not every turn — and a cooldown period after compaction prevents re-triggering loops.

**Applying compaction results.** The harness provides a generic "apply context patch" mechanism. The compaction hook directive returns a structured payload:

```python
{
    "compaction": {
        "summary": "Turns 1-15: established DB schema, implemented CRUD endpoints...",
        "prune_before_turn": 15,
    }
}
```

The tool-use loop applies the patch by reseeding the message list for the next LLM call — replacing older messages with the summary. The JSONL audit trail on disk is never affected; the transcript records `context_compaction_start` and `context_compaction_end` events (emitted by the hook directive, not by the harness) for post-hoc analysis. Pruning is a view-layer operation.

**Policy chooses _when_ and _how_; infrastructure provides a generic way to apply the result.** No compaction logic lives in the harness itself.

### Snapshot-Based Undo

opencode maintains a separate git repository per project to snapshot file state at each LLM step. This enables per-step diffs and revert. Our git integration (per-plan commits) serves a similar purpose at coarser granularity. For orchestration, per-wave snapshots could provide a rollback boundary if a wave fails.

---

## Canonical Vocabulary

Shared terminology across all orchestration docs. Authoritative definitions — other docs cross-reference this table.

### Thread Statuses

| Status      | Meaning                                              | Set By                          |
| ----------- | ---------------------------------------------------- | ------------------------------- |
| `running`   | Thread is actively executing LLM turns or tool calls | `thread_directive.execute()`    |
| `completed` | Thread finished successfully                         | `thread_directive.execute()`    |
| `error`     | Thread terminated due to unrecoverable error         | `thread_directive.execute()`    |
| `suspended` | Thread suspended, awaiting external action           | `safety_harness.check_limits()` |
| `cancelled` | Thread stopped by user or sibling failure            | `cancel_thread.execute()`       |

Suspended threads carry a `suspend_reason` in `state.json`:

| `suspend_reason` | Trigger                                       | Resume Path                               |
| ---------------- | --------------------------------------------- | ----------------------------------------- |
| `limit`          | `max_spend`, `max_turns`, or `max_tokens` hit | `resume_thread(action="bump_and_resume")` |
| `error`          | Transient error after max retries             | `resume_thread(action="resume")`          |
| `budget`         | Hierarchical budget exhausted                 | Parent bumps budget, then `resume_thread` |

### Tool Names

| Tool                 | Purpose                                          |
| -------------------- | ------------------------------------------------ |
| `thread_tool`       | Create and optionally start a child thread       |
| `wait_threads`       | Block until child threads complete               |
| `resume_thread`      | Resume a suspended or errored thread             |
| `cancel_thread`      | Request cancellation via poison file             |
| `managed_subprocess` | Start/stop/status for long-lived processes       |
| `bundler`            | Create, verify, inspect bundle manifests         |
| `budget_ledger`      | Hierarchical budget enforcement (internal)       |
| `orphan_detector`    | Scan for and recover orphaned threads (internal) |
| `error_classifier`   | Classify errors for retry decisions (internal)   |

### Hook Events

Hook events are **policy checkpoints** — the harness fires them, `evaluate_hooks()` checks for matching user-defined hooks, and unmatched events fall through to default behavior. See [Hooks as the Policy Layer](thread-resilience-and-recovery.md#architecture-hooks-as-the-policy-layer).

| Event                     | When Fired                                           |
| ------------------------- | ---------------------------------------------------- |
| `before_step`             | Before each LLM turn in the tool-use loop            |
| `after_step`              | After each LLM turn completes                        |
| `error`                   | When an error occurs (transient or permanent)        |
| `limit`                   | When a harness limit is exceeded                     |
| `after_complete`          | After the directive finishes successfully            |
| `context_window_pressure` | When token usage approaches the context window limit |

The `context_window_pressure` event payload includes pre-computed fields for simple `when` evaluation:

| Payload Field    | Type  | Description                      |
| ---------------- | ----- | -------------------------------- |
| `tokens_used`    | int   | Current token count              |
| `max_tokens`     | int   | Context window limit             |
| `pressure_ratio` | float | `tokens_used / max_tokens` (0–1) |

The event fires with **hysteresis**: only when `pressure_ratio` crosses the threshold (e.g., 0.79 → 0.81), not every turn. After a compaction hook runs, a cooldown suppresses re-triggering for at least one full turn.

### Limit Event Codes

Codes emitted by `check_limits()` when a limit is exceeded. Used by hooks (`when` expressions) and by `_handle_limit_hit()` for escalation. Mapped to limits dict keys by `_limit_code_to_key()`.

| Code                           | Limits Key | Trigger                                    |
| ------------------------------ | ---------- | ------------------------------------------ |
| `turns_exceeded`               | `turns`    | `cost.turns >= limits.turns`               |
| `tokens_exceeded`              | `tokens`   | `cost.tokens >= limits.tokens`             |
| `spend_exceeded`               | `spend`    | `cost.spend >= limits.spend`               |
| `spawns_exceeded`              | `spawns`   | `cost.spawns >= limits.spawns`             |
| `duration_exceeded`            | `duration` | `cost.duration_seconds >= limits.duration` |
| `hierarchical_budget_exceeded` | `spend`    | `budget_ledger.check_remaining() <= 0`     |

### Transcript Events

Transcript events are **audit records** written to `transcript.jsonl`. They may be emitted by infrastructure, default behaviors, or hook directives. They do not drive coordination — coordination uses `asyncio.Event` (see [Gap 3](#gap-3-push-based-coordination-replacing-polling)).

| Event                        | Producer            | Description                                                      | Criticality |
| ---------------------------- | ------------------- | ---------------------------------------------------------------- | ----------- |
| `thread_started`             | infrastructure      | Thread execution begins                                          | critical    |
| `thread_completed`           | infrastructure      | Thread finished successfully                                     | critical    |
| `thread_suspended`           | infrastructure      | Thread suspended (reason in state.json)                          | critical    |
| `thread_resumed`             | infrastructure      | Thread resumed from suspension                                   | critical    |
| `thread_cancelled`           | infrastructure      | Thread cancelled via poison file                                 | critical    |
| `step_start`                 | infrastructure      | LLM turn begins                                                  | critical    |
| `step_finish`                | infrastructure      | LLM turn ends                                                    | critical    |
| `cognition_out`              | infrastructure      | Complete cognition output (LLM response)                         | critical    |
| `cognition_out_delta`        | infrastructure      | Streaming cognition chunk (optional, requires `stream` config)   | droppable   |
| `cognition_reasoning`        | infrastructure      | Reasoning block (optional, requires `stream` in provider config) | droppable   |
| `tool_call_start`            | infrastructure      | Tool execution begins                                            | critical    |
| `tool_call_progress`         | tool (via callback) | Progress update for long-running tools                           | droppable   |
| `tool_call_result`           | infrastructure      | Tool execution completes                                         | critical    |
| `error_classified`           | default behavior    | Error classified for retry/fail decision                         | critical    |
| `retry_succeeded`            | default behavior    | Transient error resolved after retry                             | critical    |
| `limit_escalation_requested` | default behavior    | Limit hit, escalation sent to user                               | critical    |
| `child_thread_started`       | infrastructure      | Child thread spawned (audit, not coordination)                   | critical    |
| `child_thread_failed`        | infrastructure      | Child thread completed with error (audit)                        | critical    |
| `context_compaction_start`   | hook directive      | Compaction summarization begins                                  | critical    |
| `context_compaction_end`     | hook directive      | Compaction summarization ends                                    | critical    |

**Emission patterns:**

- **Critical events** are written synchronously — the tool-use loop blocks until the write completes. These are the events needed for crash recovery, replay, and debugging.
- **Droppable events** (deltas, progress, reasoning) may use fire-and-forget async emission via a centralized helper. The helper wraps `asyncio.create_task` with a done callback to consume exceptions silently, and falls back to sync write (or drop) if no event loop is running.

```python
def emit_droppable(transcript, thread_id: str, event_type: str, data: dict):
    """Fire-and-forget emission for non-critical audit events."""
    try:
        loop = asyncio.get_running_loop()
        task = loop.create_task(_safe_write(transcript, thread_id, event_type, data))
        task.add_done_callback(lambda t: t.exception() if not t.cancelled() else None)
    except RuntimeError:
        pass  # No running loop — drop the event

async def _safe_write(transcript, thread_id, event_type, data):
    try:
        transcript.write_event(thread_id, event_type, data)
    except Exception:
        pass  # Non-critical — silent failure
```

**Tool progress callbacks:**

Tools can optionally emit `tool_call_progress` events via an `on_progress` callback passed by the parallel dispatcher. Progress is **non-blocking by default** — transcript persistence uses `emit_droppable` (fire-and-forget) and is throttled (max 1 event/sec per `call_id`, or coarse milestones only: 0%, 25%, 50%, 75%, 100%) to avoid JSONL bloat.

**Streaming deltas:**

`cognition_out_delta` events are optional. They are gated on whether the provider config includes a `stream` section — the HTTP primitive supports streaming through sinks, and if the provider config declares sink configuration, the LLM loop emits delta events via `emit_droppable`. Code MUST NOT hardcode provider name checks (e.g., `if provider == "anthropic"`); all gating is data-driven from `provider_config.get("stream")`. The complete `cognition_out` event is always emitted regardless of streaming support. `cognition_reasoning` events follow the same pattern — emitted only when streaming is configured AND the response contains reasoning blocks.

### State File Locations

| File                                    | Contents                               |
| --------------------------------------- | -------------------------------------- |
| `.ai/threads/{id}/state.json`           | Harness state (cost, limits, hooks)    |
| `.ai/threads/{id}/transcript.jsonl`     | Append-only event log                  |
| `.ai/threads/{id}/cancel.requested`     | Poison file for cancellation           |
| `.ai/threads/{id}/escalation.json`      | Limit escalation request for approval  |
| `.ai/threads/{id}/processes/`           | Managed subprocess PID files           |
| `.ai/threads/registry.db`               | SQLite thread registry + budget ledger |
| `.ai/bundles/{bundle_id}/manifest.yaml` | Signed bundle manifest (SHA256 hashes) |
