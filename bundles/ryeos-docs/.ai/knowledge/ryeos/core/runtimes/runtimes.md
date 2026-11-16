<!-- ryeos:signed:2026-11-16T04:15:12Z:d6979f2f55fcd4f96237f937ad9c6166e2977a3f82fd425b2f545f1366187790:wwfydoD7z2Wd0s7fef1d4ILBjbvEm6btzzheklz8RSN8DKwnJGcixQ3PSR+jjMpr9zQ1DyUL5+onCWkvmy99BA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core
tags: [reference, runtimes, execution, subprocess]
version: "1.3.0"
description: >
  Active tool runtime descriptors and subprocess execution helpers.
---

# Runtimes

Runtimes are execution environments. Core provides generic tool helper
descriptors and the subprocess executor; the directive, graph and knowledge
bundles each provide their runtime binary.

## Tool Runtimes (Core Bundle)

### Python Function (`tool:ryeos/core/runtimes/python/function`)
Loads a Python module and calls its `execute(params, project_path)`.

- **Interpreter:** `RYE_PYTHON` override → engine-resolved `python3`. Project
  virtualenvs are not an implicit execution dependency.
- **Imports:** prepends runtime-derived bundle-local roots to `sys.path`
- **Async support:** Yes (auto-detects and wraps with `asyncio.run`)
- **Timeout:** 300s
- **Use case:** Structured Python tools with function entry point

### Python Script (`tool:ryeos/core/runtimes/python/script`)
Runs a Python script as `__main__`.

- **Command:** runtime launcher invokes `${tool_path}` as `__main__` with `--project-path ${project_path}`
- **Same interpreter and bundle-local `sys.path` setup as function runtime**
- **Timeout:** 300s
- **Use case:** Self-contained Python scripts

Shell commands run through `tool:ryeos/core/subprocess/execute`, and
graph workflows run through `runtime:graph-runtime` in the graph bundle.

## Execution Config

All runtimes inherit defaults from `config:execution/execution`:

| Setting                  | Default       |
|--------------------------|---------------|
| `timeout`                | 86400s (1 day)|
| `max_steps`              | 100           |
| `max_concurrency`        | 10            |
| `cancellation_mode`      | graceful      |
| `cancellation_grace_secs`| 5             |

Override at project level in `.ai/config/execution/execution.yaml`.

`cancellation_mode` and `cancellation_grace_secs` are the stop policy for
every launch, runtimes included. They are resolved per item and recorded in
the launch metadata when the process attaches; a stop uses the recorded
policy, with the grace capped at the node's 5-second bound. A graceful stop
sends SIGTERM to the runtime process only. The runtime owns stopping its own
children (forward SIGTERM, or stop them at its own boundary); at the grace
deadline the node SIGKILLs the whole process group, which reaches any child
still running.

Per-item overrides are kind-keyed so the policy format extends without an
engine code change:

```yaml
items:
  tool:
    my/project/action:
      timeout: 600
      max_steps: 20
```

## The `@subprocess` Alias

When a tool declares `executor_id: "@subprocess"`, it resolves to
`tool:ryeos/core/subprocess/execute`. This is the terminal subprocess
spawner that actually forks and execs the configured command.

The chain looks like:
```
Tool action → @subprocess → subprocess/execute → fork + exec → result
```

## Runtime implementation ownership

Active interpolation, conditions, graph traversal, and resume behavior
live in Rust runtimes and engine crates.

## Native resume state ownership

Declaring `native_resume` tells the daemon that the exact admitted runtime can
reconstruct its own state after the same thread's process dies. It does not
declare that every runtime uses a filesystem checkpoint. The daemon preserves
the admitted execution closure, exact thread, project/workspace authority,
launch policy, and bounded retry budget; it sets the typed resume launch mode
only after proving the old process dead. The runtime remains responsible for
interpreting its own already-durable state and refusing incomplete or
contradictory recovery evidence.

A runtime may use the daemon-owned checkpoint directory, an authoritative
event braid, or another explicitly admitted durable source. It must have one
reconstruction authority, be idempotent across repeated recovery launches, and
never treat process memory or a callback transport error as proof that an
effect did not happen. RyeOS does not manufacture an empty checkpoint merely
to make a runtime resume-eligible.

A machine continuation (a successor thread after a limit or context-window
cut-off) defaults to resuming from the predecessor's checkpoint and fails if
none exists. A runtime that rebuilds from the event braid declares
`replay_continuation: true`; its successor then cold-starts, with no
checkpoint copy and no `RYEOS_RESUME=1`, and replays its predecessor chain.

### Nonterminal process control

`ryeos.runtime.process-control.v1` is the closed stdout control contract for a
managed runtime that must stop without claiming a thread terminal. It is
deliberately separate from `RuntimeResult`; a successful process emits exactly
one JSON value in one domain or the other. The only current control value is:

```json
{
  "process_outcome": "recovery_required",
  "schema": "ryeos.runtime.process-control.v1",
  "thread_id": "T-...",
  "reason": "retained_progress_outcome_unknown"
}
```

The executor requires the reported thread to equal the exact active thread,
requires admitted native-resume authority and a captured resume ledger, leaves
the thread nonterminal, atomically rotates the launch claim within the signed
resume-attempt ceiling, and relaunches that same thread. An exhausted ceiling
settles a typed terminal failure. A concurrent durable stop wins instead of
recovery. Exit status and stderr never create this authority, and an unknown
field, control value, schema, reason, malformed thread ID, mismatched thread,
or control request without native-resume authority refuses rather than falling
through as a terminal result.

The directive runtime is event-backed: its same-thread resume folds the exact
continuation path and current thread's durable cognition, usage, tool-start,
and tool-result events. The graph runtime instead uses its versioned checkpoint
commit fence. Both use the same generic daemon native-resume lifecycle while
retaining different runtime-owned state contracts.
