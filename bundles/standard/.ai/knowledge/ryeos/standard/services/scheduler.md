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
