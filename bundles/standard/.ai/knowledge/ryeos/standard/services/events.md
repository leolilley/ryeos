<!-- ryeos:signed:2026-07-08T04:27:34Z:06edbd6d33bba0e87491eb2b6168d6528d867096f53a5487c7e57571f4bdb3e7:lJP7s9wOjjSyMg2uGI2Wmc+wL+Zr5W2DVDM6QH6u7vZPePX3GdcfBYKPQWhEQBDg71LdgkoGmwqIHYCfcGaBDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard/services
tags: [service, events, replay, threads]
version: "1.0.0"
description: Events service reference.
---

# Services: events

Invariant: event services replay persisted thread events without mutating thread state.

- `events/replay` returns events for a single thread, optionally after a chain
  sequence cursor.
- `events/chain_replay` returns events across a chain root and descendants,
  ordered by chain sequence.

These services back CLI event replay and streaming catch-up behavior.

## Event rows and refs

Durable replay rows expose the current event hash as `event_hash`, plus the
previous hash links `prev_chain_event_hash` and `prev_thread_event_hash`.
`event_hash` is the CAS hash of the canonical persisted event object after RyeOS
has assigned sequence numbers and previous-hash links.

The stable event reference shape used by higher-level trace services is:

```json
{
  "chain_root_id": "T-root",
  "thread_id": "T-child",
  "chain_seq": 42,
  "thread_seq": 3,
  "event_hash": "64-hex-cas-hash",
  "event_type": "milestone"
}
```

`chain_seq` is global within the chain root. `thread_seq` is local to the
thread. Replay cursors use chain sequence (`after_chain_seq`), not raw row ids.

Callers must not trust caller-supplied event refs by shape alone. A service that
uses an event ref must replay the cited durable event and compare
`chain_root_id`, `thread_id`, `chain_seq`, `thread_seq`, `event_hash`, and
`event_type`. The event must be reachable from the signed chain head through
the event hash chain.

## Branch and trace consumers

`events.replay` and `events.chain_replay` are raw replay surfaces. They do not
normalize state anchors, branch relations, or higher-level provenance. Use
`trace.inspect` when a caller needs a structured trace view over the same
durable rows.
