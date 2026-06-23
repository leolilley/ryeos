<!-- ryeos:signed:2026-06-21T03:45:13Z:80fd59effc9e8f10dd1515d434e3e69b05f65bb8abb6ab56d1db27889e4e4500:yojHSetupgQZHGbfLdjlEmOdpf5+tDi6IEIGH8XAKQV+kLyQ1e42pwdob5ltozFt3w9cDhqv3BaJk49V+OmYCA==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
<!-- rye:signed:2026-06-03T03:31:14Z:3bc8749c6c939f390692f5d28b3656ab191417c20e225e730cce1ccb177ca5ab:3RVJUiuhhVGyT5saPSvgWlxZ4_qi9wdBStJJGqsLEALpLzy4N6ppGVOltzRv_C2GblaROaVdPAivwdl6_C6CCA:4b987fd4e40303ac -->
```yaml
category: ryeos/future/creative-code-spaces
name: seeded-signed-code-spaces
title: Seeded Portal Realms and First Signed Spaces
entry_type: concept-note
version: "0.2.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-06-03T00:00:00Z
description: Future product seeds and creation ladders for RyeOS portal realms where users create not only objects inside worlds, but reusable engines, renderers, frames, rules, and games that others can build on.
tags:
  - creative-code-spaces
  - portals
  - seeded-worlds
  - hosted-node
  - central-auth
  - cockpit
  - future-work
```

# Seeded Portal Realms and First Signed Spaces

## Purpose

This note describes the first future experiences that could make RyeOS digital spaces feel real.

The first seed should not be an abstract protocol demo, a finished first-party game, or a blank “make anything” prompt. It should be curated. The user starts from a specific vibe-coded premise, experiences the result, and then learns how the result is made from RyeOS layers.

The goal is to make it obvious that RyeOS does not supply all graphics, mechanics, or genres while also avoiding an overly general first impression. RyeOS should supply the first guided seed and the substrate where later layers can be created, inspected, signed, shared, forked, hosted, and recombined.

The first seed can still be a persistent hosted space that a non-builder can enter, experience, and contribute to. But the product lesson should be deeper: the world itself is made from composable layers that can later be user-created.

## Curated first seed: Memory Moth Room

The first guided experience should be one concrete world, not a menu of genres.

Seed premise:

```text
Make a tiny room where visitors type one sentence.
Each sentence becomes a glowing moth.
The moth remembers the sentence and slowly changes color over time.
```

Why this seed works:

- it is visually simple;
- it is emotionally legible;
- it demonstrates persistent objects and visitor traces;
- it makes memory visible;
- it can be rendered with primitives, particles, cards, or text;
- it gives the user an immediate action: type a sentence;
- it has enough layers to teach the RyeOS stack without requiring a full game.

The curated stack:

```text
Memory Moth Room
  object type: memory_moth
  object state: remembered sentence, color, age, creator/visitor trace
  rule: visitor_message_creates_moth
  behavior: moth color shifts as it ages
  renderer: simple glow-room/moth interpretation
  frame: memory-room template
  policy: private by default, visitor submissions allowed
  portal: enter from Cockpit/Home
```

The first run should proceed in this order:

```text
1. user opens Home/Cockpit, the RyeOS creation surface
2. user sees a vibe prompt, not a schema
3. RyeOS drafts the Memory Moth Room stack
4. user enters the portal and types a sentence
5. a moth appears and persists
6. user returns to Home/Cockpit
7. Home/Cockpit reveals the stack that made it happen
8. user edits one layer, such as color behavior or renderer style
9. user saves or forks the memory-room template
```

Home/Cockpit is the through-line of the experience. It plays the same role that a coding-agent interface plays for vibe coding today: start from intent, draft files/layers, preview or enter the result, inspect the generated stack, and iterate. For this first broad experience, Home should be implemented in the web Studio surface first. The terminal client can continue toward an OpenCode-style build loop and remain aligned through the same RyeOS daemon/actions/model contracts, but a TUI alone will not provide enough visual/spatial range to make the curated portal and stack reveal feel real. Later, the same Home role can also appear in native apps or richer terminal surfaces.

