<!-- ryeos:signed:2026-11-14T05:54:19Z:5c960b1c4f28e72a8d45ea2eb2dc5ccc4514745ca8ec8e0e8b332f0b761adbb0:0vgkCVwak01dO3hKUEMR4lOiTmZ1QhXP8/wFWMKI2ZMrcb3FghUparX+0AYP6vZ/W9nOmCUjh9HzsJBPLD3/Aw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
tags: [reference, scheduler, cron, scheduling]
version: "1.0.0"
description: >
  The scheduler system — registering cron schedules, fire history,
  pause/resume, and the scheduling API.
---

# Scheduler

The independently selected scheduling capability registers capsule-producing
subprocess tools for one-shot or recurring execution. In-process services and
method dispatch are not eligible dispatch targets. Selecting scheduling grants
no execution authority; authenticated registration and each fire still require
their exact capability grants.

## Scheduler Services

| Service                | Endpoint              | Description                    |
|------------------------|-----------------------|--------------------------------|
| `scheduler/register`   | `scheduler.register`  | Create or update a schedule    |
| `scheduler/list`       | `scheduler.list`      | List all registered schedules  |
| `scheduler/deregister` | `scheduler.deregister`| Remove a schedule              |
| `scheduler/pause`      | `scheduler.pause`     | Pause a schedule               |
| `scheduler/resume`     | `scheduler.resume`    | Resume a paused schedule       |
| `scheduler/reauthorize` | `scheduler.reauthorize` | Re-pin a schedule to the owner's current grant generation |
| `scheduler/explain`     | `scheduler.explain`   | Explain schedule timing and policy |
| `scheduler/show_fires` | `scheduler.show_fires`| Show fire history for a schedule |

Mutating scheduler services declare the corresponding capability scope
(e.g., `ryeos.execute.service.scheduler/register`). Inspection descriptors
for list, explain and show_fires declare no additional required_caps; their
authenticated caller and ownership filtering still follow the service contract.
Ownership is enforced: only the schedule's creator (or an admin) can
update, pause, resume, or deregister it.

## CLI Verbs

```bash
ryeos scheduler register <spec>     # Create/update a schedule
ryeos scheduler list                # List all schedules
ryeos scheduler deregister <id>     # Remove a schedule
ryeos scheduler pause <id>          # Pause execution
ryeos scheduler resume <id>         # Resume after pause
ryeos scheduler reauthorize <id>    # Re-pin to the current grant generation
ryeos scheduler explain <id>        # Inspect timing and policy
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
schedule_id: hourly-doctor
item_ref: tool:ryeos/core/node/doctor
schedule_type: cron
expression: "0 0 * * * *"
params:
  source: /path/to/project
timezone: UTC
misfire_policy: fire_once_now
overlap_policy: skip
lateness_grace_secs: 60
enabled: true
project_root: /path/to/project
capabilities:
  - ryeos.execute.tool.ryeos/core/node/doctor
  - ryeos.read.project.live
execution_policy:
  schema_version: 2
  ownership: daemon_owned
  recovery: restart_recoverable
  response: accepted
  target:
    kind: here
  environment:
    kind: none
  project:
    kind: live_direct
    access: read_only
    child_policy:
      kind: inherit
```

All fields are required except that `project_root` is omitted for an explicitly
projectless policy. Policies are authored behavior: the daemon does not infer
them or substitute defaults.

### Bounds

Three optional fields bound what a schedule may do. They are signed into the
schedule's execution authority, and an absent field is no bound.

- **`limits`** — a per-fire ceiling with any of `turns`, `tokens`,
  `spend_usd` (a canonical decimal string), `spawns`, `depth` and
  `duration_seconds`. Each fire's hard limits are computed from the item and
  node configuration as for any root, then clamped under these values. A
  ceiling can only tighten: it never raises an item's request or a node cap.
  Every value must be positive; omit a field to leave that dimension alone.
- **`max_fires`** — the schedule admits at most this many boundaries (1 to
  10000), counted from registration whether each one ran or was skipped.
- **`expires_at`** — an RFC 3339 instant; no boundary at or after it is
  admitted. It must be in the future at registration.

