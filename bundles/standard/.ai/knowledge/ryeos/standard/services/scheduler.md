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
