<!-- ryeos:signed:2026-05-22T19:55:06Z:629682675cbcadb3f951a93bc0dc746b6b335e04a15b5ef173f6be1ee039194c:JOOucYMcrdprA1dlb5sOPi9KfvmySAkZdjyNSDSaV7YRhVoKIMQXiKtmkUSIpJCx7D+zJAcGK7S6ZgIIUl5WCQ==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
