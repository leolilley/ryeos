<!-- rye:signed:2026-02-22T02:41:03Z:c24cd4b0b1215b83130e0aa7f11d5258dcee1881211fd280ab54cb32c123ecef:E35l60qsszdMuVtxClcx7Nwe_73_s223qgxfuIeh6DJko2wO2DH83GLb5UByyrI-Bt1xeg1S7tadZx1HiEDQAA==:9fbfabe975fa5a7f -->

```yaml
id: spawning-patterns
title: Spawning Patterns
entry_type: pattern
category: rye/agent/threads
version: "1.0.0"
author: rye-os
created_at: 2026-02-18T00:00:00Z
tags:
  - spawning
  - threads
  - async
  - orchestration
references:
  - thread-lifecycle
  - limits-and-safety
  - "docs/orchestration/spawning-children.md"
```

# Spawning Patterns

How orchestrators spawn, coordinate, and collect results from child threads.

## Spawning a Child

All spawning goes through `thread_directive`:

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/thread_directive",
    parameters={
        "directive_name": "agency-kiwi/leads/discover_leads",
        "inputs": {"niche": "plumbers", "city": "Dunedin"},
        "limit_overrides": {"turns": 10, "spend": 0.10},
        "async_exec": True
    }
)
```

Returns:

```json
{
  "success": true,
  "thread_id": "agency-kiwi/leads/discover_leads-1739820456",
  "status": "running",
  "pid": 42857
}
```

## Synchronous vs Asynchronous

| Mode         | `async_exec` | Behavior                                      | Use When                        |
|--------------|-------------|-----------------------------------------------|---------------------------------|
| Synchronous  | `false`     | Blocks until child completes, returns result  | Need result before proceeding   |
| Asynchronous | `true`      | Returns `thread_id` immediately, child forks  | Spawning multiple parallel children |

### Async Internals

`os.fork()` duplicates the process. Child process:
1. Detaches via `os.setsid()`
2. Redirects stdio to `/dev/null`
3. Runs LLM loop to completion
4. Finalizes (report spend, update registry, write `thread.json`)
5. Calls `os._exit(0)`

Parent returns immediately with `thread_id` and `pid`.

## Parent Context Auto-Injection

The runner auto-injects parent context when dispatching `thread_directive` calls:

```python
if resolved_id == "rye/agent/threads/thread_directive":
    dispatch_params.setdefault("parent_thread_id", thread_id)
    dispatch_params.setdefault("parent_depth", orchestrator.get_depth(thread_id))
    dispatch_params.setdefault("parent_limits", harness.limits)
    dispatch_params.setdefault("parent_capabilities", harness._capabilities)
```

**The LLM never manually passes parent context** — it just calls `thread_directive` with directive name, inputs, and limit overrides.

Additionally, `RYE_PARENT_THREAD_ID` is set in the environment so forked children inherit the parent relationship.

## The Spawn-Wait-Collect Pattern

Standard orchestration pattern for parallel work:

```
Phase 1: Spawn children (async_exec: true)
         → collect all thread_ids

Phase 2: Wait for all children
         rye_execute(item_id="rye/agent/threads/orchestrator",
           parameters={"operation": "wait_threads",
                        "thread_ids": [...], "timeout": 300})

Phase 3: Aggregate results
         rye_execute(item_id="rye/agent/threads/orchestrator",
           parameters={"operation": "aggregate_results",
                        "thread_ids": [...]})
```

## Waiting for Children

### `wait_threads` Operation

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "wait_threads",
        "thread_ids": ["thread-1", "thread-2", "thread-3"],
        "timeout": 300
    }
)
```

`success` is `true` only if **all** threads completed successfully.

### Wait Internals

| Thread Type        | Mechanism                                              |
|--------------------|--------------------------------------------------------|
| In-process         | `asyncio.Event` — awaits `event.wait()` with timeout   |
| Cross-process      | Polls SQLite registry with exponential backoff (1s→10s) |
| Continuation chain | `resolve_thread_chain()` follows links to terminal thread |

Default timeout from `coordination.yaml` (typically 600s). Override with `timeout` parameter.

### `aggregate_results` Operation

Collects results for multiple thread IDs without waiting. Checks in-process results first, falls back to registry. Call after `wait_threads`.

### `get_status` Operation

Check single thread status. Resolution: in-process results → in-process events → registry lookup.

### `list_active` Operation

List all currently running in-process threads.

## Error Handling

**Child failures don't crash the parent.** Failed children return `status: "error"` with an error message. `wait_threads` returns `success: false` but the parent keeps running.

| Scenario                  | What Parent Sees                                          |
|---------------------------|-----------------------------------------------------------|
| Child exceeds turn limit  | `error: "Limit exceeded: turns_exceeded (10/10)"`         |
| Child exceeds spend limit | `error: "Limit exceeded: spend_exceeded (0.12/0.10)"`     |
| Child LLM call fails      | Error hooks evaluate → retry or terminate                 |
| Child tool call denied     | Permission error returned to child's LLM                  |
| Child cancelled            | Status becomes `cancelled`                                |
| Budget reservation fails   | Child never starts, `error: "Budget reservation failed"`  |

Robust orchestrators handle failures explicitly:

```
After wait_threads, check each result:
- Errored → log niche and error
- Completed → collect leads
- Continue with partial results — do NOT fail entire pipeline
```

## Thread Chains (Continuation)

If a child thread reaches its context limit, it's automatically handed off to a continuation thread. Waiting on the original `thread_id` still works — `resolve_thread_chain()` follows the chain to the terminal thread.

## Reading Child Transcripts

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={
        "operation": "read_transcript",
        "thread_id": "...",
        "tail_lines": 50
    }
)
```

## Killing Threads

```python
rye_execute(
    item_type="tool",
    item_id="rye/agent/threads/orchestrator",
    parameters={"operation": "kill_thread", "thread_id": "..."}
)
```

Sends `SIGTERM`, waits 3s for graceful shutdown, then `SIGKILL`.
