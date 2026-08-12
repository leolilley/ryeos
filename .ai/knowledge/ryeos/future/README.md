<!-- ryeos:signed:2026-08-12T07:28:15Z:fcb2194f1a4380cae5b0952194125a0df6e09fc4e950211ace20f822bd9ee99d:FIW5DV5fni29lnaiGsfmpoYbbC9ClRF6gNxvnG8j4XUlxje3Z873hOQ2ivMhngTcuaUqs3jZpBB5PskRbbJyBg==:741a8bc609b398aaec0685e5aefb682faf5129a66bd192f888d23bb642c18eea -->
```yaml
category: ryeos/future
name: README
title: Future RyeOS Design Notes
description: Index and ownership rules for explicitly deferred RyeOS design work
entry_type: reference
version: "1.2.0"
```

# Future RyeOS Design Notes

This directory holds explicitly deferred design work. It should not contain
completed implementation plans or stale branch notes. A deferred note may
state the landed foundation it depends on, but only to draw the boundary around
what remains; current operating contracts belong in core knowledge.

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

- [`substrate-growth-roadmap.md`](substrate-growth-roadmap.md) — the current
  sequencing spine from single-node workload acceptance through managed
  workers, portable evidence, hosted principal boundaries, and federation;
- [`local-execution-roadmap.md`](local-execution-roadmap.md) — the current
  local-execution foundation and the exact remaining boundaries for sealed
  inference, generation capsules, and leased latency workers;
- [`chat-latency-investigation.md`](chat-latency-investigation.md) — measured
  chat-latency boundaries, optimization order, and the evidence gate for
  managed workers;
- [`content-addressed-managed-runtime-workers.md`](content-addressed-managed-runtime-workers.md)
  — the future leased-invocation class of the existing `worker` kind; it does
  not define a second kind or replace the fixed local-provider worker;
- [`sealed-local-inference.md`](sealed-local-inference.md) — qualification from
  the landed recorded local route to honestly re-derivable execution;
- [`generation-state-capsules.md`](generation-state-capsules.md) — provider-
  owned generation checkpoints over a meaning-blind capsule substrate;
- [`large-content-realization-follow-ons.md`](large-content-realization-follow-ons.md)
  — deferred storage/composition/operational work over the landed semantically
  blind large-content tier;
- [`provider-call-effect-records.md`](provider-call-effect-records.md) — the
  remaining provider replay measurement, certification, and retention/export
  work over the landed provider record boundary;
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
  and cost-series work beyond the landed exact two-run comparison;
- [`key-lifecycle.md`](key-lifecycle.md) — signer rotation, succession, and
  delegation for a substrate whose history is gated by revocation;
- [`reflexive-deployment.md`](reflexive-deployment.md) — activation sets as
  admitted programs; RyeOS sealing its own upgrades;
- [`resolution-pipeline-advanced.md`](resolution-pipeline-advanced.md) —
  criteria for adding new resolution stages; and
- [`ryeos-native-development-platform.md`](ryeos-native-development-platform.md)
  — RyeOS-native project hosting, checks, review, and release.
