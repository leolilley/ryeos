---
category: "ryeos/concepts"
name: "threads"
description: "Thread lifecycle, state model, and execution patterns"
---

# Threads

A thread is a running execution. Every `execute` call creates or resumes a thread.

## Lifecycle

```
created → running → completed
                  → failed
                  → cancelled
```

1. **Created** — the daemon receives an execute request, resolves the item, builds a plan
2. **Running** — the daemon spawns the runtime subprocess, which calls back for tool dispatches
3. **Completed** — the runtime returns a result, the daemon writes the final state
4. **Failed** — an error occurred (runtime crash, timeout, limit exceeded, verification failure)
5. **Cancelled** — the operator cancelled the thread

## State model

Thread state is stored in an append-only CAS (content-addressed storage) chain:

- Every state transition is written as a CAS object
- Each transition is signed with the node identity key
- State includes: thread ID, item ref, parameters, turn history, token counts, tool results
- The chain is append-only — no mutations, only new entries

This means:
- State survives process crashes (the daemon reads the chain on restart)
- Threads can be resumed from checkpoints
- Every state transition is auditable
- The CAS chain is garbage-collectable when the thread is no longer needed

## Execution patterns

### Inline execution (default)

```
rye_execute(item_id="tool:my/tool", thread="inline")
```

The tool runs in the current thread context. The caller blocks until the tool returns. This is the default mode for tools.

### Forked execution

```
rye_execute(item_id="directive:my/workflow", thread="fork")
```

The directive runs as a separate thread with its own LLM loop. The caller receives a `thread_id` and can:
- Poll for completion
- Wait for the thread
- Cancel the thread
- Resume the thread later

Forked threads have their own:
- LLM conversation context
- Turn/token/spend limits
- Model selection
- Permission scope (attenuated from parent)

### Async execution

```
rye_execute(item_id="tool:my/tool", async=true)
```

Returns immediately with a `thread_id`. The tool runs in the background. Check status via `thread-get` or `thread-list`.

## Thread resumption

Threads with `native_resume` policy can be resumed after crashes:

1. The daemon detects threads left in non-terminal states
2. On restart, it checks if the subprocess is dead
3. Dead threads are finalized as failed
4. Threads with `native_resume` policy generate `ResumeIntent`s
5. The daemon re-spawns the runtime with resume context

## Thread hierarchy

Threads can spawn child threads:

```
Parent (orchestrator)
  ├── Child 1 (worker)
  ├── Child 2 (worker)
  └── Child 3 (sub-orchestrator)
        ├── Grandchild 1
        └── Grandchild 2
```

Properties:
- Budgets cascade: children cannot exceed the parent's allocated budget
- Capabilities attenuate: each level has equal or fewer permissions than its parent
- Each thread writes its own transcript
- Parent can wait for all children, cancel individual children

## Thread state storage

| Path | Contents |
|---|---|
| `<state_dir>/.ai/state/runtime.sqlite3` | Thread metadata, event history |
| `<state_dir>/.ai/state/objects/` | CAS objects (state transitions) |
| `<state_dir>/.ai/state/refs/` | CAS references |
| `<state_dir>/.ai/state/trace-events.ndjson` | Structured trace log |

## CLI commands

```bash
ryeos thread-list                  # List all threads
ryeos thread-get <id>              # Get thread details
ryeos thread-chain <id>            # Show state chain
ryeos thread-tail <id>             # Tail thread output
ryeos thread-cancel <id>           # Cancel a running thread
ryeos thread-children <id>         # List child threads
```
