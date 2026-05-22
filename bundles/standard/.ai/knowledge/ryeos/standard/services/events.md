---
category: ryeos/standard/services
tags: [service, events, replay, threads]
version: "1.0.0"
description: Events service reference.
---

# Services: events

Invariant: event services replay persisted thread events without mutating thread state.

- `events/replay` returns events for a single thread, optionally after a sequence cursor.
- `events/chain_replay` returns events across a chain root and descendants.

These services back CLI event replay and streaming catch-up behavior.
