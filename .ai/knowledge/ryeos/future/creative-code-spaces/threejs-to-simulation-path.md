<!-- ryeos:signed:2026-05-28T08:51:16Z:8cbb6eabd6188a76dfbc05078cd90b7480ac360b10ae6501b3f4b9a093b66898:BDTcQ/1UBHmXbwGz19wAACHnCj9BuvMFED2vqZLTDpShEgE3VXLj6gyxg5fOzJQVi8m/HV289NSDV5EPAXVFBA==:f168bc6752bd022d89a6778a8d2239b302f453d7e862770ed7ed1093c96363d1 -->
```yaml
category: ryeos/future
name: threejs-to-simulation-path
title: From Shared Three.js Canvas to Deeper Simulation Frames
entry_type: concept-note
version: "0.1.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-05-28T00:00:00Z
description: Staged product path for starting signed creative code spaces as a shared Three.js scene and later extending the same substrate into deeper simulation software.
tags:
  - creative-code-spaces
  - threejs
  - cockpit
  - signed-modules
  - simulation
  - frames
  - product-strategy
  - future-work
```

# From Shared Three.js Canvas to Deeper Simulation Frames

## Purpose

This note captures the staged product path for signed creative code spaces.

The first version should not try to be a full universe simulator, generic game engine, or arbitrary AI-code runtime. Start with the simplest compelling thing: a shared Three.js creative scene where users prompt visual objects and simple behaviors into existence, sign them, submit them to a node, and let other people pull, see, and remix them.

The deeper simulation platform should grow from that same substrate only after the visible creation loop is fun.

## Core strategy

Start shallow, keep the foundation real.

```text
v0: shared Three.js scene
  -> signed module data
  -> node attestation
  -> peer sync
  -> cockpit rendering
  -> remix/provenance

later: deeper simulation frames
  -> richer behavior primitives
  -> directive entities
  -> graph/time processes
  -> frame-specific simulation rules
  -> optional constrained executable hooks
```

The first version is not disposable. It is the shallow end of the same architecture.

## Stage 1: visual creative space

The first product is a visible creative canvas.

User-facing loop:

```text
open scene
prompt thing
see thing
tweak thing
sign thing
submit thing
share thing
remix thing
```

The backend loop stays small:

```text
AI-generated module data
  -> local validation
  -> creator signature
  -> CAS storage
  -> node validation
  -> node attestation
  -> peer sync
  -> Three.js renderer
```

Users create mostly visual modules:

- meshes and primitive objects;
- materials and color palettes;
- labels and glyph overlays;
- particle fields;
- lighting treatments;
- style packs;
- simple animations;
- simple spatial relationships;
- TUI and web projections.

Example module:

```yaml
frame: rye-canvas-v0
kind: scene_object
name: Blue Glass Moon
author: ed25519:creator_key
seed: b7d9
geometry:
  primitive: sphere
  radius: 180
transform:
  position: [4200, 0, 0]
material:
  kind: glass
  color: "#4abfff"
  roughness: 0.15
  opacity: 0.72
effects:
  - primitive: aurora_particles
    color: "#6fffd2"
    intensity: 0.35
behavior:
  kind: spin
  period: 90
remix_of: null
```

This is not arbitrary generated code. It is signed module data interpreted by a known frame runtime.

## Why Three.js first

Three.js is a good first visual substrate because it gives immediate payoff:

- visible 3D objects;
- camera movement;
- lights/materials;
- particle effects;
- simple orbital/spatial layouts;
- broad user familiarity;
- easy demos;
- enough expressive room for artists;
- no need to solve full simulation before people can create.

The goal is to get to the magic moment quickly:

```text
I described something and now I can see it in the shared scene.
```

## What the first backend actually needs

The first backend does not need deep simulation.

It needs:

1. A frame contract with a small schema.
2. A prompt-to-module generator.
3. A canonical module representation.
4. Local validation.
5. Creator signing.
6. CAS storage.
7. Node validation/attestation.
8. Peer sync.
9. A Three.js interpreter for accepted modules.
10. A cockpit inspector showing provenance, signatures, validation, and remix lineage.

Everything else can wait.

## Stage 2: structured behavior primitives

Once the visual loop works, add safe behavior primitives.

These remain data, not arbitrary code.

Examples:

```yaml
behavior:
  kind: orbit
  parent: red_star_hash
  radius: 1200
  period: 90
```

```yaml
behavior:
  kind: pulse
  target: material.emissive_intensity
  waveform: sine
  period: 37
  amplitude: 0.2
```

```yaml
behavior:
  kind: proximity_react
  target: comet_hash
  distance: 250
  action:
    set_material: glowing_blue
```

Useful primitive families:

