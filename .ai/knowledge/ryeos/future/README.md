```yaml
category: ryeos/future
name: README
title: Future RyeOS Design Notes
description: Index and ownership rules for scheduled and deferred RyeOS design work
entry_type: reference
version: "1.4.0"
```

# Future RyeOS Design Notes

## Vision and discussion continuity

- [Governed adaptation and memory evidence](governed-adaptation-and-memory-evidence.md)
  — review of the shared learning/identity conversation, controlled memory
  experiments, opaque-state risks and boundaries of cryptographic proof.

- [Enduring working environment](enduring-working-environment.md) — synthesis
  of the papers and principal-centered product vision, user scope decisions,
  findings and open questions for resuming the discussion.
- [Experience to reusable knowledge](experience-to-reusable-knowledge.md) —
  proposed scoped adoption, selection, correction and privacy of lessons drawn
  from execution evidence; not an implemented memory service.

These are working discussion designs, not scheduled implementation promises.
The edited index and new notes are unsigned pending normal project signing.

## Scheduled and deferred work

This directory holds scheduled and explicitly deferred design work. It should
not contain completed implementation plans or stale branch notes. A note may
state the landed foundation it depends on, but only to draw the boundary around
what remains; current operating contracts belong in bundle or core knowledge.

Completed work from the June 2026 planning scratch files includes:

- `node/commands` replacing `node/verbs` as the CLI command surface;
- data-driven command registration policy;
- thin accepted/background `ryeos execute --async` launch;
- project `.ai` deployable surface sync and project schedule reconciliation;
- RyeOS UI Dimension v0 and RyeOS UI remotes services;
- bundle event chains, bundle projection helpers, and bundle outbox helpers;
- local direct install layout updates.
- hosted structured-worker execution, private homes, credential fencing,
  candidate publication/discard, and recovery;
- portable worker environments/checkpoints and explicit cross-site placement;
- fixed recorded local-inference provider execution and replay; and
- daemon-owned private exact workspaces for trusted disabled-isolation launches.

Scheduled and deferred entries are individual knowledge items in this
directory. Notes moved
from the former top-level `docs/future` tree include:

- [`substrate-growth-roadmap.md`](substrate-growth-roadmap.md) — the current
  sequencing spine from exact execution and portable hosted-worker placement
  through local inference, self-hosted implementation, and broader federation;
- [`local-execution-roadmap.md`](local-execution-roadmap.md) — the current
  local-execution foundation and scheduled path through one serious remote
  tinygrad profile, traces, training, sealed qualification, capsules, and
  offline export;
- [`self-hosted-implementation-campaigns.md`](self-hosted-implementation-campaigns.md)
  — bounded RyeOS-owned implementation work under a strict installed-host and
  private-candidate authority cut;
- [`chat-latency-investigation.md`](chat-latency-investigation.md) — measured
  chat-latency boundaries, optimization order, and the evidence gate for
  managed workers;
- [`content-addressed-managed-runtime-workers.md`](content-addressed-managed-runtime-workers.md)
  — the future leased-invocation class of the existing `worker` kind; it does
  not define a second kind or replace the fixed local-provider worker;
- [`sealed-local-inference.md`](sealed-local-inference.md) — qualification from
  the landed recorded local route to honestly re-derivable execution;
- [`generation-state-capsules.md`](generation-state-capsules.md) — provider-
  owned recorded or qualified generation checkpoints over a meaning-blind
  capsule substrate;
- [`execution-identity.md`](execution-identity.md) — portable program identity
  paired with exact runtime/device/numerics scope for honest recorded and sealed
  claims;
- [`large-content-realization-follow-ons.md`](large-content-realization-follow-ons.md)
  — storage/composition/operational work for artifacts, traces, corpora,
  training output, and capsules over the landed semantically blind tier;
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
  — RyeOS-native project hosting, checks, review, and release; and
- [`environment-build-system.md`](environment-build-system.md) — production,
  independent verification, publication, and simple reuse of portable
  execution environments through existing Tool, Graph, content, and retained-
  result authority.
