```yaml
category: ryeos/future
name: braid-terminal-event-guard
title: Braid Terminal-Event Guard
entry_type: implementation_guide
version: "0.1.0"
description: Chain-level rejection of a second, contradictory terminal event on an already-terminal thread — defense-in-depth behind the status state machine.
tags:
  - braid
  - events
  - thread-lifecycle
  - invariants
```

# Future: Braid Terminal-Event Guard

## Status

Deferred. The thread status state machine refuses contradictory terminal
TRANSITIONS, but the event chain itself will append a second terminal
EVENT — so a braid can carry both `thread_failed` and `graph_completed`
for one run while the projection stays consistent. The one known producer
of that shape is fixed; this doc records the invariant guard for the seam
that should own it.

## The observed shape

A route timeout used to cancel the in-flight handler future; the dropped
future fired the runner's finalize-on-drop guard, appending
`thread_failed` (empty payload) mid-braid — while the runtime child, an
independent OS process, kept executing, appended further events, and
self-finalized with `graph_completed`. Result: two contradictory terminal
events in one braid. The status state machine correctly refused the
failed→completed transition, so the PROJECTED status held (stuck `failed`
for a completed run — wrong, but consistently wrong); the braid carried
both.

The producer is fixed at the route dispatcher: timeouts now bound the
client's wait (the handler runs in its own task and is never cancelled by
an abandoning caller), so client abandonment can no longer fail a thread
whose child is still running.

## Design

Defense-in-depth at the daemon-side event append/finalize path (the UDS
server's `append_event` / finalize handling): a terminal event arriving
for an already-terminal thread is rejected — or quarantined as a distinct
`terminal_conflict` record — LOUDLY, never silently appended as ordinary
braid content. The guard's job is making the next unknown producer of this
shape impossible to miss, not papering over it.

## Trigger

Build it inside the run-stability cancel/finalize plumbing, which is
already reworking terminal-state handling — not as a standalone pass.
Escalate to immediately if a second contradictory-terminal producer is
ever observed in a braid.
