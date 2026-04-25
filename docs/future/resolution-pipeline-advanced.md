```yaml
id: resolution-pipeline-advanced
title: "Resolution Pipeline — Advanced Path"
description: Deferred capabilities for the daemon-side resolution pipeline. Tracks what was intentionally cut from v1, what triggers each follow-up, and the order in which they should land.
category: future
tags: [engine, resolution, runtime, knowledge, sandbox]
version: "0.1.0"
status: planned
```

# Resolution Pipeline — Advanced Path

> **Status:** Planned. Builds on the v1 cut described in
> `.tmp/RESOLUTION-PIPELINE.md` (in-engine pipeline, tagged
> `ResolutionStepDecl` enum, hard-cut envelope v2, recursive aliases,
> `ChainHop` audit records, source_path cycle tracking).

## Why a follow-up doc

The v1 pipeline establishes the spine: an `ExecutionSchema` block on
each kind, a fixed set of built-in steps dispatched by enum, and a
`LaunchEnvelope v2` whose `resolution` field is always present. That
spine is enough to:

- delete three duplicate `resolve_extends_chain` implementations,
- give the future knowledge-runtime a place to receive pre-resolved DAGs,
- give the future node-sandboxed-execution attestation a per-hop trust
  surface to enforce against (`ChainHop.trust_class`).

Several otherwise-tempting features were cut to keep v1 reviewable
and to avoid building infrastructure ahead of the consumers that
justify it. This document tracks them, the trigger that should pull
each one back in, and the order they should land.

## What was cut, and why

| Item                                  | Why deferred                                                                 |
| ------------------------------------- | ---------------------------------------------------------------------------- |
| `resolve_provider` step               | Useful, but no current runtime is blocked on moving provider resolution out. The Python and Rust copies are isolated and small. Pull in when a 3rd consumer appears or when sandbox-wrap needs the provider in the envelope. |
| `preload_tool_schemas` step           | Same reasoning. Also wants tighter design around schema caching once tool schemas become large or come from remote runtimes. |
| Sandbox-wrap composition              | Requires node-attestation work to be meaningful. Without an attested environment to enforce against, sandbox-wrap is a config knob, not a security boundary. |
| Knowledge-runtime audit renderer      | Consumer of `ChainHop`; lives in knowledge-runtime, not in the engine. Lands with knowledge-runtime Phase 2. |
| Dynamic / plug-in resolution steps    | YAGNI for v1 (the tagged enum closes the door deliberately). Reopen only when an out-of-tree runtime needs a step the engine team won't take. |
| Per-step parallelism                  | Pipeline is fast enough today. Revisit once a step blocks on network I/O (sandbox attestation lookup, remote tool schema fetch). |
| Custom edge types beyond extends/refs | Knowledge may want `derived_from`, `superseded_by`, `produced_by`. Add as new `ResolutionEdgeType` variants when the runtime asks for them. |

## Roadmap (rough order, not dates)

### Phase A — Provider and tool-schema steps

**Trigger:** any of the following.
- A third runtime needs to read provider config (e.g., a future inference runtime).
- Sandbox-wrap composition needs `provider` / `model` in the envelope to choose a profile.
- The knowledge-runtime grows a "use the same provider as the parent directive" rule.

**Work:**
- Add `ResolveProvider` and `PreloadToolSchemas` enum variants.
- Move logic from directive-runtime bootstrap C3/C4 into engine steps.
- Define `step_outputs.resolve_provider` shape: `{ provider_id, model, endpoint }` — never the secret. Secrets stay in the daemon's keystore and are injected at spawn time, not in the envelope.
- Define `step_outputs.preload_tool_schemas` shape: `{ tool_ref → config_schema }`.
- Update directive-runtime to consume from envelope; delete bootstrap C3/C4.
- Tests for unknown provider, malformed provider config, missing tool, malformed tool schema.

**Risk:** secret leakage into the envelope. Tests must explicitly assert no secret material round-trips through `LaunchEnvelope`.

### Phase B — Knowledge-runtime audit renderer

**Trigger:** knowledge-runtime Phase 2 (compose) lands.

**Work:**
- knowledge-runtime consumes `envelope.resolution.ordered_refs:
  Vec<ChainHop>` and renders an audit sidecar (or inline annotation)
  explaining *why each item is in the prompt*: requested_id,
  resolved_ref, trust_class, alias chain.
- No engine changes. This is purely a consumer of the data v1 already
  produces.

