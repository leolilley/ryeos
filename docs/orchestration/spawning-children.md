```yaml
id: spawning-children
title: "Spawning Child Threads"
description: How to spawn, coordinate, and collect results from child threads
category: orchestration
tags: [threads, spawning, children, wait, coordination]
version: "1.0.0"
```

# Spawning Child Threads

An orchestrator directive spawns children by calling `thread_directive` — the same tool used to start any thread. The orchestrator's LLM decides when to spawn, what inputs to pass, and how much budget to allocate.

## Spawning a Child

From inside a directive's process steps, the LLM calls:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/leads/discover_leads",
        "inputs": {"niche": "plumbers", "city": "Dunedin"},
        "limit_overrides": {"turns": 10, "spend": 0.10},
        "async": True
    }
)
```

This spawns a child thread that:
- Runs the `discover_leads` directive
- Gets `niche` and `city` as interpolated inputs
- Is capped at 10 turns and $0.10 spend
- Runs asynchronously (parent continues immediately)

The response includes the `thread_id` that the parent uses to wait for and collect results:

```json
{
  "success": true,
  "thread_id": "agency-kiwi/leads/discover_leads-1739820456",
  "status": "running",
  "directive": "agency-kiwi/leads/discover_leads",
  "pid": 42857
}
```

## Synchronous vs Asynchronous

### Synchronous (default)

```python
# Blocks until child completes — result is returned directly
result = rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/leads/score_lead",
        "inputs": {"lead_id": "lead_001", "lead_data": "..."},
        "limit_overrides": {"turns": 4, "spend": 0.05}
    }
)
# result contains the child's full output
```

Use synchronous execution when the parent needs the result before proceeding — e.g., a strategy thread that decides the next step based on analysis.

### Asynchronous

```python
# Returns immediately with thread_id
result = rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/leads/discover_leads",
        "inputs": {"niche": "plumbers", "city": "Dunedin"},
        "limit_overrides": {"turns": 10, "spend": 0.10},
        "async": True
    }
)
thread_id = result["thread_id"]
```

Use asynchronous execution when spawning multiple children that can run in parallel. The parent collects all `thread_id`s and waits for them in batch.

**How async works internally:** `spawn_detached()` launches the child as a subprocess that re-executes `thread_directive.py` with `--thread-id` and `--pre-registered` flags. The child rebuilds all state from scratch (no inherited in-process state). Detached spawning uses the `rye-proc spawn` Rust binary for cross-platform support, with a POSIX `subprocess.Popen` fallback. The parent process returns immediately with the `thread_id` and `pid`.

## Parent Context Auto-Injection

When the runner dispatches a `thread_directive` tool call, it automatically injects parent context into the call parameters:

```python
# runner.py — auto-injection for child spawns
if resolved_id == "rye/agent/threads/thread_directive":
    dispatch_params.setdefault("parent_thread_id", thread_id)
    dispatch_params.setdefault("parent_depth", orchestrator.get_depth(thread_id))
    dispatch_params.setdefault("parent_limits", harness.limits)
    dispatch_params.setdefault("parent_capabilities", harness._capabilities)
```

This means the LLM never needs to manually pass parent context — it just calls `thread_directive` with the directive name, inputs, and limit overrides. The runner handles the rest.

Additionally, the parent thread sets `RYE_PARENT_THREAD_ID` in the environment before execution, so child subprocesses inherit the parent relationship automatically.

## The Spawn-Wait-Collect Pattern

This is the standard orchestration pattern for parallel work:

```
# In the orchestrator directive's process steps:

## Phase 3: Discover Leads

For each niche in the selected batch, spawn a discover_leads child thread:

rye_execute(item_type="tool", item_id="rye/agent/threads/thread_directive",
  parameters={
    "directive_name": "agency-kiwi/leads/discover_leads",
    "inputs": {"niche": "<niche>", "city": "Dunedin"},
    "limit_overrides": {"turns": 10, "spend": 0.10},
    "async": true
  })

Collect all thread_ids from the spawn results.

## Phase 4: Wait for Discovery

Wait for all discover_leads threads to complete:

rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
  parameters={
    "operation": "wait_threads",
    "thread_ids": ["<thread_id_1>", "<thread_id_2>", "..."],
    "timeout": 300
  })

Check results — any failures should be noted but not block the pipeline.

## Phase 5: Aggregate Results

Collect results from all completed threads:

rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
  parameters={
    "operation": "aggregate_results",
    "thread_ids": ["<thread_id_1>", "<thread_id_2>", "..."]
  })
