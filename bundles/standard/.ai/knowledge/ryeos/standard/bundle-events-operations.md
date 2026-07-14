<!-- ryeos:signed:2026-07-14T10:12:30Z:fb1f3a1f092d1ac2aeb1487488330fa0dbc8705343461d20a617acd151d23c78:4xNfauuEu0deot8j5I7fmmINs2/irdeg1nqvvQjNMKlB4Lbi0ABO6aDirVjynCPTDy1Yw909+DUzJRh20MnWBA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [bundle-events, runtime, operations, callbacks, identity]
version: "1.1.0"
description: >
  The bundle-events runtime operation surface — the three neutral daemon
  callbacks a bundle uses to append, read, and scan its event chains, their
  input shapes, and why bundle identity is never a caller-supplied field.
---

# Bundle-events operations

The durable bundle-event substrate is exposed as **RyeOS runtime operations**,
not a language-specific library. A bundle's executing code reaches them through
the daemon callback channel; there is no `import`-a-package surface, and no
runtime is privileged over another. For *who* may call these (the capability /
manifest runtime-authority model), see `ryeos/standard/bundle-events`. This
document covers *how* the calls are shaped.

## The three operations

The canonical public operation names are the live daemon callbacks:

- `runtime.bundle_events_append`
- `runtime.bundle_events_read_chain`
- `runtime.bundle_events_scan`

These names (namespaced under `runtime.`) are the wire contract. Any
language-specific helper is a thin wrapper over exactly these operations, never
a replacement surface.

## Bundle identity is derived, never supplied

A normal self-bundle call **must not** carry `bundle_id` or
`effective_bundle_id`. The daemon derives the effective bundle id from the
verified callback/execution context (the token minted for the executing item),
and it is that derived identity — equal to `manifest.name` — that scopes every
append, read, and scan.

This is enforced structurally: the daemon's parameter structs reject unknown
fields (`deny_unknown_fields`) and declare no identity field, so a request that
tries to pass `bundle_id`/`effective_bundle_id` is refused at deserialization
before any handler logic runs. Identity cannot be spoofed by input.

## Input shapes

### Append

```json
{
  "event_kind": "example_event",
  "chain_id": "example_123",
  "event_type": "example_planned",
  "schema_version": 1,
  "payload": {},
  "expected_chain_head_hash": null,
  "idempotency_key": "optional",
  "correlation_id": "optional",
  "causation_id": "optional"
}
```

`event_kind`, `chain_id`, and `event_type` are required. `schema_version`
defaults to `1`. `payload` defaults to an empty object. The optional
`expected_chain_head_hash` makes the append a compare-and-append against the
current chain head (optimistic concurrency); `idempotency_key` makes a retry
return the prior result instead of writing twice; `correlation_id` /
`causation_id` carry cross-event lineage.

### Read chain

```json
{
  "event_kind": "example_event",
  "chain_id": "example_123",
  "cursor": null,
  "limit": 16
}
```

Returns a newest-first page from one chain. `cursor` is the opaque event hash
returned as `next_cursor` by the previous page. `limit` defaults to 16 and may
not exceed 16. Authorized under the `scan` verb (not a separate `read`): a
create-or-append tool needs **both** `scan` and `append`.

### Scan

```json
{
  "event_kind": "example_event",
  "cursor": null,
  "limit": 16
}
```

Returns a bounded page for an event kind across the bundle. A non-null scan
cursor has the shape returned by `next_cursor`:

```json
{
  "chain_id": "example_123",
  "event_hash": "sha256..."
}
```

Chains are visited in lexical `chain_id` order and events within each chain are
newest-first. `limit` defaults to 16 and may not exceed 16. Authorized under
`scan`.

## Pagination and page bounds

Both read operations return `events` plus `next_cursor`. A null cursor means the
page is final. In addition to the 16-record maximum, the daemon enforces an 8
MiB serialized-record budget per page; a page may therefore end before its
requested record limit. Continue from `next_cursor` until it is null.

A cross-chain scan inspects at most 4,096 chain-directory entries while choosing
the next lexical chain. Larger namespaces are rejected instead of making a
small callback page perform an unbounded filesystem walk; an indexed chain-head
ordering is required before raising that operational ceiling.

Each cursor names an exact immutable event hash, so later appends to that chain
do not move the remaining traversal behind the cursor. Downstream read-model
caches over bundle events remain rebuildable projections and must not own
durable truth.

## What these are not

- Not raw CAS read/write — that is never the bundle-author API.
- Not the `ryeos-core-tools bundle-events` dev/operator command, which accepts
  caller-supplied bundle identity, opens state directly, and uses operator
  attribution. That path is plumbing, not the runtime authorization boundary.
