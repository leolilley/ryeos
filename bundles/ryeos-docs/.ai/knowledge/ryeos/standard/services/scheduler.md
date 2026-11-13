<!-- ryeos:signed:2026-11-13T01:08:33Z:7c0206890b5670fb7206f86fd3a8828a5d93fd1622338a348b94504e2bf7b396:e4frNSfX+bnC7/Yg3Lrw/sYt+wKw+uYX1gtoedqp1hkaTV86y6NMjIh29KKnzyHa5rNUJO213B2Fd3uI+rJwDg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, scheduler, scheduling]
version: "1.0.0"
description: Scheduler service reference.
---

# Services: scheduler

Invariant: scheduler services manage one-shot and recurring capsule-producing subprocess Tool specs and fire history in daemon state.

Services: `scheduler/register`, `scheduler/list`, `scheduler/deregister`, `scheduler/pause`, `scheduler/resume`, `scheduler/explain`, and `scheduler/show_fires`.

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
declared capability subset before launching. A `live_direct` fire intentionally
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
