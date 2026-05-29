<!-- ryeos:signed:2026-05-28T08:14:50Z:e97eea47dca18e6cdd8581e1c3d626f7e222617cf81989bfd2a0107bf9a9a90d:gUxNlGtKeOzmvzv5RYsVa57hy9bf0DC3QmkDB6MuJhEPAkZiYBixsSEayK7JxL8mhDepHVRQYD7PVxFrK7H+Bw==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: ryeos/future
name: seeded-signed-code-spaces
title: Seeded Signed Code Spaces and Directive Entities
entry_type: concept-note
version: "0.1.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-05-28T00:00:00Z
description: Future product concept for seeded creative frames where users prompt signed modules into shared spaces, central/community nodes attest accepted data, and directives become intentional entities that perceive and act over time.
tags:
  - creative-code-spaces
  - signed-modules
  - frame-contracts
  - cockpit
  - directives
  - graphs
  - game-concept
  - future-work
```

# Seeded Signed Code Spaces and Directive Entities

## Purpose

This note captures a future RyeOS product direction: use Rye's existing primitives — keys, signatures, CAS, remotes, directives, graphs, daemon execution, and cockpit surfaces — as the substrate for shared creative spaces.

The first seed should be concrete enough for people to feel the loop. A strong candidate is a shared solar-system / cosmos frame where people prompt stars, planets, moons, artifacts, visual styles, and simulated entities into existence. The larger idea is not limited to universes: a universe is one seeded frame that teaches the deeper model of signed, validated, shareable AI-generated spaces.

## Core thesis

Rye should let a community define a frame, then let people fill that frame with signed modules.

```text
frame contract
  -> user prompt
  -> AI-generated module data
  -> local preview
  -> creator signature
  -> node validation
  -> node attestation
  -> peer sync
  -> cockpit rendering
  -> remix / fork / extension
```

The user-facing magic is the same fun people feel when building software with LLMs:

```text
imagine something
describe it
AI builds it
see it immediately
tweak it
share it
watch others remix it
```

The grounded implementation is not arbitrary generated code running on every peer. Start with shared signed data modules interpreted by a frame runtime.

## Simplest wedge: shared Three.js creative space

The first version should be even simpler than a full simulation platform: a Three.js scene where users prompt objects, visuals, and simple behaviors into existence.

The backend can stay small:

```text
signed module data
  -> CAS storage
  -> node validation / attestation
  -> peer sync
  -> Three.js scene renderer
```

The first product does not need a deep physics engine. It needs a visible canvas, a prompt box, signed object data, and a way for other people to pull and see the same scene.

The frame runtime can interpret declarative render/behavior primitives into Three.js constructs:

```text
sphere       -> THREE.Mesh + SphereGeometry
ring         -> THREE.RingGeometry / curve
orbit        -> deterministic transform over tick
particle     -> bounded particle system
material     -> approved material/palette descriptor
label/glyph   -> text sprite / TUI projection
style pack   -> renderer/theme defaults
```

This makes the first seed feel like a creative sandbox rather than a backend architecture. If creators want to go deeper, they can still inspect the signed modules, edit the generated data, build new frame primitives, or eventually write backend/runtime extensions. But the default experience is:

```text
open scene
prompt thing
see thing
sign thing
share thing
remix thing
```

That is enough to prove the core loop.

## Frames, not one hardcoded game

A frame is a signed contract for a creative space.

It defines:

- allowed module types;
- required schemas;
- render primitives;
- behavior primitives;
- validator rules;
- runtime limits;
- signing and admission policy;
- what a central/community node must check before attesting a module;
- how clients render and inspect accepted modules.

The first frame can be `rye-cosmos-v0`, but the same substrate should later allow frames such as terminal gardens, dungeons, cities, collaborative art spaces, procedural fiction worlds, simulation labs, or user-defined games.

## Seed world: Rye Cosmos Alpha

Do not start by selling the abstract platform. Seed it with a tangible world.

Rye Cosmos Alpha could be a shared AI-generated star garden where each creator has a signing key and a region. The player opens the cockpit, sees their region, prompts something into existence, previews it locally, signs it, submits it to the Alpha node, and watches the validation/attestation flow.

Example prompt:

```text
Create a lonely blue moon orbiting a dead planet, with glowing fungal forests on the dark side.
```

The AI generates constrained module data:

```yaml
frame: rye-cosmos-v0
kind: moon
name: Blue Lantern
author: ed25519:creator_key
seed: b7d9
orbit:
  parent: dead_planet_hash
  radius: 4200
  period: 900
