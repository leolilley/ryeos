# Push-Based Thread Coordination

> Event-driven coordination via `asyncio.Event` — zero polling, zero latency.
>
> **Location:** `rye/rye/.ai/tools/rye/agent/threads/`
>
> **Configuration:** See [data-driven-coordination-config.md](data-driven-coordination-config.md) for timeout settings, event retention, and coordination policies.

## Overview

Replaces all polling-based coordination with push-based `asyncio.Event` signals. Threads signal completion immediately; `wait_threads` blocks efficiently without consuming tokens or burning CPU.

## Architecture

### Dual-Path Design

```
┌─────────────────────────────────────────────────────────────┐
│                    Thread Execution                         │
│                                                             │
│  ┌─────────────────────┐     ┌─────────────────────────┐   │
│  │ Coordination Path   │     │ Audit Path              │   │
│  │ (asyncio.Event)     │     │ (transcript JSONL)      │   │
│  │                     │     │                         │   │
│  │ • Instant signals   │     │ • Durable records       │   │
│  │ • In-process only   │     │ • For replay/debug      │   │
│  │ • Zero latency      │     │ • Written at checkpoints│   │
│  └─────────────────────┘     └─────────────────────────┘   │
└─────────────────────────────────────────────────────────────┘
```

**Key Invariant:** Coordination signals never flow through the transcript. Events are for audit; events are not a coordination mechanism.

## Components

### Completion Events

Each thread has an `asyncio.Event` that signals terminal state:

```python
# thread_tool.py (module level)
_completion_events: Dict[str, asyncio.Event] = {}
_active_tasks: Dict[str, asyncio.Task] = {}

def get_completion_event(thread_id: str) -> asyncio.Event:
    """Get or create completion event for a thread."""
    if thread_id not in _completion_events:
        _completion_events[thread_id] = asyncio.Event()
    return _completion_events[thread_id]

def signal_completion(thread_id: str) -> None:
    """Signal that thread reached terminal state.

    Called from thread_directive.execute() finally block.
    Wakes up any wait_threads blocked on this thread.
    """
    event = _completion_events.get(thread_id)
    if event:
        event.set()
```

### Wait Threads Tool

Blocks on completion events — zero polling:

```python
async def wait_threads(
    thread_ids: List[str],
    timeout: float = config.wait_threads.default_timeout (default: 600s),
    fail_fast: bool = False,
    **params,
) -> Dict[str, Any]:
    """Wait for threads to reach terminal state.

    Push-based coordination via asyncio.Event:
    Each child thread sets an asyncio.Event on completion.
    This function awaits those events directly — zero polling.
    """
    from thread_tool import get_task, get_completion_event

    results = {}
    in_process = {}

    # All threads must be in-process (no polling fallback)
    for tid in thread_ids:
        task = get_task(tid)
        if task is not None:
            in_process[tid] = get_completion_event(tid)
        else:
            results[tid] = {
                "status": "error",
                "error": f"No active task for thread {tid}"
            }

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
                    }
                except Exception as e:
                    return tid, {"status": "error", "error": str(e)}
            return tid, {"status": "unknown"}

        waiters = [wait_for_thread(tid, ev) for tid, ev in in_process.items()]

        if fail_fast:
            # Return as soon as any completes
            for coro in asyncio.as_completed(waiters):
                tid, result = await coro
                results[tid] = result
                if result.get("status") == "error":
                    break
        else:
            # Wait for all
            done, _ = await asyncio.wait(
                [asyncio.create_task(w) for w in waiters],
                timeout=timeout if timeout > 0 else None,
            )
            for coro in done:
                tid, result = coro.result()
                results[tid] = result

    return {
        "success": all(r.get("status") == "completed" for r in results.values()),
        "threads": results,
    }
```

## Lifecycle

### Thread Completion Guarantee

```python
async def execute(...):
    """Thread execution with guaranteed completion signal."""
    thread_id = generate_thread_id()

    # Pre-create completion event before task starts
    get_completion_event(thread_id)

    try:
        # Run the thread
        result = await _run_thread_logic(...)
        return result
    finally:
        # ALWAYS signal completion, regardless of outcome
        signal_completion(thread_id)
        remove_task(thread_id)
```

**Terminal States:**

- `completed` - Success
- `error` - Unrecoverable error
- `cancelled` - User or sibling cancellation
- `suspended` - Awaiting approval/resume

All trigger `signal_completion()`.

## What Was Removed

### ❌ Transcript Polling

**Old approach (REMOVED):**

```python
# REMOVED - transcript_watcher.py polling
while True:
    events = watcher.poll(thread_id)
    if check_terminal_state(events):
        break
    await asyncio.sleep(2)  # Polling delay
```

**Why removed:**

- Wastes tokens if LLM polls
- Latency if tool polls (up to poll_interval delay)
- Blind to early failures
- Complex state reconstruction from transcript

### ❌ Registry Polling

**Old approach (REMOVED):**

```python
# REMOVED - registry polling loop
while True:
    status = registry.get_status(thread_id)
    if status in TERMINAL_STATES:
        break
    await asyncio.sleep(poll_interval)
```

**Why removed:**

- SQLite reads on every poll
- No immediate failure notification
- Race conditions between poll and completion

## Benefits

| Metric            | Polling             | Push-Based     |
| ----------------- | ------------------- | -------------- |
| Latency           | Up to poll_interval | Zero           |
| Token Cost        | N × poll turns      | Zero           |
| CPU Usage         | Continuous          | Idle           |
| Failure Detection | Delayed             | Immediate      |
| Complexity        | Polling loops       | Event handlers |

## Testing

```python
class TestPushBasedCoordination:
    test_completion_event_created_before_task
    test_signal_completion_fires_on_success
    test_signal_completion_fires_on_error
    test_signal_completion_fires_on_cancel
    test_signal_completion_fires_on_suspend
    test_wait_threads_blocks_until_completion
    test_wait_threads_returns_immediate_for_completed
    test_wait_threads_no_polling
    test_fail_fast_returns_on_first_error
    test_unknown_thread_returns_error_no_poll
```

## Configuration

```yaml
# rye/rye/.ai/tools/rye/agent/threads/config/coordination.yaml
schema_version: "1.0.0"

coordination:
  # No polling configuration needed!
  # Pure event-driven

  wait_threads:
    default_timeout: 600 # seconds
    max_timeout: 3600 # 1 hour hard limit

  cleanup:
    remove_events_after_minutes: 60 # Cleanup old events
```
