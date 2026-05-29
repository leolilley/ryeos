<!-- ryeos:signed:2026-05-29T03:56:05Z:6686ede3ed1a4b97897558f0e1562ac3652e1a50b68c88d5a8e74162cebf6012:i0C4yz04XJZKrLnLhcJ368HWLanQpOw4PnMiSfJB96Q1QztibCqOkSKEJufFywl1XBczKz88Zmz64YoW1PPLCg==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: ryeos/future
name: ryeos-substrate-roadmap
title: RyeOS Substrate Roadmap for Signed Creative Spaces
entry_type: implementation_guide
version: "0.1.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-05-28T00:00:00Z
description: Roadmap of RyeOS substrate capabilities needed to support signed creative spaces, shared world documents, node attestations, object-graph sync, replayable graph time, directive entities, and federation.
tags:
  - creative-code-spaces
  - world-protocol
  - signed-modules
  - object-graph-sync
  - node-attestation
  - federation
  - directives
  - cockpit
  - future-work
```

# RyeOS Substrate Roadmap for Signed Creative Spaces

## Purpose

This note captures what needs to happen inside RyeOS to support the signed creative spaces / world protocol direction.

The desired end state is not merely a Three.js toy or a single universe game. Those are seed products. The deeper direction requires RyeOS to evolve from a signed local/project execution system into a local-first signed object graph runtime with federation, attestations, replay, directive entities, and live projections.

## Current-to-future shift

RyeOS today is primarily shaped around:

```text
items
tools/directives
projects
bundles
execution
sync/remotes
signatures
```

Signed creative spaces need RyeOS to make this shape first-class:

```text
signed objects
object graphs
frame contracts
node attestations
accepted indexes
live subscriptions
replayable graph history
cockpit projections
directive entities
federated nodes
```

The key is not to discard Rye's current primitives, but to generalize and productize them as a distributed signed-world substrate.

## Design stance

Do not try to make the system perfectly decentralized first.

Make it:

```text
local-first
signed
portable
verifiable
node-attested
eventually federated
```

The alpha version can rely on one central/community node. That is acceptable as long as the node's authority is expressed as signed attestations over portable objects rather than hidden database state.

## 1. First-class signed data objects

Rye needs official non-executable signed objects that are as real as tools and directives.

Examples:

```text
frame:rye-canvas-v0
module:blue-glass-moon
admission:alpha-node/blue-glass-moon
region:leo/home-space
event:moon-created
style:cathedral-glass
projection:threejs/basic-scene
world:cosmos-alpha
```

These objects may not execute, but they still need the Rye item pipeline:

- kind/schema declaration;
- parser dispatch;
- signature verification;
- trust classification;
- composition/effective value;
- provenance;
- content hash;
- dependency declarations;
- sync closure;
- diagnostics.

Principle:

```text
Execution is one consumer of a Rye item. It is not what makes an item real.
```

This aligns with the broader future path of making non-executable items use the same verified effective-item substrate as executable items.

## 2. Frame contracts as installable packages

A frame is the constitution/runtime contract for a space.

Frames should become signed installable Rye packages/bundles. A frame package can include:

- module schemas;
- validators;
- render primitive definitions;
- behavior primitive definitions;
- directive/entity templates;
- node policy templates;
- projection definitions;
- example modules;
- migration/version compatibility metadata.

Examples:

```text
frame:rye-canvas-v0
frame:rye-cosmos-v0
frame:terminal-garden-v0
frame:codebase-world-v0
```

The product flow becomes:

```text
install frame
create module targeting frame
validate module locally
render module
submit to a node that accepts the frame
```

## 3. Canonical module representation

Signed modules need stable canonicalization.

Every module should declare:

- kind;
- frame hash/version;
- schema version;
- creator key;
- dependencies;
- provenance/remix fields;
- render/behavior primitive versions;
- optional region/world target;
- content license/reuse policy if relevant.

Canonicalization matters because the object hash is the identity of the module. The system should avoid hidden mutable state. If the module changes, its hash changes.

## 4. Object-graph sync instead of project-only sync

For creative spaces, sync cannot only mean pushing project state to a remote.

Rye needs object-graph sync operations:

```text
sync this object
sync this object and its dependency closure
sync this admitted region head
sync all admissions from node N since cursor C
sync this frame contract and validator closure
sync this world document
```

Example dependency graph:

```text
Blue Glass Moon
  -> frame:rye-canvas-v0
  -> material:glass-v1
  -> behavior:spin-v1
  -> region:leo-home
  -> admission:alpha-node/blue-glass-moon