body:
  radius: 180
  mass: 0.8
  atmosphere: thin
render:
  - primitive: sphere
    material: cold_basalt
  - primitive: bioluminescent_forest
    color: "#4abfff"
    hemisphere: dark_side
behavior:
  kind: deterministic_pulse
  target: forest_glow
  period: 37
remix_of: null
```

That module is data, but it is code-like data: it targets a specific frame contract, is interpreted by that frame runtime, and becomes visible in the cockpit.

## Signed modules are the shared object

For the first version, users should mostly share signed data modules, not arbitrary executable code.

Module categories may include:

- physical object specs: stars, planets, moons, artifacts;
- render specs: materials, glyph palettes, style packs, motion language;
- behavior specs: orbit, pulse, particle emission, seasonal color shift;
- event records: creation, collision, remix, accepted interaction;
- provenance records: author, dependencies, remix lineage;
- region records: ownership, collaborators, policy, accepted heads.

The actual executable code lives in the frame runtime and validator. The runtime knows how to interpret allowed module data. This keeps peer-to-peer sync, validation, security, and deterministic replay tractable.

Later versions can add small deterministic formulas or sandboxed hooks, but the first version should prove the loop with declarative modules.

## Central/community nodes are attestors

A node is not the sole owner of truth. It is a validator, curator, index, and sync hub for a particular collective space.

The creator signature says:

```text
I authored this module.
```

The node attestation says:

```text
This module passed this frame's validator and is accepted into this collective space.
```

Admission records should include:

- module hash;
- creator key;
- frame hash;
- validator hash/version;
- policy hash/version;
- validation result;
- node key;
- node signature.

Peers can trust the node's admission record, or pull the same frame/validator/module and re-run validation locally.

## Validation as physics CI

The central node runs a CI pipeline for generated reality.

Validation should check:

1. Schema correctness.
2. Signature validity.
3. Frame compatibility.
4. Dependency availability and trust.
5. Deterministic replay.
6. Render primitive budget.
7. Behavior primitive budget.
8. Region/collaboration policy.
9. Safety and moderation policy.

The cockpit should expose this as part of the fun:

```text
BLUE LANTERN / local draft

schema ........ passed
signature ..... valid
determinism ... passed
render web .... passed
render tui .... passed
policy ........ passed

accepted by rye-cosmos-alpha
```

## Artists are first-class creators

The frame should not treat graphics as an afterthought. Visual modules are part of the creative surface.

Artists can create:

- render styles;
- material palettes;
- glyph sets;
- cockpit themes;
- shader-like declarative materials;
- motion language;
- map projections;
- TUI representations;
- web/canvas representations;
- social/share-card styles.

The same object can have multiple projections:

```text
same signed moon
  -> orbital web view
  -> TUI glyph view
  -> scientific inspector
  -> poetic terminal panel
  -> debug provenance graph
