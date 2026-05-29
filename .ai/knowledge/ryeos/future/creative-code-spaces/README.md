<!-- ryeos:signed:2026-05-29T03:56:05Z:ebd7fd690e2f24a42d79f3019e899319d1ab6245a0fafcc1cb15dce5a52ccf16:7+IAFm+ilgTSZPf0Q2BRMfyxiQ1yO7YS9DLPcTo9fl3vrUaaPTt1U5Rn/BE6lJqDOUiDnH7kpu/d5jmgrPSnBA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->

```yaml
category: ryeos/future
name: creative-code-spaces-overview
title: Creative Code Spaces Overview
entry_type: overview
version: "0.1.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-05-28T00:00:00Z
description: Canonical overview for the RyeOS creative code spaces direction, linking the seed product, staged path, world-protocol endgame, and RyeOS substrate roadmap.
tags:
  - creative-code-spaces
  - signed-modules
  - world-protocol
  - cockpit
  - threejs
  - node-attestation
  - federation
  - future-work
```

# Creative Code Spaces Overview

## One-sentence thesis

Rye lets people create shared spaces out of signed AI-generated modules.

## What this is

Creative code spaces are a future RyeOS direction where a community defines a frame, users prompt modules into existence, Rye signs and validates those modules, nodes attest what belongs in a shared space, peers sync the accepted graph, and cockpit projections make the space visible.

The simplest product wedge is not a full universe simulator or arbitrary AI-code runtime. It is:

```text
shared Three.js scene
  -> signed module data
  -> node validation / attestation
  -> peer sync
  -> cockpit rendering
  -> remix / provenance
```

Users should feel the loop first:

```text
I typed "make a moon with blue glass forests."
It appeared in a Three.js scene.
I signed it.
The node accepted it.
You pulled it.
Now you are remixing it.
```

That one loop exercises the deeper substrate without forcing users to understand it up front.

## Why RyeOS is a fit

RyeOS already has much of the right foundation:

- keys and Ed25519 signatures;
- content-addressed storage;
- signed items and kind schemas;
- effective item resolution for executable and non-executable items;
- bundles that can provide new kinds;
- directives, tools, graphs, and scheduling;
- daemon/remotes/sync primitives;
- cockpit/web/TUI direction;
- provenance-oriented execution history.

The missing work is making the distributed world substrate first-class:

- signed creative modules;
- frame contracts;
- node admission/attestation records;
- object-graph sync;
- live subscriptions;
- replayable graph history;
- richer trust policy;
- cockpit projections;
- directive entities.

## Core vocabulary

### Frame

A frame is the constitution/runtime contract for a space.

It defines:

- allowed module kinds;
- schemas;
- render primitives;
- behavior primitives;
- validators;
- node policy;
- projection rules;
- directive/entity templates;
- time/replay model.

Examples:

```text
frame:rye-canvas-v0
frame:rye-cosmos-v0
frame:terminal-garden-v0
frame:codebase-world-v0
```

### Module

A module is the signed unit of creation.

For the first version, modules should mostly be declarative data, not arbitrary executable code.

Examples:

- scene object;
- material/style;
- behavior primitive;
- event;
- region;
- projection;
- directive entity;
- admission record.

### Node attestation

A node attestation is a signed claim by a community/central node:

```text
I validated object X against frame Y with validator Z and policy P.
I accept it into collective C.
```

This is not global consensus. It is portable, verifiable, node-local agreement.

### World document

A world document is the larger signed graph:

```text
frame contract
+ accepted modules
+ event history
+ directive entities
+ projections
+ node attestations
+ region heads
+ provenance/remix lineage
```

It is closer to a Git repo + game save + package + website + agent workspace than to a traditional file.

## Product path

### Stage 1: shared Three.js creative canvas

Start with a visible creative scene.

Users prompt objects, materials, labels, particles, lights, and simple movement into existence. The AI generates signed module data. The cockpit renders those modules as a Three.js scene.

This proves the immediate dopamine loop:

```text
prompt thing
see thing
sign thing
share thing
remix thing
```

### Stage 2: structured behavior primitives

Add safe data-driven behavior:

- orbit;
- spin;
- pulse;
- path follow;
- proximity reaction;
- bounded particle emission;
- material transition;
- scheduled state transition.

Still avoid arbitrary generated code.

### Stage 3: frame-specific simulation

Let frames deepen their own rules.

Examples:

- cosmos: orbits, heat/light bands, atmosphere tags, biomes;
- garden: growth cycles, pollination, decay;
- city: roads, districts, traffic, population flows;
- dungeon: rooms, enemies, treasures, traps.

### Stage 4: directive entities

Directives become inhabitants of spaces.

Examples:

- `worldsmith` creates valid modules;
- `artist` creates visual styles;
- `curator` explains node policy;
- `scheduler` advances frame time;
- `ecologist` proposes biome changes;
- `probe` explores remote regions.

They receive scoped world context, not the entire universe.

### Stage 5: graph replay time

Time becomes the replayable execution graph of signed changes.

Each graph node can record:

```text
inputs: prior module hashes + frame hash + seed + directive version
execution: graph step / directive action / validator gate
outputs: proposed module/event hashes
acceptance: signer + node attestation + validation result
```

The cockpit can then replay, inspect, fork, compare, and explain world history.

### Stage 6: constrained executable hooks

Only after declarative primitives hit real limits, add deeper execution:

1. tiny expression language;
2. sandboxed formulas;
3. deterministic WASM/Lua-like modules;
4. frame-specific plugin SDKs.

Do not start here.

## Endgame

The logical end is metaverse-shaped, but not one corporate 3D world.

The stronger idea is:

```text
not one metaverse
many signed worlds
```

At the far edge:

- every app is a frame;
- every file is a module;
- every assistant is a directive entity;
- every server is a node;
- every save is a signed event;
- every plugin is an attested module;
- every community is a node policy;
- every UI is a projection.

This is not a metaverse as a place. It is a metaverse as a software grammar.

## First implementation wedge

Recommended first PR:

> Canvas v0 signed seed modules render in Cockpit.

Scope:

1. Add `bundles/canvas/.ai/`.
2. Add signed non-executable kind schemas:
   - `frame`
   - `scene_module`
3. Add signed `frame:rye-canvas-v0`.
4. Add two hand-authored signed scene modules.
5. Add a UI endpoint that lists effective scene modules with provenance/trust.
6. Add a Cockpit `Canvas` tab.
7. Render the modules in a Three.js scene.
8. Show module ref, content hash, frame, signer, trust status, and source in an inspector.

This proves:

```text
signed module data
  -> effective item
  -> UI endpoint
  -> cockpit projection
  -> visible object
```

Explicit deferrals from the first PR:

- prompt-to-module generation;
- arbitrary generated code execution;
- node admission/countersigning;
- remote peer sync;
- directive entities;
- physics/simulation engine;
- generic frame authoring UI;
- full federation.

## Supporting notes