```

## Waiting for Children

### `wait_threads` Operation

The `orchestrator` tool's `wait_threads` operation waits for multiple threads concurrently:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "wait_threads",
        "thread_ids": [
            "agency-kiwi/leads/discover_leads-1739820456",
            "agency-kiwi/leads/discover_leads-1739820457",
            "agency-kiwi/leads/discover_leads-1739820458"
        ],
        "timeout": 300
    }
)
```

**Response:**

```json
{
  "success": true,
  "results": {
    "agency-kiwi/leads/discover_leads-1739820456": {"status": "completed"},
    "agency-kiwi/leads/discover_leads-1739820457": {"status": "completed"},
    "agency-kiwi/leads/discover_leads-1739820458": {"status": "error", "error": "..."}
  }
}
```

`success` is `true` only if all threads completed successfully.

**How waiting works internally:**

- **In-process threads:** Each thread has an `asyncio.Event`. `wait_threads` awaits the event with `asyncio.wait_for(event.wait(), timeout)`. When `runner.run()` completes, it calls `complete_thread()` which sets the event.

- **Cross-process threads (async):** The child runs in a separate process — no shared event. `wait_threads` uses the `rye-watch` Rust binary for push-based file watching (inotify/FSEvents/ReadDirectoryChangesW) on the registry, with a 500ms polling fallback.

- **Continuation chains:** Before waiting, `resolve_thread_chain()` follows any `continued` → `continued` → ... links to find the terminal thread. This means if a thread was handed off, waiting on the original ID still works correctly.

**Default timeout** comes from `coordination.yaml` (typically 600 seconds / 10 minutes). Override with the `timeout` parameter.

### `aggregate_results` Operation

Collects results for multiple thread IDs without waiting:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "aggregate_results",
        "thread_ids": ["thread-1", "thread-2", "thread-3"]
    }
)
```

Checks in-process results first, then falls back to the registry. Useful after `wait_threads` returns to get the actual result data.

## Checking Status

### `get_status` Operation

Check a single thread's status:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "get_status",
        "thread_id": "agency-kiwi/leads/discover_leads-1739820456"
    }
)
```

Resolution order: in-process results → in-process events (still running) → registry lookup.

### `list_active` Operation

List all threads that are currently running in this process:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "list_active"}
)
```

Returns thread IDs for all in-process threads whose events haven't been set yet.

## Error Handling

**Child failures don't crash the parent.** When a child thread errors, its status is `error` and its result contains an error message. The parent's `wait_threads` call returns `success: false` (because not all children completed), but the parent thread keeps running and can inspect individual results.

A robust orchestrator handles failures explicitly:

```
## Phase 4: Check Discovery Results

After wait_threads completes, check each result:
- If a discovery thread errored, log the niche and error
- If a discovery thread completed, collect its leads
- Continue with whatever leads were successfully discovered

Do NOT fail the entire pipeline because one niche failed to scrape.
```

**Child errors that the parent sees:**

| Scenario | What happens |
|----------|-------------|
| Child exceeds turn limit | Child returns `error: "Limit exceeded: turns_exceeded (10/10)"` |
| Child exceeds spend limit | Child returns `error: "Limit exceeded: spend_exceeded (0.12/0.10)"` |
| Child's LLM call fails | Error hooks evaluate → retry or terminate |
| Child's tool call is denied | Permission error returned as tool result, LLM sees it |
| Child is cancelled | Status becomes `cancelled` |
| Budget reservation fails | Child never starts, returns `error: "Budget reservation failed"` |

## Reading Transcripts

The parent can read a child's full conversation log:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "read_transcript",
        "thread_id": "agency-kiwi/leads/discover_leads-1739820456",
        "tail_lines": 50  # optional: only last 50 lines
    }
)
```

## Killing Threads

For threads that need to be forcefully stopped (not just cancelled):

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "kill_thread",
        "thread_id": "agency-kiwi/leads/discover_leads-1739820456"
    }
)
```

Uses `rye-proc kill` (cross-platform Rust binary) to terminate the process, with a POSIX `os.kill` fallback. Sends `SIGTERM`, waits 3 seconds for graceful shutdown, then escalates to `SIGKILL` if the process is still alive.

## What's Next

- [Safety and Limits](./safety-and-limits.md) — How budget and limits work for child threads
- [Permissions and Capabilities](./permissions-and-capabilities.md) — How capabilities attenuate through the hierarchy
