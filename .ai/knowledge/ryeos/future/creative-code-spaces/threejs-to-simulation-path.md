<!-- rye:signed:2026-06-03T03:31:14Z:d8bc3e01059956ee65f4c4e3e6fe2329a995792ab4d81955f9459cf9d5d7e909:h0V2gxpPl3HFlLr_vgOtzBB-bAnN9gtJLvcSrlFRUA5MkNJJbAgSMihAyN7kMul5JBOh-TDDVFWroT2twWkhAg:4b987fd4e40303ac -->
```yaml
category: ryeos/future/creative-code-spaces
name: threejs-to-simulation-path
title: From Three.js Dimensions to Simulation Frames
entry_type: concept-note
version: "0.2.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-06-03T00:00:00Z
description: Staged future path from a Three.js dimension over signed scene state toward behavior primitives, frame-specific simulation, directive inhabitants, replayable graph time, and constrained executable hooks.
tags:
  - creative-code-spaces
  - threejs
  - dimensions
  - simulation
  - frames
  - cockpit
  - future-work
```

# From Three.js Dimensions to Simulation Frames

## Purpose

This note describes the staged rendering/runtime path for RyeOS worlds.

Three.js is a strong first dimension because it makes signed state visible quickly. But Three.js is not the world itself. The world is signed state plus frame policy. Three.js is one projection/runtime interpretation of that state.

## Core strategy

Start with a dimension, not a universe simulator.

```text
Stage 1: Three.js dimension over signed scene objects
Stage 2: Declarative behavior primitives
Stage 3: Frame-specific simulation rules
Stage 4: Directive/tool entities as inhabitants
Stage 5: Replayable graph time
Stage 6: Constrained executable hooks
```

The first stage should be shallow enough to build, but real enough that it uses the same signed object and frame model that deeper simulations will need.

## Stage 1: Three.js dimension over signed scene objects

The first dimension renders signed scene objects through a small frame contract.

Object examples:

- mesh primitives;
- transforms;
- materials;
- lights;
- labels;
- particles;
- style packs;
- camera hints;
- simple animation descriptors.

Example object:

```yaml
kind: scene-object/v1
frame: threejs-scene-v0
name: Blue Glass Moon
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
  - primitive: aurora-particles
    color: "#6fffd2"
    intensity: 0.35
behavior:
  kind: spin
  period: 90
```

The Cockpit should show both the dimension and the truth behind it:

- rendered object;
- object hash;
- signer;
- frame;
- dependencies;
- validation state;
- hosted admission, if any;
- remix/provenance lineage.

## Stage 2: declarative behavior primitives

Once static objects work, add safe behavior primitives.

These remain data, not generated code:

- orbit;
- spin;
- pulse;
- path follow;
- proximity reaction;
- bounded particle emission;
- material transition;
- attachment/parenting;
- visibility/phase changes;
- scheduled state transitions.

Example:

```yaml
behavior:
  kind: orbit
  parent: red-star-hash
  radius: 1200
  period: 90
```

Validation should check bounds, determinism, supported primitive versions, and render/runtime budgets.

## Stage 3: frame-specific simulation rules

After the visible loop is fun, frames can deepen their own rules.

Examples:

### Garden frame

- growth cycles;
- light/water needs;
- pollination;
- decay;
- seasonal palettes;
- visitor traces.

### Fiction frame

- canon heads;
- character state;
- location consistency;
- branch/merge rules;
- narrator policy.

### Codebase frame

- dependency graph;
- test/build state;
- code ownership;
- release gates;
- directive/tool inhabitants.

### Cosmos frame

- orbits;
- heat/light bands;
- atmosphere tags;
- biome descriptors;
- artifact activation;
- civilization/story modules.

Each frame owns its validator and policy. The substrate stays the same: signed state, validation, admission, sync, projection.

## Stage 4: directive/tool entities as inhabitants

Directives and tools can become active entities inside a dimension/world.

Entity examples:

- `worldsmith` proposes valid objects;
- `artist` creates visual style variants;
- `curator` reviews admissions and explains policy;
- `scheduler` advances frame time;
- `ecologist` proposes garden/biome changes;
- `test-agent` investigates a codebase region;
- `probe` explores remote object graphs.

An entity should receive scoped context:

```text
selected object hashes
visible region/window
frame contract
validator diagnostics
allowed actions
current tick/time window
principal/request context
node/world policy
```

It should propose signed or signable outputs, not mutate world truth invisibly.

## Stage 5: replayable graph time

Worlds should record time as signed graph evolution.

```text
prior head
  -> event/proposal
  -> validator
  -> admission/policy
  -> new head
```

Graph time can represent:

- validation pipelines;
- object lifecycle;
- simulation ticks;
- seasonal updates;
- directive decisions;
- moderation workflows;
- multi-agent collaboration;
- branch/merge events.

The Cockpit can then replay, scrub, fork, compare, and explain state:

```text
Why did this tree turn silver?
  -> event 42: visitor submitted winter-light prompt
  -> event 43: artist directive generated material variant
  -> event 44: validator reduced particle budget
  -> event 45: hosted node admitted new head
```

## Stage 6: constrained executable hooks

Only add executable hooks when declarative primitives are not enough.

Possible progression:

1. tiny expression language;
2. sandboxed formulas;
3. deterministic WASM/Lua-like modules;
4. frame-specific plugin SDKs;
5. node-attested runtime plugins.

Any executable layer must enforce:

- explicit permissions;
- no secret access by default;
- no network/filesystem by default;
- bounded CPU/memory;
- deterministic seed/time inputs where replay matters;
- versioned runtime;
- validation parity between local and hosted nodes;
- signed provenance of inputs and outputs.

Do not start here. Arbitrary generated code is powerful, but it is not required for the first portal realm.

## Rendering failure mode

Dimensions must degrade gracefully.

If a client cannot render a primitive or frame, it should still show:

- object data;
- provenance;
- signatures;
- dependency status;
- placeholder geometry;
- required frame/runtime;
- fetch/install options.

The state remains real even when a dimension cannot fully render it.

## First vertical slice

A useful future vertical slice:

1. Define `threejs-scene-v0` frame.
2. Define signed scene-object schema.
3. Render hand-authored signed objects in Cockpit.
4. Show provenance/validation inspector.
5. Generate one object from a prompt as local draft.
6. Validate and sign it.
7. Host it through a portal on hosted-node.
8. Open the same portal from another browser/node.

This proves the path without deep simulation.
