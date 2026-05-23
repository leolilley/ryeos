<!-- ryeos:signed:2026-05-23T09:45:40Z:e5b0916deb9086e9b4f40323473f97dfa657c420781fc2918dd59b92d8c25554:D2ywkzaY7TVkOT0NXU/gvraAOs5kB5aJYO7jovwsq8GpKorpVHNyzuvoCIBQxP15JyQNfkQdtm7zp9Bud7nRDw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
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