```

A peer should be able to request:

```text
pull object hash abc123 with closure
```

and receive the minimum graph needed to validate, inspect, and render that object.

Required capabilities:

- dependency manifests;
- graph traversal;
- partial replication;
- closure fetch;
- sync cursors;
- object availability checks;
- missing dependency diagnostics;
- compact/rebuildable indexes.

## 5. Node attestation as a protocol

Central/community nodes should be attestors, not opaque owners of truth.

A node must be able to make a signed claim:

```text
I validated object X against frame Y with validator Z and policy P.
I accept it into collective C.
Here is my signature.
```

This requires an admission/attestation object.

Example shape:

```yaml
kind: admission
object: hash:abc123
frame: hash:def456
validator: hash:ghi789
policy: hash:jkl012
creator: ed25519:alice
node: ed25519:alpha_node
collective: rye-cosmos-alpha
result: accepted
diagnostics:
  schema: passed
  signature: valid
  render_budget: passed
  behavior_budget: passed
  policy: passed
signature: node_signature
```

The creator signature says `I authored this`. The node attestation says `This is accepted here`. This is not global consensus. It is node-local agreement, made portable and verifiable.

## 6. Validation as a first-class pipeline

Even when shared modules are just data, validation is critical.

The validation pipeline should be:

- deterministic;
- versioned;
- locally runnable;
- node runnable;
- diagnostic-rich;
- signature-aware;
- dependency-aware;
- policy-aware.

Initial validators should cover:

1. Schema validity.
2. Canonicalization.
3. Creator signature.
4. Frame compatibility.
5. Dependency availability/trust.
6. Render primitive budget.
7. Behavior primitive budget.
8. Region/collaboration policy.
9. Safety/moderation policy.

This is the CI system for shared reality.

## 7. Rich trust policy

Signing alone is not enough. The system needs layered trust.

Trust questions include:

- Who authored this module?
- Which node accepted it?
- Which frame was it validated against?
- Which validator/policy version was used?
- Is this signer trusted for this module type?
- Is this node trusted for this frame?
- Does the region require multiple signers?
- Is this module accepted only locally, experimentally, or publicly?

Trust policy should express rules like:

```text
trust node alpha for frame rye-canvas-v0
trust signer Mira for visual modules only
require multisig for region head changes
accept experimental modules locally but do not publish them
quarantine modules with unknown dependencies
```

Without this, distributed spaces become chaotic or too centralized.

## 8. Region heads and immutable events

Worlds need a model for accepted heads.

Recommended stance:

```text
objects are immutable
events are immutable
accepted heads move by signature/policy
conflicts become branches
branches can be merged by policy
```

Example:

```text
region head A
  -> Alice change

region head A
  -> Bob change
```

Both can exist. The region/node policy decides whether to accept one branch, accept both, require a curator merge, ask AI to propose a merge, or fork them into separate timelines.

This is Git-like and fits CAS/signing better than pretending there is one mutable shared object. CRDTs may help later for live collaborative editing, but canonical world truth should start with signed event/head graphs.

## 9. Conflict model and merge semantics

Conflict should not be treated as corruption. Conflict is branch creation.

Rye needs visible conflict objects:

```yaml
kind: region_conflict
region: hash:region123
base_head: hash:headA
branches:
  - hash:alice_change
  - hash:bob_change
