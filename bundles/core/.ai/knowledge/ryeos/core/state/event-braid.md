<!-- ryeos:signed:2026-06-07T04:05:13Z:cbfb4e8cc3a513f0e638b6305c9ad029202a3a7e55a8954daf52dc9f4b1ead11:L4Jq1277tkxfHa3xno13SJ4M0tmM13iyHawRhNnhfjTd1CDGoQGro4e8kmKNwjwSCQqPCsx77G9JVsDCxEHCBQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/state
tags: [architecture, events, braid, chain, thread, durability, replay]
version: "1.0.0"
description: >
  The dual hash-linked event braid — two independent hash chains in a
  single event stream, enabling efficient global and per-thread replay.
  Also covers event durability tiers and replay mechanisms.
---

# Event Braid

Every event in Rye OS carries two hash links, creating a **braid**: a
single event stream with two independent traversal paths.

## The Two Links

Each `ThreadEvent` has two `Option<String>` back-link fields:

| Field | Links to | Purpose |
|---|---|---|
| `prev_chain_event_hash` | Previous event in the **chain** (across all threads) | Global ordering |
| `prev_thread_event_hash` | Previous event for this **thread** | Per-thread sub-chain |

Both fields are `None` for the first event in their respective chains.

## How the Braid Works

When events are appended, the chain module maintains two pointers:

1. The chain-level pointer — the hash of the last event appended to
   the chain, regardless of thread
2. The thread-level pointer — the hash of the last event appended to
   this specific thread

Each new event gets both pointers as its back-links, then both pointers
advance to the new event's hash.

```
Chain:   E1(A) → E2(A) → E3(B) → E4(A) → E5(B)
Thread A: E1 → E2 ──────────────→ E4
Thread B:           E3 ──────────────→ E5
```

You get efficient **global replay** (follow chain links) AND efficient
**per-thread replay** (follow thread links) without duplicating events.

Both links are SHA-256 hashes stored as 64-character hex strings. They
are validated against `lillux::valid_hash()` on append.

## Event Structure

Events are stored as CAS objects in canonical JSON. Each event contains:

| Field | Type | Description |
|---|---|---|
| `chain_root_id` | String | The chain root this event belongs to |
| `chain_seq` | i64 | Monotonic sequence number in the chain |
| `thread_id` | String | The thread this event belongs to |
| `thread_seq` | i64 | Monotonic sequence number in the thread |
| `event_type` | String | The event type (e.g. `thread_created`, `tool_call_start`) |
| `durability` | String | One of `durable`, `journal`, `ephemeral` |
| `ts` | String | ISO 8601 timestamp |
| `prev_chain_event_hash` | Option\<String\> | Chain back-link |
| `prev_thread_event_hash` | Option\<String\> | Thread back-link |
| `payload` | Value | Event-specific data |

## Durability Tiers

| Tier | CAS Stored? | SQLite Indexed? | Survives Crash? | Use Cases |
|---|---|---|---|---|
| `Durable` | Yes | Yes | Yes | Lifecycle events, artifacts, tool dispatch |
| `Journal` | Yes | No | Yes | Audit trail, progressive events |
| `Ephemeral` | No | No | No | Token deltas, streaming snapshots |

Only durable events appear in the SQLite projection. Journal events are
recoverable from CAS during a projection rebuild but are not queryable
at runtime. Ephemeral events are transient — they exist only in the
broadcast stream and are not persisted.

High-frequency progressive events are assigned journal tier:
`token_delta`, `stream_snapshot`, `cognition_reasoning`,
`graph_foreach_iteration`. All other events are durable.

## Replay

Two replay endpoints provide separate traversal paths:

### Thread-Scoped Replay (`events.replay`)

Replays events for a single thread, ordered by `chain_seq`. Supports
cursor-based pagination via `after_chain_seq`.

```bash
ryeos events replay <thread_id> --limit 200
```

### Chain-Scoped Replay (`events.chain_replay`)

Replays the entire chain (root + all descendant threads), ordered by
`chain_seq`. Includes events from all threads in the chain.

```bash
ryeos events chain-replay <chain_root_id> --limit 200
```

### Pagination

Both endpoints use `chain_seq` as a monotonic cursor. If the result set
equals the requested limit, a `next_cursor` is returned (the `chain_seq`
of the last event). Default limit: 200.

### Projection Rebuild

The projection rebuild walks the chain toward earlier links via
`prev_chain_event_hash`, starting from signed chain heads. It projects
only durable events into the SQLite `events` table. This is a full CAS
replay that produces an identical projection to the original.

### Live Streaming

Real-time event streaming uses per-thread `tokio::sync::broadcast`
channels. Events are published **after** CAS persistence. Lagged SSE
subscribers catch up by replaying from `after_chain_seq = last_seen_seq`.

## Bundle Events

Bundle events are durable, bundle-scoped application events owned by Rye
OS state/CAS. They are distinct from thread lifecycle events: a bundle
uses them for its own domain history, while the daemon still owns
identity, authorization, storage, and attribution.

Bundle manifests declare event kinds and allowed operations:

```yaml
name: ryeos-email
bundle_events:
  - event_kind: email_event
    operations: [append, scan]
```

When a verified bundle-qualified tool executes, Rye OS derives callback
capabilities from the signed manifest and the tool ref. For the manifest
above, a direct `tool:ryeos-email/...` execution receives callback caps
such as:

```text
ryeos.append.bundle_events.ryeos-email/email_event
ryeos.scan.bundle_events.ryeos-email/email_event
```

Bundle code must not pass `bundle_id`. The daemon mints callback tokens
with `effective_bundle_id` derived from the verified root item ref and
rejects caller-supplied bundle identity. The runtime callback APIs take
only event-kind and chain data; daemon-side UDS handlers enforce the
callback token and required capability before touching state.

Runtime callback operations:

| Operation | Capability checked | Purpose |
|---|---|---|
| `bundle_events_append` | `ryeos.append.bundle_events.<bundle>/<event_kind>` | Append one event to a bundle chain. |
| `bundle_events_read_chain` | `ryeos.scan.bundle_events.<bundle>/<event_kind>` | Read one chain for an event kind. |
| `bundle_events_scan` | `ryeos.scan.bundle_events.<bundle>/<event_kind>` | Scan all records for an event kind. |

`read_chain` is covered by the `scan` manifest operation in the current
schema. If a future manifest adds an explicit `read` or `read_chain`
operation, this table and the capability derivation must be updated
together.

Append request shape:

```json
{
  "event_kind": "email_event",
  "chain_id": "email_123",
  "event_type": "email_planned",
  "schema_version": 1,
  "payload": {"campaign_id": "campaign_abc"},
  "idempotency_key": "email_plan:email_123",
  "expected_chain_head_hash": null,
  "correlation_id": null,
  "causation_id": null
}
```

Read-chain request shape:

```json
{
  "event_kind": "email_event",
  "chain_id": "email_123"
}
```

Scan request shape:

```json
{
  "event_kind": "email_event"
}
```

The daemon adds `thread_id` and callback authentication on the UDS
request. Tool authors should use the runtime callback client available to
their runtime rather than shelling out to operator commands such as
`ryeos-core-tools bundle-events ...`.

## Reachability

The GC sweep's `collect_reachable` function follows both hash links via
BFS traversal. Any event reachable from a signed head (through either
chain or thread links) is considered live and will not be collected.
