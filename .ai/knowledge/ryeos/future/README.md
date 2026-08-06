<!-- ryeos:signed:2026-08-06T03:37:12Z:cbc777d3e3590a78c9cd37c5047f19ee25974e6c6c786470f7ecaee486063aa0:WazzF/LC+t28uO1pMR0WxdQBxIVIG6bgBGCXsYax4XDYPWUsVhbyDPa781smz93E7ssA1zSLGp5JBEhVm3E1DA==:8faa64a253fbe14970a4ef4f65ed9725c5163ba4defd74591599424c412efb96 -->
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

- [`chat-latency-investigation.md`](chat-latency-investigation.md) — measured
  chat-latency boundaries, optimization order, and the evidence gate for
  managed workers;
- [`mcp-server-auth.md`](mcp-server-auth.md) — authentication for any future
  non-local MCP transport;
- [`native-resume-snapshot-pinning.md`](native-resume-snapshot-pinning.md) —
  stronger native-resume policy and cross-node continuation semantics;
- [`node-operations.md`](node-operations.md) — criteria for a non-CLI operation
  taxonomy;
- [`project-ai-surface-registry.md`](project-ai-surface-registry.md) — a signed
  discoverable registry for deployable project surfaces;
- [`determinism-classes.md`](determinism-classes.md) — effect-class contract
  (sealed/recorded/live) and replay-or-divergence-proof semantics;
- [`execution-family-analytics.md`](execution-family-analytics.md) — seed-diff
  attribution and cost series over effective-definition families;
- [`key-lifecycle.md`](key-lifecycle.md) — signer rotation, succession, and
  delegation for a substrate whose history is gated by revocation;
- [`reflexive-deployment.md`](reflexive-deployment.md) — activation sets as
  admitted programs; RyeOS sealing its own upgrades;
- [`resolution-pipeline-advanced.md`](resolution-pipeline-advanced.md) —
  criteria for adding new resolution stages; and
- [`ryeos-native-development-platform.md`](ryeos-native-development-platform.md)
  — RyeOS-native project hosting, checks, review, and release.
