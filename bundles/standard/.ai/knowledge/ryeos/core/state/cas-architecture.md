<!-- ryeos:signed:2026-07-14T23:18:43Z:e77ceab63ac74285cb292f30a4c2117a776287607391635ef1014cea8f65a72c:TT325xfHbQoRAMpYG6wqpqaSb2TGMsh43NaczQXnIrZo86biC7agMpOzuRR274JOGyedlsO9SCI/B9hyCHwHDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->

---
category: ryeos/core/state
tags: [architecture, cas, state, truth, projection, sqlite]
version: "1.0.0"
description: >
  The three-tier truth model — CAS objects, signed refs, and the
  rebuildable SQLite projection. Content-addressed storage as the
  foundation of all state in the system.
---

# CAS Architecture

Rye OS uses content-addressed storage (CAS) as its single source of
truth. All state — events, snapshots, manifests, project files — flows
through CAS. Everything else is a derived view.

## The Three-Tier Truth Model

| Tier | Mutable? | Rebuildable? | Purpose |
|---|---|---|---|
| **CAS objects** | No (append-only) | N/A | Authoritative source of truth |
| **Signed refs** | Yes (one per head) | No | Entry points into the CAS graph |
| **SQLite projection** | Yes (derived) | Fully | Query performance |

The critical contract is **CAS-first, journaled writes**. Every chain-head
change records a durable pending transition before publishing its signed ref.
If projection work fails, the pending record remains and the repair worker
replays that named chain. Normal startup enumerates only this journal; it does
not sweep every historical signed head.

The selected projection has a generation-scoped instance identity and path,
named
`projection.<instance-id>.sqlite3`, chosen by
`state/recovery/thread-projection/generation.json`. If that selected generation
is absent, invalid, or from another schema epoch, bootstrap builds a fresh
instance by walking trusted signed heads, verifies it, and then atomically
publishes the generation pointer. The bootstrap lifecycle surface remains
responsive while that offline recovery runs.

### CAS Objects

Objects are stored under `state/objects/` using a two-level hex shard
layout: `objects/<ab>/<cd>/<sha256hash>.json`. Writes use
`lillux::atomic_write` (write-then-rename) for crash safety. Content is
serialized to canonical JSON and SHA-256 hashed before storage.

Every object — events, snapshots, manifests, chain state — is an
immutable JSON blob in CAS. The hash is the identity.

### Signed Refs

Refs are mutable pointers into the CAS graph. They live under
`state/refs/` and are updated atomically. Each ref points to exactly
one CAS object hash. Refs include:

- **Chain heads** — the latest chain state per root
- **Project heads** — the latest snapshot per project/principal
- **Bundle registrations** — signed records linking bundle names to paths

Refs are written by the node's signing key, so they are tamper-evident.

### SQLite Projection

The selected `projection.<instance-id>.sqlite3` is a materialized view of CAS
state, optimized for query performance. Only `Durable` events are indexed;
journal and ephemeral events are not projected.

The projection is **not authoritative** — it is a cache that can be
fully rebuilt from CAS at any time.

### Stable Operational State

`state/operational.sqlite3` owns records that cannot be reconstructed from
signed chain heads: CAS-entry attribution, admission-attestation lookup rows,
sync jobs, and sync-job attempts. It is a fixed source-of-truth store, is never
selected through `generation.json`, and is never copied, replaced, or removed
by projection rebuild or generation cleanup. `state/operational.initialized`
fails closed if an established operational database later disappears.

## SQLite Schema Ownership

Every database in the system is stamped with a `PRAGMA application_id`:

| Database | ID | Hex | ASCII |
|---|---|---|---|
| `runtime.sqlite3` | `0x52594541` | `RYEA` | Runtime |
| `operational.sqlite3` | `0x52594f50` | `RYOP` | Stable operational state |
| `projection.<instance-id>.sqlite3` | `0x5259504a` | `RYPJ` | Projection generation |
| `scheduler.sqlite3` | `0x52595343` | `RYSC` | Scheduler projection |

On open, the system performs a four-step exhaustive check:

1. **Application ID match** — verifies the file was created by this
   daemon, not a foreign process
2. **Table set verification** — every expected table exists; no
   unexpected tables
3. **Column verification** — column count, names, types, primary keys,
   and NOT NULL constraints match exactly
4. **Index verification** — all expected indexes exist with correct
   uniqueness, columns, and tables; no unexpected indexes

If any check fails, the error includes a recovery recipe:
`mv <file> <file>.foreign.$(date +%s)`.

An empty file (new database) triggers `init_owned()` which runs the DDL
and stamps the application ID. This means the projection is self-healing
on first startup and fail-loud on foreign files.

## Event Durability Tiers

Events have three tiers that control CAS storage and SQLite indexing:

| Tier | CAS Stored? | SQLite Indexed? | Survives Crash? | Use Case |
|---|---|---|---|---|
| `Durable` | Yes | Yes | Yes | State changes, artifacts, lifecycle |
| `Journal` | Yes | No | Yes | Audit trail, tool calls |
| `Ephemeral` | No | No | No | Token deltas, streaming logs |

The SQLite `events` table has a CHECK constraint that only accepts
`'durable'`:

```sql
durability TEXT NOT NULL CHECK (durability IN ('durable'))
```

Journal events are in CAS (recoverable on rebuild) but not queryable
through the projection. Ephemeral events are transient — lost on process
crash.

Events are assigned tiers by the runtime: high-frequency progressive
events (`token_delta`, `stream_snapshot`, `graph_foreach_iteration`)
are journal-only; all lifecycle and audit events are durable.

## CAS-First Write Contract

The system never writes to the projection without first writing to CAS and
publishing through the per-chain transition journal. The projection is a
**second-class citizen**:

- CAS/head succeeds, projection fails → the pending Set stays durable and the
  named-chain repair worker converges it
- Selected generation is deleted → bootstrap performs a verified full rebuild
  into a new selected instance before the application becomes Ready
- `INSERT OR IGNORE` in the projection prevents duplicate event indexing

`projection verify` is a fail-only, read-only inspection of the selected
generation. `projection rebuild` explicitly constructs and verifies a new
generation while the daemon is offline. Neither command runs as an automatic
history-sized sweep on a normal current-generation boot.

This is an event sourcing pattern where the events live in a
content-addressed graph with hash-linked chains, and the projection is
just a materialized view.

## Object Services

The CAS layer is exposed through three service endpoints:

| Service | Endpoint | Purpose |
|---|---|---|
| `objects/has` | `POST /objects/has` | Check whether hashes exist |
| `objects/put` | `POST /objects/put` | Write content to CAS |
| `objects/get` | `POST /objects/get` | Fetch content by hash |

All three are fail-closed: `objects/get` aborts if any requested hash
is missing rather than returning partial results. Remote push/pull,
pushed-head execution, and remote bundle install all depend on these
services.

## Garbage Collection

GC operates on the CAS layer in two phases:

1. **Compact** (opt-in): prunes snapshot DAGs per project according to
   `RetentionPolicy`, rewrites parent hashes, advances HEAD refs
2. **Sweep** (always): mark-and-sweep across all sharded directories,
   deletes unreachable objects and blobs, cleans empty shard directories

See [maintenance-gc](../services/maintenance-gc.md) for details.
