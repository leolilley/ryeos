---
category: ryeos/core
tags: [reference, scheduler, cron, scheduling]
version: "1.0.0"
description: >
  The scheduler system — registering cron schedules, fire history,
  pause/resume, and the scheduling API.
---

# Scheduler

The scheduler allows registering items (directives, tools, graphs) to
execute on a recurring schedule. It provides full CRUD plus operational
controls.

## Scheduler Services

| Service                | Endpoint              | Description                    |
|------------------------|-----------------------|--------------------------------|
| `scheduler/register`   | `scheduler.register`  | Create or update a schedule    |
| `scheduler/list`       | `scheduler.list`      | List all registered schedules  |
| `scheduler/deregister` | `scheduler.deregister`| Remove a schedule              |
| `scheduler/pause`      | `scheduler.pause`     | Pause a schedule               |
| `scheduler/resume`     | `scheduler.resume`    | Resume a paused schedule       |
| `scheduler/show_fires` | `scheduler.show_fires`| Show fire history for a schedule |

All scheduler services require no capabilities (`required_caps: []`).

## CLI Verbs

```bash
ryeos scheduler register <spec>     # Create/update a schedule
ryeos scheduler list                # List all schedules
ryeos scheduler deregister <id>     # Remove a schedule
ryeos scheduler pause <id>          # Pause execution
ryeos scheduler resume <id>         # Resume after pause
ryeos scheduler show-fires <id>     # View fire history
```

## Registration

Register a schedule by providing:
- **Item ref** — canonical ref of the item to execute
- **Schedule spec** — cron expression or interval
- **Parameters** — input values for each execution
- **Project path** — which project context to run in

The daemon evaluates the schedule and fires executions at the
specified times. Each fire creates a new thread.

## Fire History

`ryeos scheduler show-fires <id>` returns the execution history:
- Fire timestamps
- Thread IDs for each execution
- Result status (completed, failed, cancelled)
- Duration and token usage

## Pause and Resume

Pausing a schedule stops new fires from being created. Existing
running threads are not affected. Resuming re-enables the schedule
starting from the next scheduled time.

## Use Cases

- **Periodic health checks** — run a diagnostic directive every hour
- **Data sync** — execute a sync tool on a cron schedule
- **Report generation** — generate daily/weekly reports via graph
- **Cleanup** — run maintenance tasks on a schedule