- [Seeded Signed Code Spaces and Directive Entities](file://./seeded-signed-code-spaces.md)
  - Core concept, seeded frames, signed modules, node attestations, artists, directives as entities, graphs/time.
- [From Shared Three.js Canvas to Deeper Simulation Frames](file://./threejs-to-simulation-path.md)
  - Staged path from a simple Three.js creative scene into behavior, simulation, directive entities, graph time, and executable hooks.
- [World Protocol Endgame for Signed Creative Spaces](file://./world-protocol-endgame.md)
  - Big direction: many signed worlds, world documents, frames as constitutions, nodes as city-states, cockpit as world devtools.
- [RyeOS Substrate Roadmap for Signed Creative Spaces](file://./ryeos-substrate-roadmap.md)
  - RyeOS implementation substrate: signed objects, object-graph sync, attestations, trust policy, replay graph, directive entities, cockpit projections, federation.

## Older RyeOS future context to absorb

The creative-code-spaces direction should absorb context from older RyeOS future notes. These notes are not just references; each contributes a concept that should shape the world-protocol design.

### Non-executable items must still be real Rye items

[Effective Non-Executable Items Substrate](file://../effective-non-executable-items.md) contributes the core principle:

> Execution is one consumer of an effective item. It is not what makes an item real.

Creative spaces rely on this. Frames, modules, admissions, regions, events, worlds, projections, style packs, and directive entity definitions are mostly non-executable, but they still need the full Rye item path:

```text
canonical ref
  -> kind schema
  -> parser
  -> signature/trust verification
  -> resolution/references
  -> composer
  -> effective value
  -> provenance/diagnostics
```

This should be treated as foundational, not optional.

### User/AI shorthand should compose into strict effective modules

[Source and Composed Descriptor Contracts](file://../source-vs-composed-contracts.md) contributes the contract layering model.

Creative modules may start as user- or AI-authored shorthand:

```yaml
kind: moon
style: blue-glass
orbit: nearby
```

but the composed effective module should be strict and normalized:

```yaml
kind: scene_object
geometry: ...
material: ...
transform: ...
behavior: ...
dependencies: ...
```

So creative spaces should distinguish:

- parser output guarantees;
- raw source module contract;
- composer input requirements;
- final composed module contract;
- runtime projection contract.

Canvas v0 can use simple identity-composed YAML, but richer frames will need this layered model.

### Long-term world history may need authenticated signature metadata

[Signed Envelope V2 Authenticated Metadata](file://../signed-envelope-v2-authenticated-metadata.md) contributes the provenance warning.

Current signatures prove body content. They do not authenticate every piece of visible signature-line metadata such as timestamp or fingerprint text.

That is probably fine for the first signed scene modules. But world histories and node attestations may eventually care about tamper-evident metadata:

- signed-at time;
- signer identity/key id;
- scope;
- claim type;
- frame hash;
- policy hash;
- validator hash;
- node/region context.

Early versions can use current signatures plus separate signed attestation objects. Audit-heavy world history may later require authenticated signed envelopes or stronger attestation payloads.

### Node admissions and head updates need crash-safe mutation

[Remote AI Sync Advanced Recovery Design](file://../remote-ai-sync-advanced-recovery.md) contributes the operational recovery model.

World/object sync is broader than project AI sync, but the same principles apply:

- journal before mutation;
- serialize head updates;
- do not advance refs until durable state exists;
- preserve enough recovery state to finish or roll back;
- keep accepted/deployed refs honest;
- expose operator-visible status for in-progress, failed, recovered, and rolled-back operations.

For creative spaces this maps to:

```text
module submitted
  -> node validates
  -> node stores dependency closure
  -> node journals admission
  -> node signs attestation
  -> node advances accepted index/head
```

A node should not attest or publish a world head that it cannot recover after a crash.

### Frame bundles are trust-boundary objects

[Shared Bundle Registration Validator Advanced Path](file://../shared-bundle-registration-validator.md) contributes the bundle trust-boundary warning.

Frames will likely ship as signed installable bundles containing schemas, validators, render primitives, projection definitions, and directive entity templates. If daemon bootstrap, bundle planning, cockpit projection, and node validation disagree about which frame bundles are installed/trusted, peers can validate or render different realities.

Creative spaces should preserve the principle:

```text
bundle/frame registration validation must be shared, strict, and fail-closed
```

### Projection routes, assets, and streams must preserve daemon composition

[Route Composition and Reload Context](file://../route-composition-reload-context.md) contributes the composition-root rule.

Creative spaces will add or depend on:

- UI routes;
- node admission streams;
- frame projection endpoints;
- static renderer assets;
- browser-session auth;
- possibly frame-provided projection extensions.

Route reload must preserve the same composed descriptors, auth verifiers, stream sources, and static providers as daemon startup. Do not let UI/frame-specific route state disappear by rebuilding with API-only defaults.

### Remote directive entities need explicit target/preflight/provenance

[Target-Site Forwarding Future Advanced Paths](file://../target-site-forwarding-phase3.md) contributes remote execution caution.

Later, directive entities or graph processes may act against remote nodes:

- remote validator runs against a module;
- remote graph executes a simulation step;
- remote directive entity proposes a change;
- remote-to-remote federation mirrors an admission.

Those paths should preserve:

- explicit target node/site identity;
- local preflight before remote I/O;
- clear forwarding of operations and inputs;
- target-aware validation semantics;
- structured provenance for local vs remote authority.

### Shared execution should wait until real caller pressure exists

[Shared Engine-Backed Offline Executor](file://../shared-offline-executor.md) contributes the restraint principle.

Canvas v0 does not need a shared execution abstraction. But if frame validators, local runtime hooks, CLI commands, daemon workers, and offline tools start needing identical launch semantics, execution should be centralized below those callers rather than reimplementing descriptor parsing and dispatch repeatedly.

Until then, keep the first creative-space slice declarative and avoid building a broad executor too early.

## Remote-execution implementation context to absorb

The temporary remote-execution implementation notes in `.tmp/remote-execution-impl/` contain additional distributed-systems context that matters for creative spaces. They are not creative-space docs, but several principles should carry forward.

### v1 remotes are operator-trusted, not multi-tenant

The remote-execution v1 trust boundary is explicit:

```text
v1 remote execution is for operator-trusted remotes, not mutually untrusted tenants.
CAS is shared/global within a node; capability checks protect access, not storage partitioning.
Vault is a single shared store in v1; capability checks protect mutation/listing, not per-principal isolation.
```

Creative spaces should not accidentally inherit this as a permanent product promise. Canvas v0 and early alpha nodes can be operator-trusted, but public creative nodes eventually need stronger boundaries:

- per-principal CAS attribution/manifests;
- quota/GC ownership;
- per-principal vault partitioning if secrets are involved;
- clearer tenant isolation;
- node policy that distinguishes trusted operators from public creators.

### Clean-base conflict detection maps to region heads and graph branches

Remote execution's pull/apply design requires the exact pushed base manifest. Recomputing the base later is wrong because local state may have drifted.

Creative spaces have the same shape:

```text
base region head
  -> proposed module/event
  -> validation/admission
  -> advance head only if base still matches policy
```

If the base changed, do not blindly overwrite. Create a branch/conflict object, ask for merge policy, or let the cockpit show alternate graph branches.

### Async remote work requires a real job/result model, not polling bolted on

Phase 5 notes that long-running remote jobs need a persisted push-base journal and a real async job/result model. Polling a synchronous orchestrator is not enough.

Creative spaces will eventually have long-running validators, directive entities, graph simulations, and node admissions. Those should be represented as durable jobs with:

- persisted base/input hashes;
- status;
- result object hashes;
- validation diagnostics;
- recovery metadata;
- cancellation policy;
- replay/provenance links.

Do not fake this with ad hoc polling once admissions or simulations outlive the user's current request.

### Bundle sync becomes frame distribution

Remote-execution notes defer CAS-sourced bundle export/install until operators need cross-node bundle deployment.

Creative spaces are likely to trigger this sooner because frames are bundles:

```text
frame schemas
validators
render primitives
projection assets
directive entity templates
example modules
```

A node that accepts `frame:rye-canvas-v0` needs peers to fetch the exact frame bundle/closure that validated and rendered the modules. CAS-sourced bundle sync is therefore part of the later frame federation story.

### Request-scoped trust overlays may matter for public submissions

Remote execution defers request-scoped trust overlays because the engine trust store is boot-time.

Creative nodes may eventually need to validate submissions from creators whose keys are not permanently pinned in the node's global trust store, or whose trust is scoped to one frame/region/admission request.

This does not block Canvas v0, but public creative nodes may need:

- per-request trust overlays;
- scoped signer authority;
- temporary submission keys;
- policy-bound trust rather than global trust pins.

### CAS debugging and arbitrary hash pull are product primitives for worlds

Remote execution treats arbitrary remote hash pull as an operator debugging convenience. In creative spaces, pulling object hashes and dependency closures becomes a core product primitive.

World tooling needs:

```text
pull this module hash
pull this module plus closure
pull this admission and target
pull this frame bundle closure
pull this region head graph
```

The existing `objects_get` shape is a seed, but world sync needs closure-aware object graph pulls.

### Chunked transfer and availability become important with media-heavy worlds

Remote execution defers chunked transfer until blobs exceed roughly 100 MB or unreliable links cause push/pull failures.

Creative spaces may hit that faster because worlds can include textures, models, generated assets, videos, audio, or large simulation artifacts. The world protocol should eventually plan for:

- chunked upload/download;
- resumable transfer;
- pinned dependency closures;
- missing dependency diagnostics;
- archival/mirror nodes;
- placeholder rendering when heavy assets are unavailable.

### mTLS/TLS pinning remains secondary to signing keys, but may be required by deployments

Remote execution's stance is that HTTPS + TOFU + signed requests is enough for v1; the signing key remains identity, not the TLS cert.

Creative spaces can keep the same stance initially. But compliance-heavy or public nodes may need mTLS/TLS pinning as a transport hardening layer without changing the underlying identity model.

### Persistent remote workspaces map to long-lived node simulations

Remote execution defers per-principal persistent workspaces until checkout cost dominates runtime.

Creative spaces may later need long-lived per-principal or per-region runtime state for simulations, validators, or directive entities. The same caution applies: do not add persistent workspaces until temp/CAS materialization cost is the actual bottleneck.

### Typed `HandlerContext` matters for principal-aware creative APIs

Remote-execution notes identify `_caller_fingerprint` / `_caller_scopes` injection as a pattern that becomes brittle as principal-aware handlers grow.

Creative spaces will add many principal-aware APIs:

- submit module;
- sign draft;
- admit module;
- advance region head;
- list private drafts;
- subscribe to node events;
- run directive entity;
- inspect collaborator state.

A typed `HandlerContext` becomes more important as this surface grows.

### Registry with namespace claims maps to multi-publisher frames

Remote execution defers a registry with namespace claims until multiple teams publish bundles.

Creative spaces likely create that pressure once people publish frames, projection packs, validators, directive entities, and style packs. This is not needed for the first alpha, but it is part of the multi-publisher world protocol.

### Daemon-to-daemon and cluster federation are part of the long-term cloud path

The older remote-execution notes treated daemon-to-daemon forwarding as excluded, but that decision has since been superseded. RyeOS is moving toward fuller cloud functionality: storage-backed nodes, node-to-node behavior, and cluster federation.

Creative spaces should absorb the newer direction, not the older exclusion. Long-term world nodes may need to coordinate directly for:

- object graph replication;
- storage/availability mirroring;
- frame bundle distribution;
- node admission federation;
- remote validation;
- graph or directive execution on a capable node;
- cluster-local routing;
- failover and archival nodes;
- shared indexes across a cluster.

The caution that remains useful is about authority and provenance. Daemon-to-daemon behavior should be explicit, signed, policy-bound, and inspectable. A node should not silently launder authority by forwarding arbitrary execution without preserving:

- source node identity;
- target node identity;
- initiating principal;
- frame/policy context;
- input object hashes;
- output object hashes;
- validation/admission records;
- replayable graph events.

So the updated principle is:

```text
daemon-to-daemon is allowed for cloud/cluster federation,
but every hop must preserve signed provenance and policy authority.
```

## Working phrases

Useful framing candidates:

- signed creative spaces;
- signed worlds;
- world protocol;
- AI-generated spaces;
- Git for living worlds;
- world documents;
- the operating system for user-created software worlds;
- a universe of signed, remixable software spaces.

Most grounded phrase:

> Rye lets people create shared spaces out of signed AI-generated modules.