```yaml
limits:
  turns: 20
  spend_usd: "0.50"
  duration_seconds: 600
max_fires: 30
expires_at: "2027-01-01T00:00:00Z"
```

When the last admitted boundary has been recorded, the scheduler pauses the
schedule itself through the same signed write `scheduler.pause` uses, and
records why: `paused_reason` is `max_fires_reached` or `expired`.
`scheduler.explain` reports `paused_reason` and a `lifetime` block
(`max_fires`, `expires_at`, `last_admitted_boundary`, `exhausted`).
`scheduler.resume` refuses a schedule whose lifetime is used up with the typed
error `schedule_lifetime_exhausted`; register the schedule again with a new
bound instead. A daemon that was down past the last admitted boundary still
applies the misfire policy to that boundary and plans nothing after it.

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

## Schedule Authority

Registration pins the exact authorized-key grant generation of the
registering principal, and every fire proves that generation still holds
before anything launches. When it does not, the fire is recorded as `failed`
with a typed outcome instead of the generic `dispatch_failed`:

| Outcome | Cause |
|---|---|
| `authority_revoked` | the principal's grant, or a forwarding node's grant, no longer exists or carries a revocation tombstone |
| `authority_expired` | the principal's grant has lapsed; renew it with `ryeos renew-key` (renewal re-seals the schedules it still covers) |
| `authority_changed` | a different grant generation, principal class or origin site; a node-owned schedule that no longer names this node |
| `authority_narrowed` | the grant no longer covers a declared capability |

The first such refusal pauses the schedule through the same signed write
`scheduler.pause` uses, with `paused_reason` set to the outcome, so later
boundaries do not fail one by one. `scheduler.explain` reports an `authority`
block computed by the same revalidation: `valid`, the typed `refusal`
(`grant_revoked`, `grant_expired`, `grant_generation_changed`, `class_or_origin_changed`,
`capability_no_longer_covered`, `forwarding_grant_revoked`,
`forwarding_grant_changed`, `forwarding_attestation_not_covered` or
`node_authority_changed`), its `outcome`, and a `detail`. `scheduler.resume`
refuses with `schedule_authority_invalid` until the authority holds again.

### Reauthorize

The generation pin is exact: re-issuing a grant, even with identical scopes,
is a new generation, and the schedule's fires are refused until its owner
re-pins it. `ryeos scheduler reauthorize <id>` (`scheduler.reauthorize`) does
that without resubmitting the spec. Signed by the owner, it seals the grant
generation that authenticated the request into the existing signed source;
every other field, `registered_at` and the registration request digest are
kept, and the enabled state is unchanged, so a paused schedule is then resumed
with `scheduler.resume`. It is refused, and nothing is written, with:

- `schedule_reauthorize_not_covered` when the current grant no longer covers
  every capability the schedule declares (register it again with a narrower
  capability set);
- `schedule_reauthorize_class_or_origin_changed` when the current grant's
  principal class or origin site differs from the pinned one;
- `schedule_not_grant_bound` for a node-owned schedule, which pins no grant.

A caller that does not own the schedule gets not-found, as for the other
owner verbs. `ryeos node doctor` reports, under `schedule_authority`, every
schedule whose pinned generation is missing or superseded, before its next
fire refuses.

## Pause and Resume

Pausing a schedule stops new fires from being created. Existing
running threads are not affected. Resuming re-enables the schedule
starting from the next scheduled time. The scheduler also pauses a schedule
by itself when its lifetime bounds are reached (see Bounds); it records the
reason as `paused_reason`, which an operator pause leaves absent and resume
clears.

## Use Cases

- **Periodic health checks** — run the Core doctor subprocess Tool every hour
- **Data sync** — execute a sync tool on a cron schedule
- **Report generation** — run an admitted reporting subprocess Tool
- **Cleanup** — requires an eligible admitted subprocess target; the current
  in-process maintenance/gc service cannot be dispatched by this capsule path

Service and command descriptors are owned by the scheduling bundle. This
reference remains with the existing Standard knowledge content owner; reading
it is not a runtime dependency of scheduling. Source configuration and this
example do not establish signed admission, filesystem confinement or a
qualified operation. The source-only composition migration leaves generated
bundle manifests pending rebuild and separately authorized signing.
