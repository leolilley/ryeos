<!-- ryeos:signed:2026-06-19T13:42:34Z:1aabcf8bd5d00bdd446556d91a59c2c66dfdd47bfbcff81c66ab98975313f26d:tqibQmUayY4ki1HdIhqaZhQeDK4AKb63pTt4wbPOAtefc2UXHBgZCHKfrRUu3YxgfdOFU79tqXe+CY+paAlyDA==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/standard
tags: [bundle-events, runtime-authority, manifest, capabilities, vault]
version: "1.0.0"
description: >
  Durable bundle events and the runtime-authority model — how a tool gains the
  capability to append/read its bundle's event chains, and why that authority
  comes only from a signed manifest, never from graph or directive permissions.
---

# Bundle events and runtime authority

A bundle event is a durable, append-only record written to a per-bundle event
chain via the daemon runtime callbacks (`runtime.bundle_events_append`,
`runtime.bundle_events_read_chain`, `runtime.bundle_events_scan`). They are the
"durable memory" half of a real agent: pattern stats, suppression lists, audit
trails that must survive across runs.

Access is gated by capabilities. The important rule:

> Bundle-event (and runtime-vault) capabilities are **runtime authority**: the
> daemon mints them **only** from a signed bundle manifest. They can **never**
> be granted through a graph's or directive's `permissions:` block.

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
bundle_events:
  - event_kind: arc_pattern_event
    operations: [append, scan]   # both — the tool does create-or-append
runtime_vault: []
```

From this signed manifest the daemon mints, for a tool whose ref resolves under
bundle `arc`:

```
ryeos.append.bundle-events.arc/arc_pattern_event
ryeos.scan.bundle-events.arc/arc_pattern_event
```

## What does NOT work

Listing the capability in a graph or directive `permissions:` block:

```yaml
# graph.yaml — INERT, and now rejected
permissions:
  - ryeos.append.bundle-events.arc/arc_pattern_event
```

This never grants access — the daemon does not accept runtime authority from
composed permissions. As of the manifest-runtime-authority policy it is
**rejected** outright: `ryeos graph validate` reports it up front, and the daemon
refuses to mint a callback token that carries a self-granted runtime-authority
capability. Declare it in the signed manifest instead.

## Runtime vault

Runtime-vault capabilities (`ryeos.<verb>.vault.<bundle-id>/<namespace>`, verbs
`put`/`get`/`delete`/`list`) follow the identical model: declared as
`runtime_vault:` in the signed manifest, minted by the daemon, never grantable
via `permissions:`.