policy: curator_merge_required
status: open
```

Merge can be manual, AI-assisted, policy-driven, multisig-approved, rejected, or kept as an alternate timeline.

The cockpit should show conflicts as graph branches, not just errors.

## 10. Live subscriptions

For the product to feel alive, manual pull is not enough.

Rye needs live subscriptions such as:

```text
subscribe to node admissions
subscribe to region head changes
subscribe to object graph updates
subscribe to validation status
subscribe to directive/entity events
```

Initial transport can be SSE or WebSocket.

Flow:

```text
creator submits module
node validates module
node emits admission event
subscribed clients pull object closure
clients verify signatures/attestation
cockpit renders update
```

The event stream announces change. CAS/object fetch remains the source of truth.

## 11. Discovery and indexes

CAS is excellent when the hash is known. Humans need discovery.

Rye needs rebuildable indexes:

- latest accepted modules by node;
- objects in region;
- modules by creator key;
- remix trees;
- dependency graph;
- admissions for object;
- available world documents;
- available frames;
- active directive entities;
- events since cursor;
- featured/curated objects.

Critical distinction:

```text
CAS is truth.
Indexes are convenience.
```

If an index lies or is stale, signatures and hashes still protect the content.

## 12. Replayable graph execution

The time model should be graph replay.

A graph run should record enough to answer:

- Why did this happen?
- What was the state before this?
- Can I replay this branch?
- Can I fork from here?
- Did my replay match the node's attested result?
- Which directive/entity made this decision?
- Which validator accepted this event?

Graph records should include:

- graph id/version;
- frame hash/version;
- input object hashes;
- directive/entity version;
- model/provider metadata where relevant;
- deterministic seed;
- tool calls/actions;
- outputs;
- validation results;
- signer;
- node attestation;
- child graph nodes;
- branch/merge metadata.

Time is therefore not merely a tick counter. It is the traversable execution graph of signed changes.

## 13. Event chains and state reconstruction

World state should be reconstructable from signed objects and events.

Possible event types:

- module created;
- module remixed;
- module admitted;
- region head advanced;
- graph step completed;
- directive proposed change;
- validator rejected change;
- branch forked;
- branch merged;
- node mirrored admission;
- frame upgraded.

The cockpit should support scrubbing history, replaying from an event, inspecting inputs/outputs, forking from a prior graph node, comparing alternate branches, and explaining visible state from provenance.

## 14. Directive entities and scoped context

To become inhabitants, directives need a formal scoped context model.

Not:

```text
here is the entire world/project
```

But:

```text
you are the ecologist for region X
you can see these object hashes
you can propose these module kinds
you can execute these tools
you have this budget
you cannot submit without user approval
current frame tick is N
node policy is P
```

A directive entity context should include:

- frame contract;
- selected object graph;
- event history window;
- visible region scope;
- allowed module kinds;
- permissions/tools;
- budget/model limits;
- output schema;
- submission policy;
- validator diagnostics;
- current tick/graph node.

This makes directives feel like agents inside the space rather than generic chatbots.

## 15. Directive/entity lifecycle

Directive entities need lifecycle state:

- created;
- active;
- sleeping;
- scheduled;
- waiting for approval;
- waiting for node validation;
- failed;
- retired;
- forked/remixed.

They may be represented as signed modules plus runtime state/event records.

Example entity module:

```yaml
kind: directive_entity
name: Blue Lantern Ecologist
directive: directive:frames/cosmos/ecologist
scope:
  region: hash:blue_lantern_region
permissions:
  can_propose:
    - biome_module
    - atmosphere_adjustment
  can_submit: false
budget:
  max_turns: 6
  model_tier: general
```

## 16. Cockpit projection APIs

The cockpit needs to become a projection layer for signed objects, not only a UI for Rye internals.

Required APIs/surfaces:

- open world document;
- list accepted modules;
- resolve object closure;
- get effective frame;
- stream node admissions;
- inspect provenance;
- inspect signatures/attestations;
- preview draft module;
- submit draft;
- show validation diagnostics;
- render supported projection data;
- fallback inspect unsupported modules;
- browse replay graph/timeline;
- chat with directive entity.

A projection is:

```text
effective signed module data
  -> interpreted under a frame
  -> rendered as Three.js / TUI / graph / inspector
