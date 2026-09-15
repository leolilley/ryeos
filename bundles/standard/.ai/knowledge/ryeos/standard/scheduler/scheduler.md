<!-- ryeos:signed:2026-09-05T01:24:37Z:484a45898c683e6387517ecb7c6f455e0c4395e14770f53b79c95c6d59482811:p3oBFii2vUWqF8iKlGzBN+e8pS2mOKP5e68xPoY18++1cCtKGGvFTwYOC5oAGkgnt5SDFmEvG9kZiOjPBa9bDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
- **Capabilities** — a sorted, unique, non-empty subset of the caller's grant
- **Execution policy** — the ordinary explicit project, environment, target,
  lifecycle, and response policy
- **Project root** — absent for projectless work and required for project-backed work

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
capabilities:
  - ryeos.execute.graph.reports/hourly
execution_policy:
  schema_version: 2
  ownership: daemon_owned
  recovery: restart_recoverable
  response: accepted
  target:
    kind: here
  environment:
    kind: project_overlay
    include_operator_vault: true
    name_policy:
      kind: declared_required
  project:
    kind: live_direct
    access: read_write
    child_policy:
      kind: inherit
```

All fields are required except that `project_root` is omitted for an explicitly
projectless policy. Policies are authored behavior: the daemon does not infer
them or substitute defaults.

`live_direct` and `pinned` are separate supported lanes. Use `live_direct` when
each fire is intentionally meant to see the then-current live tree. Use
`pinned` with `source.kind: current_head` or an explicit snapshot hash when one
immutable project authority must survive admission and restart recovery.
Scheduled execution rejects `capture_live`, because capture is an interactive
admission operation rather than a recurring policy.

`live_direct` is never snapshotted implicitly: every fire, including recovery
of an interrupted fire, resolves the live filesystem again. A pinned fire does
the opposite. Its first admitted attempt binds the immutable generation and
every recovery rematerializes only that stored authority; it never falls back
to the live tree.

Each fire carries one daemon-authored, immutable execution context. Graph and
directive expressions read it as `execution.schedule`; direct subprocess tools
receive the same canonical JSON projection in the protected
`RYEOS_EXECUTION_CONTEXT` environment variable. Its `schedule` field is `null`
for ordinary nonscheduled execution. The context is separate from item
parameters and includes the schedule ID, fire ID, scheduled time, first durable
dispatch time, trigger reason, and exact schedule-spec hash. Recovery of the
same fire preserves these values. A recurring external observation should bind
`fire_id` into that observation's signed action input: the context is available
authority, not an implicit cache partition for every unrelated child effect.

The daemon evaluates the schedule and fires executions at the
specified times. Each fire creates a new thread.

## Fire History

`ryeos scheduler show-fires <id>` returns the execution history:
- Scheduled, reserved, dispatched, and completed timestamps
- Thread IDs for each execution
- Result status (completed, failed, cancelled)
- The schedule-spec hash, bound project authority, and admitted capsule hash

## Pause and Resume

Pausing a schedule stops new fires from being created. Existing
running threads are not affected. Resuming re-enables the schedule
starting from the next scheduled time.

## Use Cases

- **Periodic health checks** — run a diagnostic directive every hour
- **Data sync** — execute a sync tool on a cron schedule
- **Report generation** — generate daily/weekly reports via graph
- **Cleanup** — run maintenance tasks on a schedule
