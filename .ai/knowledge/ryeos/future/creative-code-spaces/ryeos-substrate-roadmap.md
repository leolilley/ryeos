<!-- rye:signed:2026-06-03T03:31:14Z:41823d75c9809ff76ae9e8e621014b981eee359eee6629cdf16592a39765c42f:kG1hveYOMEoTULljD7ppaUekyvjBwknaPiBOKNeQ2tSlj2wtSO_x4HIaFYKq9sNkWORKz3-gHaDjMlUEKKPrDg:4b987fd4e40303ac -->
```yaml
category: ryeos/future/creative-code-spaces
name: ryeos-substrate-roadmap
title: RyeOS Substrate Roadmap for Portals, Worlds, and Dimensions
entry_type: implementation_guide
version: "0.2.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-06-03T00:00:00Z
description: Future technical roadmap from principal-aware hosted user-space toward signed object worlds, portals, dimensions, hosted presence, object graph sync, remote execution, and federation.
tags:
  - creative-code-spaces
  - world-protocol
  - portals
  - hosted-node
  - signed-cas
  - object-graph-sync
  - remote-execution
  - federation
  - future-work
```

# RyeOS Substrate Roadmap for Portals, Worlds, and Dimensions

## Purpose

This roadmap describes the future RyeOS substrate needed for Cockpit portals, signed worlds, renderer/runtime dimensions, hosted presence, peer sync, and decentralized remote execution.

It does not restate implemented local/hosted-principal work. That work is assumed as the baseline. The remaining challenge is to turn the current RyeOS substrate into a protocol for signed digital spaces.

## Design stance

The system should be:

```text
local-first
signed
portable
verifiable
hostable
eventually federated
```

Hosted nodes are useful because they stay online. They are not authority roots. Centralized services, if used, should become caches, mirrors, indexers, or bootstrap helpers before they become dependencies.

## Phase 1: first-class signed objects

RyeOS needs first-class signed objects that are not necessarily executable items.

Examples:

```text
world:personal-garden
portal:blue-room
frame:threejs-scene-v0
dimension:graph-view-v0
policy:world-writers-v1
node-descriptor:hosted-node-a
admission:blue-room/head-42
event:tree-planted
snapshot:garden-checkpoint-17
```

These objects need the same seriousness as tools/directives:

- kind/schema declaration;
- canonical serialization;
- content hash identity;
- creator signature;
- dependency declarations;
- validation diagnostics;
- provenance/remix lineage;
- closure fetch/sync;
- trust classification.

Principle:

```text
Execution is one consumer of Rye objects. It is not what makes an object real.
```

## Phase 2: policy as signed CAS

Project, world, frame, and portal policy should become signed CAS objects.

Policy objects answer different questions:

| Policy | Question |
|---|---|
| Project policy | Who owns or can write this project-level space? |
| World policy | Which changes are accepted into this world? |
| Frame policy | Which object kinds, validators, renderers, and behaviors are valid? |
| Portal policy | Who can enter, submit, view, or administer this portal? |
| Node policy | What will this node host, execute, admit, or serve? |

Important separation:

```text
central-auth may decide whether a browser session can enter one portal UI.
world-policy decides what signed changes are accepted into the world.
node-local grants decide what this node will execute or admit.
```

Policy should cover:

- owners and maintainers;
- writers/contributors;
- app-local visitor rules;
- accepted frame versions;
- admission requirements;
- moderation/safety rules;
- visibility and export rules;
- sync/mirroring rules;
- execution grants and limits.

## Phase 3: signed node descriptors

A hosted node descriptor should be a signed/pinned CAS object, not a credential.

It should include:

- node fingerprint/public key;
- endpoint URL(s);
- supported protocol versions;
- supported bundles/frames/dimensions;
- resource/capability metadata;
- admission methods;
- operator/provider metadata if relevant;
- availability/role metadata;
- signature and issuance time.

The descriptor is a trust pin and discovery object. It does not grant authority by itself.

Future Cockpit UX should show hosted nodes as reach:

```text
local node
  -> pinned hosted node
  -> reachable portals
  -> supported frames
  -> sync status
  -> jobs running there
  -> object availability
```

## Phase 4: frame and dimension runtime

Frames are installable contracts for valid worlds/dimensions.

A frame package may contain:

- object schemas;
- validators;
- canonicalization rules;
- render primitive definitions;
- behavior primitive definitions;
- dimension bindings;
- directive/entity templates;
- migration rules;
- policy templates.

A dimension runtime interprets signed state through a frame.

Examples:

```text
world state + threejs frame      -> spatial scene
world state + tui frame          -> terminal cockpit
world state + graph frame        -> dependency map
world state + simulation frame   -> tick/replay runtime
world state + timeline frame     -> history view
```

The runtime should begin with declarative primitives and deterministic validators. Arbitrary generated code should come later, behind constrained hooks.

## Phase 5: portal model

A portal is a signed entry point into a world/dimension.

A portal object should identify:

- target world/ref/head;
- frame and dimension runtime;
- required object closure;
- hosted node descriptor(s), if any;
- portal policy;
- app-local realm auth binding, if any;
- launch parameters;
- provenance and owner.

Opening a portal should be a Cockpit operation:

```text
resolve portal
  -> inspect descriptor/policy/provenance
  -> fetch required closure
  -> verify signatures
  -> enter dimension
  -> submit signed changes or app-local actions as allowed
```

## Phase 6: hosted presence

Hosted nodes provide presence:

- keep a portal reachable;
- serve object closures;
- publish accepted heads;
- host live subscriptions;
- run admitted jobs;
- maintain indexes/search for worlds they host;
- mirror objects for availability.

They do not provide global identity, global naming, or global truth.

Near-term hosted products should prefer isolated hosted nodes/spaces over true shared-daemon multi-tenancy. Shared-daemon hosting needs stronger principal isolation, quotas, vault scoping, audit, and policy enforcement.

## Phase 7: object graph sync

Worlds need object-graph sync, not only project push/pull.

Required operations:

```text
pull object by hash
pull object plus dependency closure
pull accepted world head
pull frame/runtime closure
subscribe to admissions since cursor
publish signed ref/head update
diagnose missing dependencies
rebuild local indexes
```

Objects are immutable. Heads move by signed ref updates and policy. Conflicts become branches. Merges are policy decisions.

This is where registries become indexers and caches, not roots of truth.

## Phase 8: remote execution and durable jobs

Hosted worlds will need jobs that outlive a request/response cycle.

Future remote execution should use:

- signed execution requests;
- target-node-local grants;
- principal-aware authorization;
- durable job objects;
- signed result objects;
- event/mirror visibility in Cockpit;
- replay protection persistence;
- explicit vault/secret scoping.

The provider should not be in the hot path as an authority. The target node decides what it will run based on signed requests, pinned principals, node-local grants, and policy.

## Phase 9: federation and cluster path

Federation is advanced, not the first product requirement.

Add it when one hosted node is insufficient:

- mirrors for object availability;
- node-to-node sync;
- hosted failover;
- cluster routing;
- resource/hardware descriptors;
- optional fleet enrollment for managed pools;
- hardware/sandbox attestation when self-reporting is insufficient.

Fleet enrollment, hardware attestation, cluster vaults, and remote capability leases are hosted/operator scale features. They should not become the base RyeOS identity model.

## Cross-cutting future guardrails

- Do not make central-auth the RyeOS identity layer.
- Do not make hosted nodes the source of truth.
- Do not require global consensus to render or verify a world.
- Do not start with arbitrary generated code.
- Do not assume shared-daemon multi-tenancy before isolation work lands.
- Do not hide trust decisions in provider databases.

## Target shape

```text
Cockpit home dimension
  -> portals
  -> dimensions/worlds
  -> signed CAS state
  -> signed project/world/frame policy
  -> signed node descriptors
  -> hosted nodes for presence/reachability
  -> object graph sync
  -> remote jobs
  -> federation when needed
```
