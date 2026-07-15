<!-- ryeos:signed:2026-07-14T23:18:43Z:79d622c08de678f9500cc7b4d07ecdb42a557b4560d16924ba5d63931cf06742:Ke438Jv2hf1fGcGpSGUhdFn3BHaA+o8tVqcaKtbNISKJc9sM1y9EaHEW1bPW5gzjTU42vLggkq/IUeIhaUKRDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
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

All scheduler services require the caller to hold the corresponding
capability scope (e.g., `ryeos.execute.service.scheduler/register`).
Ownership is enforced: only the schedule's creator (or an admin) can
update, pause, resume, or deregister it.

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
- **Schedule ID** — stable identifier for update and operations
- **Item ref** — canonical ref of the item to execute
- **Schedule type and expression** — `cron`, `interval`, or `at`
- **Parameters** — an object passed to each execution
- **Timezone** — an explicit IANA timezone such as `UTC`
- **Misfire policy** — `skip`, `fire_once_now`,
  `catch_up_bounded:N`, or `catch_up_within_secs:S`
- **Overlap policy** — `allow`, `skip`, or `cancel_previous`
- **Lateness grace** — a positive number of seconds
- **Enabled state** — an explicit boolean
- **Project root** — optional project context for execution

Complete registration object:

```yaml
schedule_id: hourly-report
item_ref: graph:reports/hourly
schedule_type: cron
expression: "0 0 * * * *"
params:
  format: summary
timezone: UTC
misfire_policy: fire_once_now
overlap_policy: skip
lateness_grace_secs: 60
enabled: true
project_root: /path/to/project
```

All fields in this example except `project_root` are required. Policies are
authored behavior: the daemon does not infer them or substitute defaults.

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
