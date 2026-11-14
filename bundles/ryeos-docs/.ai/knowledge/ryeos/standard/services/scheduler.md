<!-- ryeos:signed:2026-11-14T05:54:19Z:c0e67f4362ad1ff365c05f6df806b06ed33ab356159636d208cdae408cec5e61:UDUGH2qrHH4P/SXyOkIeFc2MwFy+B4BvH3Xcm1RqrQKPOAmSKf/dwkbgaJ8WFOtX4A3MBOwfPIiwy3kYNlEdDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
`authority_revoked` (the grant, or a forwarding node's grant, is gone or
carries a revocation tombstone), `authority_expired` (the grant lapsed),
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
matches, schedules whose grant lapses within the policy's warning window, and
schedules paused for want of authority under `schedule_authority`, each with
its fix. `scheduler/explain`'s `authority` block also carries `expires_at`,
the earlier of the pinned grant's expiry and the schedule's own `expires_at`.

Grant lifecycle and schedules: `identity/renew-key` supersedes the pinned
generation, so in the same operation it re-seals every schedule pinned to the
superseded generation whose declared capabilities the renewed grant still
covers, and pauses the ones it no longer covers with `authority_narrowed`; its
receipt lists both. `identity/revoke-key` pauses every schedule pinned to a
revoked grant at once with `authority_revoked` (the tombstoned digest is what
the schedule names, so authorizing the same key again does not revive it:
re-register the schedule, or re-seal it with `scheduler/reauthorize` as its
owner, then resume). A lapsed grant pauses its schedules at their next fire
with `authority_expired`; renewing the grant re-seals them. A `live_direct` fire intentionally
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