```

The cockpit should not need to understand every future world. It needs extension points and fallback inspectors.

## 17. Projection and render primitives

For the first shared Three.js canvas, the frame should define a small primitive vocabulary:

- sphere;
- box;
- ring;
- line/curve;
- point cloud;
- particle system;
- material;
- light;
- label;
- glyph overlay;
- transform;
- camera hint;
- simple animation.

Unknown or unsupported primitives should degrade gracefully:

```text
I cannot render primitive X.
I can show the module data, provenance, and a placeholder.
```

Versioned render primitives are required for federation.

## 18. Versioning and compatibility

Distributed worlds require strict version discipline.

Every accepted module should declare:

- frame hash/version;
- schema version;
- render primitive version;
- behavior primitive version;
- validator version;
- dependency hashes;
- optional migration hints.

Clients need fallback behavior:

```text
I cannot render primitive X, but I can inspect the module.
I cannot validate frame Y, but I can show that node Z attested it.
I can render a simplified placeholder.
I need to fetch/install frame F to fully open this world.
```

Without this, federation will break as soon as independent nodes evolve.

## 19. Federation before full peer mesh

The eventual direction can be peer-to-peer, but the practical first path is federation through nodes.

Progression:

```text
Phase 1: local daemon + one alpha node
Phase 2: multiple trusted nodes
Phase 3: node-to-node federation
Phase 4: direct peer object exchange
Phase 5: richer discovery/routing/availability
```

Do not make NAT traversal or full global peer discovery a blocker for the seed product.

The system can still be peer-to-peer in spirit because objects are portable, signed, content-addressed, and verifiable.

## 20. Node-to-node federation

Later, nodes should be able to exchange:

- admitted module indexes;
- frame support metadata;
- region heads;
- creator profiles/provenance;
- mirrored admissions;
- rejection/quarantine metadata if policy allows;
- subscription cursors;
- availability information.

The long-term RyeOS cloud direction also includes storage-backed nodes and cluster federation. Creative spaces should be designed so node-to-node behavior can grow from simple mirroring into richer cluster coordination:

- object graph replication;
- storage/availability routing;
- frame bundle distribution;
- node admission federation;
- remote validation;
- graph or directive execution on a capable node;
- failover/archive nodes;
- shared cluster indexes.

The rule is not "no daemon-to-daemon". The rule is that daemon-to-daemon behavior must preserve signed provenance, explicit target/source identity, initiating principal, policy context, input/output object hashes, and replayable events.

Nodes may choose to mirror, ignore, quarantine, or revalidate objects from other nodes.

Example:

```text
Art Node accepts object X.
Hard Sci-Fi Node rejects object X for its canonical cosmos region.
Archive Node mirrors object X with Art Node's attestation.
User's cockpit can still inspect all three outcomes.
```

## 21. Availability and storage policy

CAS identity does not guarantee availability.

Rye needs policies for:

- pinning objects;
- pinning dependency closures;
- garbage collection;
- archival nodes;
- local cache eviction;
- offline access;
- missing dependency repair;
- popularity-based mirroring;
- creator-owned backups.

A world should be able to report unavailable dependencies and render placeholders or repair/fetch options.

## 22. Privacy and visibility

Not every module should be public immediately.

States:

- local draft;
- signed private;
- shared with collaborators;
- submitted to node;
- accepted public;
- rejected/quarantined;
- archived;
- deleted from index but still content-addressable if retained.

Signing does not mean publishing. The cockpit and node APIs should preserve this distinction.

## 23. Moderation and safety

Creative spaces need moderation without pretending every node has identical rules.

Moderation belongs in node/frame policy:

- allowed content classes;
- banned primitives/content tags;
- resource limits;
- trusted curator keys;
- appeal/review workflow;
- quarantine states;
- public/private visibility;
- age/safety profiles if needed.

Because content is signed, moderation can act on provenance without relying only on opaque accounts.

## 24. Sandboxing and executable hooks

The first version should prefer declarative data modules. Arbitrary generated code should be deferred.

When executable hooks are needed, add them gradually:

1. Behavior primitives.
2. Tiny deterministic expression language.
3. Sandboxed formulas.
4. Deterministic WASM/Lua-like modules.
5. Frame-specific plugin SDKs.

Any executable layer must enforce:

- no filesystem unless explicitly granted;
- no network unless explicitly granted;
- no secret access;
- deterministic seed/time inputs;
- bounded CPU/memory;
- versioned runtime;
- replay diagnostics;
- node validation parity.

## 25. Prompt-to-module generation

The AI generation loop should output structured modules, not freeform code.

Pipeline:

```text
user prompt
  -> frame-aware generator
  -> draft module data
  -> local validation
  -> repair loop if invalid
  -> local preview
  -> user approval
  -> creator signature
  -> node submission
```

The generator needs frame schema context, examples, allowed primitive lists, validation feedback, object/region context, and style/provenance context when remixing.

## 26. Admission UX

Validation and attestation should be visible and fun, not hidden infrastructure.

Cockpit status example:

```text
BLUE GLASS MOON / local draft

