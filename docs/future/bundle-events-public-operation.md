# Bundle events public operation surface

RyeOS has a durable bundle-event substrate: bundle events are append-only,
CAS-backed facts scoped to a verified bundle identity. The storage service,
signed manifest declarations, callback RPCs, and manifest-derived capabilities
exist, but the public bundle-author surface is intentionally not a language
runtime package.

## Current landing

- `bundle_events:` in a signed bundle manifest declares event kinds and allowed
  operations.
- The executor derives `ryeos.append.bundle_events.<bundle>/<event_kind>` and
  `ryeos.scan.bundle_events.<bundle>/<event_kind>` capabilities from the
  verified bundle manifest.
- The daemon callback path derives the effective bundle from the verified
  executing tool context and authorizes against those capabilities.
- Durable storage lives in RyeOS state/CAS as bundle event objects and refs.
- The low-level `ryeos-core-tools bundle-events` command remains dev/operator
  plumbing only; it is not the runtime authorization boundary.

## Problem to solve

Bundle authors need a language-neutral way to append, read, and scan bundle
events. The first attempted API exposed this as a Python-only runtime support
package (`from ryeos_runtime import events`), which made a platform primitive
look like Python SDK surface and privileged one runtime over others.

## Desired public contract

Expose bundle events as a RyeOS runtime operation, not as a language-specific
library and not as a direct DB CLI wrapper.

Candidate operations:

- `bundle_events.append`
- `bundle_events.read_chain`
- `bundle_events.scan`

Normal self-bundle calls must not accept `bundle_id` or `effective_bundle_id`.
The daemon must derive bundle identity from callback/execution context.

Append input shape:

```json
{
  "event_kind": "email_event",
  "chain_id": "email_123",
  "event_type": "email_planned",
  "schema_version": 1,
  "payload": {},
  "expected_chain_head_hash": null,
  "idempotency_key": "optional",
  "correlation_id": "optional",
  "causation_id": "optional"
}
```

Read-chain input shape:

```json
{
  "event_kind": "email_event",
  "chain_id": "email_123"
}
```

Initial scan input shape:

```json
{
  "event_kind": "email_event"
}
```

Future scan should add pagination/cursors before high-volume use.

## Non-goals

- Do not expose raw CAS read/write as the bundle-author API.
- Do not wrap `ryeos-core-tools bundle-events` for normal runtime execution;
  that command accepts caller-supplied bundle identity, opens state directly,
  and uses dev/operator attribution semantics.
- Do not reintroduce a Python-only API as the canonical surface. Language
  helpers may exist later only as thin wrappers over the neutral operation.

## Projection follow-up

Bundle projections/read models remain separate. Until RyeOS exposes a
first-class bundle projection API, downstream bundles may keep rebuildable
read-model caches over bundle events, but those caches must not own durable
truth.
