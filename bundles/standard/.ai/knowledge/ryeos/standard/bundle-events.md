<!-- ryeos:signed:2026-07-04T03:59:54Z:83db9611f757faee8503e75cff9e1f2ed0764679adf43646999542f04f77393b:7eH77Gw737q2GvaCpSi9UqnEAj6lILEJX1BYvXUtzfZZjeO6rMFBHTT5vfTc9AvKwGnJO6lDTN+GC/bgidvTCA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [bundle-events, runtime-authority, manifest, capabilities, vault]
version: "1.0.0"
description: >
  Durable bundle events and the runtime-authority model — how a tool gains the
  capability to append/read its bundle's event chains, and why that authority
  comes only from a signed manifest, never self-granted by an item.
---

# Bundle events and runtime authority

A bundle event is a durable, append-only record written to a per-bundle event
chain via the daemon runtime callbacks (`runtime.bundle_events_append`,
`runtime.bundle_events_read_chain`, `runtime.bundle_events_scan`). They are the
"durable memory" half of a real agent: pattern stats, suppression lists, audit
trails that must survive across runs.

Access is gated by capabilities. The important rule:

> Bundle-event (and runtime-vault) capabilities are **runtime authority**: the
> daemon mints them **only** from a signed bundle manifest. An item *requests*
> them under `requires.capabilities.manifest`; they can **never** be self-granted
> under `requires.capabilities.declared`.

This is the manifest runtime-authority model — see
`ryeos/future/tool-runtime-authority`. Authority is always minted by the daemon
from signed metadata, never accepted from runtime-declared input.

## Capability shape

Capabilities are `ryeos.<verb>.<kind>.<subject>`. For bundle events:

```
ryeos.<verb>.bundle-events.<bundle-id>/<event-kind>
```

- `<verb>` is `append` or `scan`.
- `<bundle-id>` is the effective bundle id — the first segment of the executing
  item's ref (e.g. `tool:arc/play` → `arc`). You do **not** declare it; the
  daemon derives it from the verified tool ref and it must equal `manifest.name`.
- `<event-kind>` is the event family you declared.

## Verbs: append vs scan (and why read needs scan)

| Runtime callback | Authorized verb |
|---|---|
| `bundle_events_append` | `append` |
| `bundle_events_read_chain` | `scan` |
| `bundle_events_scan` | `scan` |

`read_chain` is authorized under **`scan`**, not a separate `read` verb. So a
tool that does **create-or-append** — read the current chain, then append — needs
**both** capabilities granted: `scan` (to read) and `append` (to write).
Declaring only `append` produces a confusing `scan` denial on the read.

Event kinds are **concrete-only, on both sides**: manifest validation rejects
`*`/`?` in `event_kind` (and in `runtime_vault.namespace`), and a request is
matched by exact id at mint and compose time — spell every event kind out.
Pattern grammar exists only for item-authoring namespaces; that rule is
stated in `knowledge:ryeos/standard/item-authoring` § "Wildcard semantics".

## Declaring authority in the manifest

Declare the event kinds and operations in the bundle's
`.ai/manifest.source.yaml`, then sign the bundle (`ryeos bundle publish`). The
manifest declares only kinds and operations — never the bundle namespace (that
comes from the verified tool ref):

```yaml
name: arc            # must equal the effective bundle id used in item refs
version: "0.1.0"
description: ARC agent
provides_kinds: []
requires_kinds: []
uses_kinds: []
runtime_authority:
  bundle_events:
    - event_kind: arc_pattern_event
      operations: [append, scan]   # both — the tool does create-or-append
```

From this signed manifest the daemon mints, for a tool whose ref resolves under
bundle `arc`:

```
ryeos.append.bundle-events.arc/arc_pattern_event
ryeos.scan.bundle-events.arc/arc_pattern_event
```

## What does NOT work

Self-granting the capability under `requires.capabilities.declared`:

```yaml
# graph.yaml / directive — rejected
requires:
  capabilities:
    declared:
      - ryeos.append.bundle-events.arc/arc_pattern_event
```

This never grants access — `declared` is self-asserted action authority, and the
daemon **rejects** a runtime-authority capability there outright: `ryeos graph
validate` reports it up front, and the daemon refuses to mint a callback token
that carries a self-granted runtime-authority capability.

## What works

Declare the authority in the signed manifest, then *request* the subset the item
needs under `requires.capabilities.manifest`:

```yaml
# item — requested, and minted only because the signed manifest backs it
requires:
  capabilities:
    manifest:
      runtime_authority:
        bundle_events:
          - event_kind: arc_pattern_event
            operations: [append]
```

## Runtime vault

Runtime-vault capabilities (`ryeos.<verb>.vault.<bundle-id>/<namespace>`, verbs
`put`/`get`/`delete`/`list`) follow the identical model: declared under
`runtime_authority.runtime_vault:` in the signed manifest, requested under
`requires.capabilities.manifest.runtime_authority`, minted by the daemon, never
self-grantable under `requires.capabilities.declared`.
