<!-- rye:signed:2026-03-16T09:53:44Z:d10e0c7be09f959d09f687883050313acc39ccc245db5be848c6c5a55f83d211:CEMcqPwcrsBUrvjkXrVGDrz1ErQhwRDBSX2s_8P9v7DBVAX5fbvsjUvCoGXkpR9tHnFElJ33mia5BJjy1Tb9AQ==:4b987fd4e40303ac -->
<!-- rye:unsigned -->

```yaml
name: processes
title: Process Management Tools
entry_type: reference
category: rye/core
version: "1.0.0"
author: rye-os
tags:
  - processes
  - cancel
  - status
  - sigterm
  - graph
  - management
```

# Process Management Tools

Three tools for managing running processes (graph runs and threads) via the thread registry.

## rye/core/processes/status

Check if a running process is alive.

| Param    | Required | Description                        |
| -------- | -------- | ---------------------------------- |
| `run_id` | yes      | The `graph_run_id` or `thread_id`  |

Looks up the PID from the thread registry and checks liveness via `SubprocessPrimitive.status(pid)`.

**Returns:**

```json
{
  "success": true,
  "run_id": "abc-123",
  "status": "running",
  "pid": 54321,
  "alive": true,
  "directive": "my/workflow",
  "created_at": "2026-03-10T04:00:00Z",
  "updated_at": "2026-03-10T04:00:05Z"
}
```

**Usage:**

```json
rye_execute(item_type="tool", item_id="rye/core/processes/status", parameters={"run_id": "abc-123"})
```

## rye/core/processes/cancel

Cancel a running process via SIGTERM.

| Param   | Required | Description                          |
| ------- | -------- | ------------------------------------ |
| `run_id`| yes      | The `graph_run_id` or `thread_id`    |
| `grace` | no       | Grace period in seconds (default: 5) |

Sends SIGTERM via `SubprocessPrimitive.kill(pid, grace)`, which triggers the walker's SIGTERM handler for clean graph shutdown with CAS state persistence. Updates the registry status to `"cancelled"`.

**Returns:**

```json
{
  "success": true,
  "run_id": "abc-123",
  "pid": 54321,
  "method": "sigterm"
}
```

**Usage:**

```json
rye_execute(item_type="tool", item_id="rye/core/processes/cancel", parameters={"run_id": "abc-123"})
rye_execute(item_type="tool", item_id="rye/core/processes/cancel", parameters={"run_id": "abc-123", "grace": 10})
```

## rye/core/processes/list

List processes from the thread registry.

| Param    | Required | Description                                                      |
| -------- | -------- | ---------------------------------------------------------------- |
| `status` | no       | Filter by status: `running`, `completed`, `cancelled`, `error`, `killed` |

Without a filter, returns all active (non-terminal) processes.

**Returns:**

```json
{
  "success": true,
  "runs": [
    {
      "run_id": "abc-123",
      "directive": "my/workflow",
      "status": "running",
      "pid": 54321,
      "parent_id": null,
      "created_at": "2026-03-10T04:00:00Z",
      "updated_at": "2026-03-10T04:00:05Z"
    }
  ],
  "count": 1
}
```

**Usage:**

```json
rye_execute(item_type="tool", item_id="rye/core/processes/list", parameters={})
rye_execute(item_type="tool", item_id="rye/core/processes/list", parameters={"status": "running"})
```

## SIGTERM-Based Cancellation

The `cancel` tool uses SIGTERM for clean process shutdown, replacing the old cancel-file polling mechanism.

### How It Works

1. **Handler registration** — When a walker starts, it registers a `signal.SIGTERM` handler that sets a `_shutdown_requested` flag on the walker instance
2. **Between-step check** — Between graph steps, the walker checks the `_shutdown_requested` flag
3. **Clean shutdown** — When the flag is set, the walker:
   - Persists current CAS state with status `"cancelled"`
   - Updates the thread registry entry
   - Writes a transcript event recording the cancellation
   - Exits the process
4. **Trigger** — `rye/core/processes/cancel` sends SIGTERM via `SubprocessPrimitive.kill(pid, grace)`, which triggers this handler

This approach is reliable and immediate — no filesystem polling delay, no stale cancel files.