This matches the current UI direction: Rust owns the semantic Studio model and scene model, the browser renders Home/ambient/workspace, and browser effects call daemon services. The first Memory Moth wedge should therefore prefer adding semantic world/portal/layer state to the Studio model and rendering it in the web Home surface, rather than building an unrelated web app or hardcoding daemon fetches directly into animation code.

Only after this curated seed should RyeOS widen the surface toward gardens, fiction rooms, scene engines, codebase worlds, and arbitrary user-built frames.

## Two participant lanes

Portal realms should support two lanes without confusing their authority.

| Participant | Identity model | Typical actions |
|---|---|---|
| Builder/operator | RyeOS key/principal | signs world objects, frames, policies, portals, node descriptors, execution requests |
| Visitor/player | app-local realm principal | enters a portal UI, interacts with app state, submits prompts/actions through allowed app surfaces |

`central-auth` can gate app-local realm access for visitors inside one portal. It does not identify RyeOS principals and does not authorize RyeOS protocol execution. It is the door lock for one app realm, not the passport for RyeOS.

## Product loop

The first loop should be felt, not explained. For visitors and players:

```text
enter portal
  -> see persistent space
  -> prompt or interact
  -> AI proposes a change
  -> local/app preview appears
  -> accepted changes become signed world state
  -> hosted node keeps the space reachable
  -> Cockpit can inspect provenance
```

For builders, the loop expands:

```text
open Cockpit
  -> start from the curated Memory Moth Room
  -> inspect the memory_moth object type and moth instances
  -> inspect the visitor_message_creates_moth rule
  -> inspect the glow-room renderer/engine layer
  -> modify one behavior, style, or policy
  -> package the result as a reusable frame or world template
  -> open a portal into it
  -> publish, host, mirror, or remix the layer
```

The product should make the stack visible:

```text
RyeOS substrate
  -> user-built renderer/engine
  -> user-built frame/world type
  -> user-built game or portal
  -> user-built objects, lore, mods, and rules
  -> forks and new engines
```

The creation experience is the point, but it should be taught through one curated first seed. The seed spaces below are examples of where users can go after they understand the stack, not genres RyeOS must own.

## Seed 1: persistent garden frame

A garden is the simplest expression of digital space with gravity.

It does not need complex gameplay. It needs persistence, care, identity, and visible change over time. The key is not only “RyeOS ships a garden”; the key is that someone can define a garden frame that other people use to create their own gardens.

Reusable layers:

- plant object types;
- growth rules;
- visitor trace rules;
- seasonal/weather rules;
- garden renderer bindings;
- default portal/editor UI;
- policy template for private, shared, or public gardens.

Objects:

- plants;
- paths;
- weather states;
- light/time cycles;
- notes/memories;
- visitors' signed traces;
- generated structures;
- seasonal events.

Why it works:

- easy to understand;
- low simulation burden;
- emotionally legible;
- proves persistent hosted presence;
- demonstrates a reusable world type;
- lets users fork the frame, not only the garden contents;
- supports private, shared, and public modes;
- gives non-builders something to return to.

## Seed 2: collaborative fiction room

A collaborative fiction room lets people prompt a shared world into existence.

This should be framed as a reusable fiction engine: canon rules, branch rules, narrator policy, object types, and renderers can be authored by one creator and reused by others.

Objects:

- locations;
- characters;
- artifacts;
- events;
- rules of the setting;
- branches/alternate timelines;
- accepted canon heads;
- narrator/curator directives.

Why it works:

- text-first generation is tractable;
- history and authorship matter immediately;
- branches/conflicts can be product features;
- visitors can participate through realm auth;
- builders can inspect the world graph and policy;
- creators can publish alternate fiction engines with different canon, genre, or moderation rules.

## Seed 3: scene engine or renderer