### Phase C — Sandbox-wrap composition

**Trigger:** node-sandboxed-execution Phase 2 (sandbox engine
integration) lands and node attestation publishes
`isolation.engines[]`.

**Work:**
- Introduce `SandboxWrap` as a **post-pipeline** engine pass, not a
  `ResolutionStepDecl` variant. Steps produce data; sandbox-wrap
  mutates the spawn target.
- Inputs: aggregate `trust_class` over `ChainHop`s, the directive's
  declared sandbox profile, the node's attested engines.
- Output: replace the dispatched `SubprocessSpec` with a wrapped one
  (`nsjail --config … -- <inner>`).
- Refuse-to-dispatch when the directive demands a profile the node
  hasn't attested to. No silent downgrade.
- Compose order: `chain resolution → SubprocessSpec → sandbox_wrap(spec, attestation, profile) → SubprocessSpec → dispatch`.
- New error variants: `SandboxProfileUnsupported`, `TrustClassBelowProfileFloor`.

**Risk:** trust-class aggregation rules. Define explicitly: the
aggregate is the *weakest* hop in the chain. `trusted_system + unsigned
= unsigned`. Tests must cover mixed-tier extends chains.

### Phase D — New edge types

**Trigger:** knowledge-runtime starts asking for relationships beyond
`extends` / `references`.

**Work:**
- Extend `ResolutionEdgeType` with named variants
  (`DerivedFrom`, `SupersededBy`, `ProducedBy`, …).
- Each new edge type either lives in `references_edges` (lateral) or
  earns its own `Vec<…>` on `ResolutionOutput` if it has ordering
  semantics.
- Engine adds parameterized `ResolveReferences { field, edge_type, … }`
  variants per type, or a single generalized step. Decide based on
  whether per-type semantics diverge.

### Phase E — Per-step parallelism

**Trigger:** a step starts blocking on network I/O — most likely
remote tool-schema fetch or remote attestation lookup.

**Work:**
- Tag each `ResolutionStepDecl` variant as `Pure | NetworkBound`.
- Run `NetworkBound` steps concurrently when the dependency graph
  permits; serialize `Pure` steps.
- Add a `step_durations` map to `ResolutionOutput` for observability.

### Phase F — Plug-in / dynamic step registration (only if forced)

**Trigger:** an out-of-tree runtime needs a step the engine team will
not take in-tree. Strong default: do not enable.

**Work:**
- Reintroduce a `ResolutionStepRegistry`-style trait, but keep the
  built-in tagged enum as the fast path. Only the tail (unknown
  variants) routes through the registry.
- Plug-in steps must declare their schema and validate at registration
  time so unknown steps still fail at kind-schema parse time, not at
  first execution.
- Trust posture: plug-in steps run inside the daemon and can read any
  item the daemon can. Document this clearly; require signed
  registration.

## Non-goals (still)

- Backwards-compat envelope v1.
- Runtime-side fallback when `resolution` is missing or malformed —
  hard fail at the daemon, always.
- Inferring `extends` or `references` from item content. The fields
  are explicit; the pipeline only resolves what was declared.
- A general-purpose graph DSL on kind schemas. The pipeline is a flat
  ordered list of well-known steps; if you need a graph, write a
  step.

## Relationships to other future work

- **Knowledge runtime** (`knowledge-runtime.md`): the largest near-term
  consumer. Phase B audit renderer ships with knowledge-runtime
  Phase 2.
- **Node-sandboxed execution** (`node-sandboxed-execution.md`): Phase C
  sandbox-wrap is the engine half of the work that doc describes.
  `ChainHop.trust_class` is the contract between the two.
- **Lillux envelope evolution** (`lillux-envelope-evolution.md`):
  envelope v2 is the v1 cut here. Future envelope bumps (v3+) likely
  carry sandbox-wrap data and the `step_durations` map from Phase E.
- **Native runtimes** (`native-runtimes.md`): native runtimes get
  pre-resolved DAGs for free; they never duplicate `resolve_extends_chain`.
- **Encrypted shared intelligence**: per-hop `trust_class` is also the
  signal an encrypted-execution gate uses to decide whether a hop is
  allowed inside the sealed boundary.

## When to revisit this document

Whenever a triggered phase lands, move it from this doc into either
the main pipeline doc (if it became permanent v1-style infrastructure)
or its own implementation plan in `.tmp/`. This file should always
describe only what is *not yet* in the engine.
