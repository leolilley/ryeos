<!-- ryeos:signed:2026-11-13T01:22:04Z:f098c4c02b8b82b5bb1fbf7ee81324c3980a31cfbdd07fd947352dea54cf42af:jL9QjaUo+4xj8YoUlgHVznfVCgiePpqOczJ98XIxzPzGloOnzbaoFCvo/gOoKE6CXQz4mvRE4M4YVv+u0uCiCw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, scheduler, scheduling]
version: "1.0.0"
description: Scheduler service reference.
---

# Services: scheduler

Invariant: scheduler services manage one-shot and recurring capsule-producing subprocess Tool specs and fire history in daemon state.

Services: `scheduler/register`, `scheduler/list`, `scheduler/deregister`, `scheduler/pause`, `scheduler/resume`, `scheduler/reauthorize`, `scheduler/explain`, and `scheduler/show_fires`.

`scheduler/register` requires an explicit complete schedule contract:
`schedule_id`, `item_ref`, `schedule_type`, `expression`, object `params`,
`timezone`, `misfire_policy`, `overlap_policy`, positive
`lateness_grace_secs`, boolean `enabled`, a sorted unique non-empty
`capabilities` subset, and a complete `execution_policy`. `project_root` must
be absent for projectless execution and present for both explicit
`live_direct` and `pinned` project execution. No scheduling or execution-policy
field is defaulted by the daemon.

Three optional bounds are signed into the schedule's execution authority:
`limits` (a per-fire ceiling clamped into each fire under the item and node
caps), `max_fires` (at most that many boundaries from registration) and
`expires_at` (no boundary at or after that RFC 3339 instant). When the lifetime
bounds admit no further boundary, the scheduler pauses the schedule through
the ordinary signed pause write and records `paused_reason`
(`max_fires_reached` or `expired`); `scheduler/resume` then refuses with
`schedule_lifetime_exhausted`.

Registration seals the authenticated principal's current grant generation
into the schedule authority. Every fire revalidates that exact grant and the
declared capability subset before launching. A fire refused because that
authority no longer holds is recorded as `failed` with a typed outcome:
`authority_revoked` (the grant, or a forwarding node's grant, is gone),
`authority_changed` (a different grant generation, class or origin) or
`authority_narrowed` (the grant no longer covers a declared capability). The
first such refusal pauses the schedule through the ordinary signed pause write
and records the same code as `paused_reason`. `scheduler/explain` reports an
`authority` block (`valid`, `refusal`, `outcome`, `detail`) computed by the
same revalidation, and `scheduler/resume` refuses with
`schedule_authority_invalid` while the authority does not hold.
`scheduler/reauthorize {schedule_id}`, called by the schedule's owner, seals
the owner's current grant generation into the existing signed schedule when
its class, origin and scopes still cover the schedule, and otherwise refuses
with `schedule_reauthorize_not_covered`,
`schedule_reauthorize_class_or_origin_changed` or `schedule_not_grant_bound`.
`ryeos node doctor` reports schedules whose pinned generation no longer
matches under `schedule_authority`. A `live_direct` fire intentionally
resolves the live project again at each fire and recovery and is never
implicitly snapshotted. A `pinned` fire binds one durable
`current_head` or explicit `snapshot` authority and retains it through recovery;
it never falls back to the live tree. Scheduled execution does not permit
interactive `capture_live`.

The admitted fire context is exposed without changing ordinary item
parameters: graph and directive expressions use `execution.schedule`, while a
direct subprocess tool reads the identical canonical JSON projection from the
protected `RYEOS_EXECUTION_CONTEXT` environment variable. Recovery preserves
that exact context; a later fire receives a different `fire_id`. External
observation actions should bind that ID explicitly so recovery replays the same
fire while a later fire receives a fresh observation coordinate.

Scheduler service and command descriptors live in the independently selected
scheduling bundle, which requires Core service support. This reference stays
with the current Standard knowledge content owner. In-process service and
method dispatch targets are ineligible under the capsule-producing dispatch
guard; neither selecting scheduling nor this documentation grants authority.