A shared scene engine gives the fastest visible portal payoff. Three.js can be the first implementation, but the concept should be “a user-buildable renderer/engine layer,” not “RyeOS chooses the graphics engine forever.”

Objects:

- scene objects;
- materials;
- lights;
- labels;
- simple animations;
- particles;
- spatial relationships;
- style packs.

Loop:

```text
I created or forked a scene engine.
I defined what scene objects mean.
I described something.
It appeared through that engine.
It persisted as signed state.
Someone else built a game or object pack on top.
The Cockpit shows who made each layer and what accepted it.
```

This seed should avoid deep physics and arbitrary generated code. It should prove prompt-to-signed-state-to-rendered-dimension.

## Seed 4: world notebook

A world notebook is a hybrid of document, memory palace, graph, and portal.

It is also a good bridge for explaining that tools, directives, knowledge, schemas, renderers, and policies are just files until a space gives them meaning, dependencies, provenance, and an interface.

Objects:

- notes;
- generated rooms;
- source links;
- embedded tools/directives;
- task artifacts;
- project memories;
- timelines;
- explainers.

Why it works:

- naturally bridges builders and non-builders;
- can start as document/graph before 3D;
- turns knowledge into navigable space;
- fits RyeOS knowledge/directive workflows;
- teaches creation without requiring a polished 3D engine first.

## Seed 5: codebase world

A codebase world renders a software project as a place.

This is the most direct builder-facing example of crowdsourcing layers: one user can make a codebase renderer, another can make test-gate mechanics, another can make a release portal, and another can make agents that inhabit failing regions.

Objects:

- crates/packages/modules as regions;
- tools/directives as inhabitants;
- tests as gates;
- failures as visible weather/events;
- dependency edges as roads/tunnels;
- threads/jobs as moving activity;
- release/deploy portals.

Why it works:

- builders will understand it quickly;
- proves Cockpit as devtools;
- connects directly to current RyeOS substrate;
- makes AI coding feel like constructing and entering a place;
- shows that a “game engine” can be a project/workflow engine, not only a 3D renderer.

## Seed 6: music or memory space

Music and memory spaces are less operational but more emotionally direct.

Music space:

- composition as architecture;
- rhythm as pulse/light;
- melody as path;
- provenance as ownership/edition.

Memory space:

- a principal's RyeOS history as landscape;
- portals visited;
- worlds created;
- people/keys trusted;
- threads and changes as terrain.

These seeds help non-builders feel the “digital space with gravity” idea without needing to understand CAS or signatures first.

## Hosted node role

For seed realms, a hosted node should provide:

- always-on portal reachability;
- object closure availability;
- live subscriptions;
- accepted head publication;
- app-local visitor session support where needed;
- admitted jobs/directives;
- index/search for hosted worlds.

It should not provide:

- global RyeOS identity;
- global truth;
- uninspectable authority;
- hidden platform ownership of the world.

## First technical wedge

The smallest credible first seed is probably:

```text
Memory Moth Room
  -> memory_moth object type
  -> one persisted moth object created by visitor input
  -> visitor_message_creates_moth rule
  -> simple glow-room/moth renderer interpretation
  -> memory-room frame/world template
  -> signed portal object from Cockpit/Home
  -> one fork/remix of color behavior, renderer, or rule
  -> hosted node keeps the portal reachable if shared
```

A Three.js renderer can satisfy the “simple glow-room/moth renderer” step, but it should be presented as one layer among many possible layers. The demo should show the renderer/engine as a thing that can be inspected, forked, replaced, and used by other worlds.

Defer:

- arbitrary generated code;
- full multiplayer simulation;
- global discovery;
- true shared-daemon multi-tenancy;
- economic layer;
- deep federation;
- fully generalized frame authoring UI.

Do not defer the visible creation ladder. Even if early authoring uses simple files, templates, or narrow scaffolds, users need to see that they are creating the kind of space, not merely filling in content inside a space RyeOS already decided.

## Product principle

Start with spaces people can feel.

The protocol matters because it gives the space gravity, but the first portal should be legible before the protocol is explained.
