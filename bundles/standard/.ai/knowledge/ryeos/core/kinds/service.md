<!-- ryeos:signed:2026-05-31T08:15:56Z:afdee3729b1d239a16478a0c7e20c4affd97677ca5069637deaa587cb86996d0:TCV3wIPPoxWbWz+lkmmDPIBHjXRBZzwi5tIWrQclCwxf4/udo4gGDRwhdjIS2v7qiSliJfnsMOs4tD/GGyjkDQ==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
---
category: ryeos/core/kinds
tags: [kind, service, daemon, offline, availability]
version: "1.1.0"
description: Service kind reference.
---

# Kind: service

Invariant: `service` items are executable in-process daemon endpoints registered by endpoint name and gated by required capabilities.

- Directory: `services/`
- Formats: signed YAML
- Composer: identity
- Execution terminator: `in_process` registry `services`
- Metadata: `endpoint`, `required_caps`, `schema`, `description`, `availability`

## Availability

Services declare an `availability` field that determines whether the CLI can
run them without a daemon:

- `offline` — runs in the CLI process. Used for source-tree authoring
  operations like `sign`, `verify`, `fetch`.
- Omitted or `daemon` — requires a running daemon. Default for runtime services.
- `both` — may run either way; CLI prefers offline when safe.

The descriptor is the source of truth for availability. The CLI reads
service descriptors from installed bundles on disk and dispatches
offline-capable commands in-process. The CLI's offline handler registry
is only an implementation lookup — it does not decide which commands may
run offline.

Services avoid subprocess overhead for daemon-internal operations such as thread queries, bundle management, object fetches, vault calls, and health/status checks.