- orbit;
- spin;
- pulse;
- path follow;
- proximity reaction;
- bounded particle emission;
- material transition;
- collision event;
- attachment/parenting;
- visibility/phase change;
- scheduled state transition.

This makes the scene feel alive while preserving deterministic replay and easy validation.

## Stage 3: frame-specific simulation

After people are creating and sharing, specific frames can deepen their own simulation rules.

For a cosmos frame:

- orbital constraints;
- heat/light bands;
- atmosphere tags;
- biome descriptors;
- artifact activation;
- event chains;
- civilization/story modules.

For a garden frame:

- growth cycles;
- light/water needs;
- pollination;
- decay;
- species interaction;
- seasonal palettes.

For a city frame:

- roads;
- districts;
- traffic;
- population flows;
- zoning;
- weather;
- economy tags.

For a dungeon frame:

- rooms;
- doors;
- enemies;
- treasures;
- traps;
- encounter rules;
- path connectivity.

Each frame can have its own validator and node admission policy. The substrate stays the same.

## Stage 4: directives as active entities

After objects and behavior primitives exist, directives can become active entities inside a frame.

A directive is naturally an entity because it already has:

- context;
- intent;
- permissions;
- model choice;
- limits;
- tool access;
- resumability;
- graph/process integration.

Entity examples:

- `worldsmith`: helps create valid modules;
- `artist`: generates visual styles and material variants;
- `curator`: reviews submissions and explains node policy;
- `scheduler`: advances frame time;
- `ecologist`: proposes biome/species changes;
- `civilization`: simulates a society's choices;
- `probe`: explores and reports remote regions.

The directive entity gets a scoped view of the scene/frame:

```text
selected object hashes
nearby accepted modules
frame contract
validator diagnostics
allowed actions
current tick/time window
player request
node policy
```

It then proposes module changes, runs validation, asks the player for approval, or submits to the node if authorized.

## Stage 5: graphs and time

Graphs can model processes that unfold over time.

The important refinement is: time can be graph replay. A world does not only advance through ticks; it records the graph of how each state came to exist.

Examples:

- validation pipeline;
- animation/event sequence;
- object lifecycle;
- biome growth;
- civilization decision loop;
- scheduled seasonal update;
- multi-agent collaboration;
- node moderation workflow.

The key is to represent time as frame-defined ticks and signed events, not uncontrolled wall-clock mutation.

```text
tick N
  -> graph evaluates state
  -> directive/entity proposes event
  -> event validates
  -> event is signed/attested
  -> peers sync
  -> cockpit renders update
```

Wall-clock time can schedule work, but shared state should derive from signed modules, ticks, seeds, and event chains.

Each graph node should be replayable when possible:

```text
graph node
  inputs: prior module hashes + frame hash + seed + directive version
  execution: graph step / directive action / validator gate
  outputs: proposed module/event hashes
  acceptance: signer + node attestation + validation result
```

That makes time inspectable. The cockpit can scrub the graph, replay the world from an earlier node, fork from a prior moment, compare branches, or explain why a visible object changed.

Time is therefore not just a numeric tick. It is the signed execution graph of the space.

## Stage 6: constrained executable hooks

Only add deeper executable logic after the data primitives hit real limits.

Possible progression:

1. Declarative modules only.
2. Behavior primitives.
3. Tiny deterministic expression language.
4. Sandboxed formula hooks.
5. Deterministic WASM/Lua-like modules.
6. Frame-specific plugin SDKs.

Do not start here. Arbitrary generated code introduces security, determinism, performance, compatibility, and moderation problems before the product loop is proven.

## The continuity principle

Every stage keeps the same core objects:

- frame contract;
- signed module;
- creator key;
- CAS hash;
- node validation;
- node attestation;
- peer sync;
- cockpit rendering;
- provenance/remix lineage.

At first, a module might be a sphere with a material. Later, a module might be a biome rule, civilization event, simulation primitive, or directive entity state. The protocol does not need to change its identity just because the modules get deeper.

## What to avoid early

Avoid making v0 depend on:

- real astrophysics;
- full game engine abstractions;
- arbitrary user-generated scripts;
- multiplayer action-game latency;
- global consensus;
- complex economy/governance;
- generic frame authoring UI;
- deep civilization simulation;
- unbounded shaders or browser code.

The first product should be almost embarrassingly simple:

```text
shared Three.js scene
AI creates signed modules
node accepts modules
peers pull modules
people remix modules
```

That is enough to test whether the creation loop is fun.

## Working product line

Use the seed to reveal the platform gradually.

Initial promise:

> Prompt objects into a shared 3D scene. Sign them, publish them, and remix what others make.

Underlying truth:

> Every visible thing is a signed module accepted by a frame node.

Long-term direction:

> The same signed module substrate can grow into deeper simulation frames, directive entities, graph-driven time, and user-defined spaces.
