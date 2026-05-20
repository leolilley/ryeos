<!-- ryeos:signed:2026-05-20T05:57:10Z:629682675cbcadb3f951a93bc0dc746b6b335e04a15b5ef173f6be1ee039194c:JOMgRcIYtDavmLQYaVPVGKvT/OfXoBBbaH3mwWUPRd2Q+bN/GAuO6y3Gk0Tp8/8KIO217t8/6QCP50ibaqCzDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, scheduler, workflows]
version: "1.0.0"
description: Scheduler service reference.
---

# Services: scheduler

Invariant: scheduler services manage recurring workflow execution specs and fire history in daemon state.

Services: `scheduler/register`, `scheduler/list`, `scheduler/deregister`, `scheduler/pause`, `scheduler/resume`, and `scheduler/show_fires`.

Scheduler descriptors live in standard because scheduled work is a workflow-layer feature that launches directives/graphs through the normal execution runner.