```

This fits Rye's existing direction: a web cockpit for visual/spatial magic and a TUI cockpit for operator/hacker/provenance depth.

## Directives as entities

The next step is to treat directives not merely as workflows, but as intentional entities inside a frame.

A directive already has:

- context;
- intent;
- permissions;
- tool access;
- limits;
- model selection;
- resumability/forking potential;
- a relationship to graphs and execution state.

In the solar-system frame, a directive can become an entity that perceives a region, reasons with its assigned context, and acts through Rye.

Example entity types:

- `worldsmith`: proposes physical/module changes that fit the frame;
- `artist`: creates visual modules and style packs;
- `ecologist`: evolves biome/species modules over ticks;
- `curator`: reviews submissions against a node's aesthetic/policy;
- `oracle`: summarizes hidden interactions and suggests next prompts;
- `scheduler`: advances time and triggers graph steps;
- `civilization`: acts as a simulated society with memory and goals;
- `probe`: explores remote regions and reports discoveries.

The player can chat with these entities:

```text
player -> ecologist:
  What would happen if I warm Blue Lantern by 12 degrees?

ecologist -> player:
  The fungal forests would spread toward the terminator, but the thin atmosphere
  cannot hold enough moisture unless you add a geothermal vent module.

player -> ecologist:
  Create the smallest plausible geothermal vent module and preview it.
```

The directive sees the frame state, proposes module data, runs validation, and either returns a draft or submits it if authorized.

## Directives perceive through scoped space

An entity should not see the whole universe by default. It gets a scoped view.

Its context may include:

- the current region graph;
- selected objects and their module hashes;
- accepted frame contract;
- relevant validator diagnostics;
- local event history;
- allowed actions;
- current tick/time window;
- player instructions;
- node policy.

That scoped context is what makes a directive feel like an entity in the world rather than a generic chat bot.

## Graphs as temporal processes

Graphs can represent processes that unfold over time.

In a cosmos frame, graphs might model:

- planet cooling;
- atmosphere formation;
- orbital resonance drift;
- biome evolution;
- civilization development;
- artifact activation;
- node validation/admission pipelines;
- multi-agent collaboration;
- seasonal or scheduled events.

Each graph step can produce signed events or draft modules. A scheduler advances graph steps according to frame time, wall-clock time, manual triggers, or node policy.

```text
frame tick
  -> scheduler directive wakes
  -> graph step evaluates region state
  -> entity directive proposes event/module
  -> validator checks result
  -> accepted event appended to region chain
  -> peers sync and render update
```

This gives Rye's existing graph/scheduler model a game-world role without inventing a separate simulation system immediately.

## Time model

Time should be explicit and frame-defined.

Possible time layers:

- local preview time: fast, reversible, not collective;
- frame tick time: deterministic simulation ticks;
- node admission time: when a node attests an accepted object/event;
- narrative time: the time an entity/civilization believes it is experiencing;
- wall-clock time: only for scheduling and user experience, not deterministic physics.

Avoid relying on wall-clock time for shared simulation truth. Use ticks, seeds, event chains, and signed state transitions.

## Minimal vertical slice

The smallest useful demo should prove the whole spine:

1. Define `rye-cosmos-v0` as a frame contract.
2. Create a few hand-authored signed modules.
3. Render them in the cockpit.
4. Add prompt-to-local-draft generation for one module type.
5. Validate locally.
6. Sign the module.
7. Submit it to an Alpha node.
8. Have the node validate and countersign an admission record.
9. Let a second client pull and render the accepted module.
10. Add one directive entity that can inspect the region and propose a change.

The first directive entity can be simple: a `worldsmith` that sees selected objects, asks clarifying questions only when necessary, generates valid module data, and explains validation warnings.

## Deferred paths

Do not start with these unless usage demands them:

- arbitrary user-generated Python/Lua/Jai/WASM running on peers;
- real astrophysical simulation promises;
- global blockchain-style consensus;
- generic frame builder UI before one seeded frame is compelling;
- unbounded multi-agent civilization simulation;
- arbitrary shaders or browser code;
- global mutable shared state.

Keep the first system narrow: signed modules, frame validator, local preview, node attestation, peer sync, cockpit rendering, remix/provenance, and a few directive entities that make the space feel alive.

## Working phrase

This direction can be described as:

> signed, validated, shareable AI-generated code spaces.

For the seed product:

> Rye Cosmos Alpha: a shared AI-generated star garden where every object, visual style, and event is a signed module accepted by a community node and rendered in the cockpit.
