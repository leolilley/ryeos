<!-- ryeos:signed:2026-11-21T09:20:32Z:f5672bf0dabd15849161c4f5b20a325b086b8e695f28f3b7abc826a119469da4:DGQFzC646ldoEEmfyJzfk1FNEjXDCi+ksYA0EjGKLoyoDmC6G08HhVoKE7hXL71Zqaq10nEgZuCORDZk6PCYAw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [fundamentals, threads, execution, lifecycle]
version: "1.0.0"
description: >
  How threads work — lifecycle, events, trees, cancellation,
  and the thread API.
---

# Threads

Every execution in Rye OS runs in a **thread** — a tracked unit of work
with its own ID, event log, and lifecycle. Threads provide observability,
cancellation, and replay.

## Thread Lifecycle

```
created → running → completed
                 → failed
                 → cancelled
```

- **created** — thread registered, waiting to start
- **running** — subprocess active, events streaming
- **completed** — execution finished successfully, result captured
- **failed** — execution errored out
- **cancelled** — operator cancelled via `ryeos commands submit <id> cancel` or HTTP cancellation

## Thread IDs

Thread IDs are unique identifiers assigned at creation. They appear in:
- `RYE_THREAD_ID` environment variable (in subprocess tools)
- CLI output (`ryeos thread list`, `ryeos thread get`)
- HTTP API responses
- Event logs

## Event Log

Each thread maintains an append-only event log. Events include:
- `launched` — subprocess started
- `stdout_chunk` — output received (streaming tools)
- `tool_call` — tool invocation within a directive
- `tool_result` — tool return value
- `llm_request` / `llm_response` — model interaction
- `completed` / `failed` / `cancelled` — terminal events

### Which event means "done"

Runtime-authored events such as `graph_completed` mean the run ended; the
thread record may not have settled yet, because the runtime appends them
before it calls the daemon's finalize callback. The daemon's own terminal
events (`thread_completed`, `thread_failed`, `thread_cancelled`,
`thread_killed`, `thread_timed_out`, `thread_continued`) are committed in the
same transaction as the thread's terminal snapshot and published only after
it. Once one is visible, `threads/get` returns the terminal status and the
result. Clients that need the result key on the daemon's `thread_*` event (or
on `threads/get` leaving `running`), never on a runtime event.

Events can be replayed:
- `ryeos events replay <thread_id>` — replay a single thread
- `ryeos events chain-replay <thread_id>` — replay entire chain

## Thread Trees

Threads can have parent-child relationships:
- A directive can **fork** sub-threads for parallel work
- Parent threads can wait for, cancel, or aggregate child results
- The tree is traversed via `ryeos thread children` and `ryeos thread chain`

## Thread API

| Verb                    | Description                             |
|-------------------------|-----------------------------------------|
| `ryeos thread list`     | List all threads                        |
| `ryeos thread get <id>` | Get thread detail + result              |
| `ryeos thread tail <id>` | Tail thread events (live SSE)          |
| `ryeos thread children <id>` | List direct children            |
| `ryeos thread chain <id>` | Get full parent chain                |
| `ryeos events replay <id>` | Replay persisted events             |
| `ryeos events chain-replay <id>` | Replay chain events        |

### Retained project results

`ryeos thread get <exact-thread-id>` exposes
`thread.result_project_snapshot_hash` alongside the thread's admitted
`project_authority`. It is the retained generation recorded by that exact
thread's immutable snapshot, or JSON null when no result was retained.

The query does not follow a continuation, select a later execution, advance
project HEAD, or infer a result from snapshot history. Retain-result execution
can therefore leave ordinary `snapshot log` unchanged while its private
result remains available. Thread reads retain their existing owner check.
Thread-list fields remain rebuildable display projections; use the exact
thread-detail query before selecting a retained result for qualification.

This coordinate is a locator, not publication or import permission. A retained
result import still verifies the exact chain/thread, successful terminal
authority, caller ownership, retained-result policy and requested snapshot.

## HTTP Routes

| Method | Path                          | Auth          | Description            |
|--------|-------------------------------|---------------|------------------------|
| GET    | `/threads/{thread_id}`        | `ryeos_signed` | Thread detail         |
| GET    | `/threads/{thread_id}/events/stream` | `ryeos_signed` | SSE event stream |
| POST   | `/threads/{thread_id}/cancel` | `ryeos_signed` | Cancel thread         |

## Cancellation

Threads can be cancelled via `ryeos commands submit <id> cancel` or the HTTP
`POST /threads/{thread_id}/cancel` endpoint. The daemon sends SIGTERM to the subprocess, waits
`cancellation_grace_secs` (default 5s), then SIGKILL if still running.

Cancellation mode is configurable in `config/execution/execution.yaml`:
- `graceful` — SIGTERM → grace period → SIGKILL
- `immediate` — SIGKILL immediately

## Remote thread inspection

Use `ryeos remote threads` and `ryeos remote thread-status` to inspect threads on another node. See [Remote Command Reference](../../core/remote/remote-command-reference.md).
