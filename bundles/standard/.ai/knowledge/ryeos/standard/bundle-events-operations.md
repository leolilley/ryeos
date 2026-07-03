<!-- ryeos:signed:2026-07-03T09:54:31Z:1bc3cb1387a5721f9f4fe97d1d57e906054c30826523869adc30af20ba1daa57:TxBSWHBvPTm1W/aGSqB5V/txoCTGnfseDyIaLfIiXtuaVddeppc7SHfL6zd5EoTYj56NDf17+ZfTwRphhpBYDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [bundle-events, runtime, operations, callbacks, identity]
version: "1.0.0"
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
  "chain_id": "example_123"
}
```

Returns the ordered events of one chain. Authorized under the `scan` verb (not a
separate `read`): a create-or-append tool needs **both** `scan` and `append`.

### Scan

```json
{
  "event_kind": "example_event"
}
```

Returns events of a kind across the bundle. Authorized under `scan`.

## Scan pagination is the pre-high-volume gate

Scan currently returns the full matching set for an event kind. Before any
high-volume use, `runtime.bundle_events_scan` must grow cursor/pagination
parameters (an opaque cursor plus a bounded page size); until then, treat scan
as bounded to modest event counts and prefer `read_chain` for a known chain.
Downstream read-model caches over bundle events remain rebuildable projections
and must not own durable truth.

## What these are not

- Not raw CAS read/write — that is never the bundle-author API.
- Not the `ryeos-core-tools bundle-events` dev/operator command, which accepts
  caller-supplied bundle identity, opens state directly, and uses operator
  attribution. That path is plumbing, not the runtime authorization boundary.
