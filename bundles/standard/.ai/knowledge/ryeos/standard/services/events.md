<!-- ryeos:signed:2026-05-20T05:57:10Z:e5b0916deb9086e9b4f40323473f97dfa657c420781fc2918dd59b92d8c25554:35yXvtr+DwIcDo/fLdXvrpCJpLDdDPtwdU6wBfrnzf9hWbv+lxf9+QCjgyY83pP5Ed7VYPXm3rC7oQqfrZh2DQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
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
