<!-- rye:signed:2026-02-26T05:52:24Z:175fb2d452df6146866e71baef0ec23e9170eca01dfda32bc7dcc11a16271d18:1I7lraR6mLyn2lSoLKCsL92U87-cKipsO9V03KV30foAorscfNxsSb7BQiURoYSOEpgxyVB8GHZdP1RHeWQzBQ==:4b987fd4e40303ac -->
```yaml
name: spawning-patterns
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

All spawning goes through `execute directive`:

```python
rye_execute(
    item_type="directive",
    item_id="agency-kiwi/leads/discover_leads",
    parameters={"niche": "plumbers", "city": "Dunedin"},
    async=True,
    limit_overrides={"turns": 10, "spend": 0.10}
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

| Mode         | `async` | Behavior                                      | Use When                        |
|--------------|-------------|-----------------------------------------------|---------------------------------|
| Synchronous  | `false`     | Blocks until child completes, returns result  | Need result before proceeding   |
| Asynchronous | `true`      | Returns `thread_id` immediately, child spawns as subprocess | Spawning multiple parallel children |

### Async Internals

`spawn_detached()` delegates to `SubprocessPrimitive.spawn()`, which calls `lillux-proc spawn` (cross-platform Rust binary). No POSIX fallbacks — lillux-proc is a hard dependency. Child process:
1. Runs as a detached subprocess (`__main__` with `--thread-id` and `--pre-registered` flags)
2. Runs LLM loop to completion
3. Finalizes (report spend, update registry, write `thread.json`)
4. Exits

Parent returns immediately with `thread_id` and `pid`.

## Parent Context Auto-Injection

When `execute directive` spawns a thread, the runner internally delegates to `thread_directive` and auto-injects parent context:

```python
if resolved_id == "rye/agent/threads/thread_directive":
    dispatch_params.setdefault("parent_thread_id", thread_id)
    dispatch_params.setdefault("parent_depth", orchestrator.get_depth(thread_id))
    dispatch_params.setdefault("parent_limits", harness.limits)
    dispatch_params.setdefault("parent_capabilities", harness._capabilities)
```

**The LLM never manually passes parent context** — it just calls `execute directive` with the directive ID, inputs, and limit overrides. The internal delegation to `thread_directive` is transparent.

Additionally, `RYE_PARENT_THREAD_ID` is set in the environment so spawned children inherit the parent relationship.

## The Spawn-Wait-Collect Pattern

Standard orchestration pattern for parallel work:

```
Phase 1: Spawn children (async=True)
         rye_execute(item_type="directive", item_id="domain/discover",
           parameters={...}, async=True, limit_overrides={...})
         → collect all thread_ids

Phase 2: Wait for all children
         rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
           parameters={"operation": "wait_threads",
                        "thread_ids": [...], "timeout": 300})

Phase 3: Aggregate results
         rye_execute(item_type="tool", item_id="rye/agent/threads/orchestrator",
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
| Cross-process      | Push-based `lillux-watch` on registry.db with 500ms polling fallback |
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

Delegates to `SubprocessPrimitive.kill()`, which calls `lillux-proc kill` (graceful→force). No POSIX fallbacks.
