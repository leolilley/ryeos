<!-- ryeos:signed:2026-07-21T00:24:56Z:5c25d8ab80379a9d6369b70cafdf8032e892f265a9b8aae37521d9adc0a7280b:fvborgWUxRwyVzayzck4oNKEoG7uENJahGDmnKqf/anuat4V6EyEx186uzdOOIqIjBUaNmJpMsdjQEVWicP8Dw==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: README
title: Future RyeOS Design Notes
description: Index and ownership rules for explicitly deferred RyeOS design work
entry_type: reference
version: "1.0.0"
```

# Future RyeOS Design Notes

This directory holds explicitly deferred design work. It should not contain
completed implementation plans or stale branch notes.

Completed work from the June 2026 planning scratch files includes:

- `node/commands` replacing `node/verbs` as the CLI command surface;
- data-driven command registration policy;
- thin accepted/background `ryeos execute --async` launch;
- project `.ai` deployable surface sync and project schedule reconciliation;
- RyeOS UI Dimension v0 and RyeOS UI remotes services;
- bundle event chains, bundle projection helpers, and bundle outbox helpers;
- local direct install layout updates.

Deferred entries are individual knowledge items in this directory. Notes moved
from the former top-level `docs/future` tree include:

- [`mcp-server-auth.md`](mcp-server-auth.md) — authentication for any future
  non-local MCP transport;
- [`native-resume-snapshot-pinning.md`](native-resume-snapshot-pinning.md) —
  stronger native-resume policy and cross-node continuation semantics;
- [`node-operations.md`](node-operations.md) — criteria for a non-CLI operation
  taxonomy;
- [`project-ai-surface-registry.md`](project-ai-surface-registry.md) — a signed
  discoverable registry for deployable project surfaces;
- [`resolution-pipeline-advanced.md`](resolution-pipeline-advanced.md) —
  criteria for adding new resolution stages; and
- [`ryeos-native-development-platform.md`](ryeos-native-development-platform.md)
  — RyeOS-native project hosting, checks, review, and release.
