<!-- ryeos:signed:2026-07-14T23:18:43Z:0a5517278357f12227abfc49d64b9e72fad820f541b40bc0923414fbd1cfb14a:k3rZqnDDi0DQubEnvqoupeYr9dZS38hsp1GObRyKQI04u8ld7ACnT+FATJVPGr2bB5hpw0TUdVAwT3Sq9N50Cw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, scheduler, workflows]
version: "1.0.0"
description: Scheduler service reference.
---

# Services: scheduler

Invariant: scheduler services manage recurring workflow execution specs and fire history in daemon state.

Services: `scheduler/register`, `scheduler/list`, `scheduler/deregister`, `scheduler/pause`, `scheduler/resume`, and `scheduler/show_fires`.

`scheduler/register` requires an explicit complete schedule contract:
`schedule_id`, `item_ref`, `schedule_type`, `expression`, object `params`,
`timezone`, `misfire_policy`, `overlap_policy`, positive
`lateness_grace_secs`, and boolean `enabled`. `project_root` is optional.
No scheduling or policy field is defaulted by the daemon.

Scheduler descriptors live in standard because scheduled work is a workflow-layer feature that launches directives/graphs through the normal execution runner.
