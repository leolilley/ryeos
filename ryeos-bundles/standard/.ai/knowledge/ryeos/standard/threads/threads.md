---
category: ryeos/core
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
- **cancelled** — operator cancelled via `ryeos thread cancel <id>`

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

## HTTP Routes

| Method | Path                          | Auth          | Description            |
|--------|-------------------------------|---------------|------------------------|
| GET    | `/threads/{thread_id}`        | `ryeos_signed` | Thread detail         |
| GET    | `/threads/{thread_id}/events/stream` | `ryeos_signed` | SSE event stream |
| POST   | `/threads/{thread_id}/cancel` | `ryeos_signed` | Cancel thread         |

## Cancellation

Threads can be cancelled via `ryeos thread cancel <id>` or the HTTP
endpoint. The daemon sends SIGTERM to the subprocess, waits
`cancellation_grace_secs` (default 5s), then SIGKILL if still running.

Cancellation mode is configurable in `config/execution/execution.yaml`:
- `graceful` — SIGTERM → grace period → SIGKILL
- `immediate` — SIGKILL immediately