schema ........ passed
signature ..... valid
frame ......... rye-canvas-v0
render ........ passed
behavior ...... passed
policy ........ warning: large particle count reduced
node .......... accepted by alpha
```

This teaches users the model: local creation, signature, submission, validation, acceptance, sync, and remix.

## 27. Minimal RyeOS roadmap

### Phase A: signed object substrate

- First-class non-executable item path.
- Kinds for frame/module/admission/region/event/world/projection.
- Canonical hashing and provenance fields.
- Effective value resolution for non-executable items.
- Dependency declarations and closure diagnostics.

### Phase B: local frame/runtime demo

- Implement `rye-canvas-v0` as a frame package.
- Add a minimal Three.js primitive renderer/projection.
- Create hand-authored signed modules.
- Add cockpit world view and object inspector.

### Phase C: prompt-to-module

- Frame-aware AI generator for one object kind.
- Local validation.
- Local draft preview.
- Creator signing.

### Phase D: node admission

- Submit module to node.
- Node resolves dependency closure.
- Node validates.
- Node signs admission record.
- Cockpit shows diagnostics/status.

### Phase E: live sync

- Subscribe to node admissions.
- Second client pulls object closure.
- Verify creator signature and node attestation.
- Render same scene.

### Phase F: replay graph

- Graph execution records.
- Signed event chains.
- Cockpit timeline.
- Replay/fork from past node.

### Phase G: directive entities

- Scoped directive context package.
- Worldsmith/artist/curator entities.
- Entity proposes module changes.
- Validation/submission loop.

### Phase H: federation

- Multiple nodes.
- Trust policies.
- Node-to-node indexes.
- Region subscriptions.
- Portable world documents.

## 28. Minimal vertical slice

The smallest demo that proves the substrate:

1. Install/load `rye-canvas-v0`.
2. Hand-author two signed scene object modules.
3. Render them in a Three.js cockpit view.
4. Show module hash, creator signature, frame hash, and provenance in inspector.
5. Prompt-generate one new scene object as a local draft.
6. Validate it locally.
7. Sign it.
8. Submit it to an Alpha node.
9. Node signs an admission record.
10. Second client subscribes, pulls closure, verifies, renders.
11. User remixes the object; remix references the original hash.

This proves the loop without needing deep simulation.

## 29. What can wait

Do not block the seed product on:

- full p2p mesh networking;
- arbitrary code execution;
- real astrophysics;
- generic frame authoring UI;
- global consensus;
- complex economic layer;
- rich governance;
- VR;
- CRDT-based live multi-editing;
- deep civilization simulation;
- universal object portability across frames.

These may become important later, but they are not needed to prove the core creative loop.

## 30. Core missing distributed concepts

Compressed list of the RyeOS substrate gaps:

```text
1. Object graph sync, not only project sync.
2. Node attestation, not global consensus.
3. Signed heads/events, not mutable shared state.
4. Frame contracts, not hardcoded app logic.
5. Replayable graph history, not opaque time.
6. Trust policies, not one universal authority.
7. Live subscriptions, not manual pull only.
8. Cockpit projections, not fixed UI.
9. Directive entities, not generic chatbots.
10. Rebuildable indexes, not CAS-only discovery.
```

## 31. Working architecture diagram

```text
Creator / Cockpit
  -> prompt-to-module generator
  -> local draft module
  -> local validator
  -> creator signature
  -> CAS
  -> submit to node

Node
  -> fetch dependency closure
  -> run frame validator
  -> apply node policy
  -> sign admission record
  -> publish admission event/index

Peer / Cockpit
  -> subscribe to node
  -> receive admission event
  -> pull module closure from CAS/node
  -> verify creator signature
  -> verify/trust node attestation
  -> resolve frame projection
  -> render scene
  -> inspect provenance/replay graph
```

## 32. Product principle

The substrate work is large, but the first user-facing product must stay simple.

Show this:

```text
I typed "make a moon with blue glass forests."
It appeared in a Three.js scene.
I signed it.
The node accepted it.
You pulled it.
Now you are remixing it.
```

That one loop exercises the real substrate: AI generation, module data, signing, validation, attestation, sync, projection, provenance, and remix.

If that loop feels good, the deeper distributed world protocol becomes worth building.
