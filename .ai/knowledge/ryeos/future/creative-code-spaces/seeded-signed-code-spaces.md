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
description: Future product seeds for RyeOS portal realms where non-builders can enter persistent spaces while builders inspect, sign, remix, and host the underlying world state.
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

The first seed should not be an abstract protocol demo. It should be a portal realm: a persistent hosted space that a non-builder can enter, experience, and contribute to, while builders can inspect, fork, remix, and sign the underlying world state from the Cockpit.

## Two participant lanes

Portal realms should support two lanes without confusing their authority.

| Participant | Identity model | Typical actions |
|---|---|---|
| Builder/operator | RyeOS key/principal | signs world objects, frames, policies, portals, node descriptors, execution requests |
| Visitor/player | app-local realm principal | enters a portal UI, interacts with app state, submits prompts/actions through allowed app surfaces |

`central-auth` can gate app-local realm access for visitors inside one portal. It does not identify RyeOS principals and does not authorize RyeOS protocol execution. It is the door lock for one app realm, not the passport for RyeOS.

## Product loop

The first loop should be felt, not explained:

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
  -> inspect portal/world/frame
  -> sign or modify policy
  -> create objects/modules
  -> run validators/tools/directives
  -> host or mirror the portal
  -> enter the resulting dimension
```

## Seed 1: persistent garden

A garden is the simplest expression of digital space with gravity.

It does not need complex gameplay. It needs persistence, care, identity, and visible change over time.

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
- supports private, shared, and public modes;
- gives non-builders something to return to.

## Seed 2: collaborative fiction room

A collaborative fiction room lets people prompt a shared world into existence.

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
- builders can inspect the world graph and policy.

## Seed 3: shared Three.js scene

A shared Three.js scene gives the fastest visible portal payoff.

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
I described something.
It appeared in the scene.
It persisted.
Someone else entered the portal and saw it.
The Cockpit shows who made it and what accepted it.
```

This seed should avoid deep physics and arbitrary generated code. It should prove prompt-to-signed-state-to-rendered-dimension.

## Seed 4: world notebook

A world notebook is a hybrid of document, memory palace, graph, and portal.

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
- fits RyeOS knowledge/directive workflows.

## Seed 5: codebase world

A codebase world renders a software project as a place.

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
- makes AI coding feel like constructing and entering a place.

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
hosted Three.js portal realm
  -> one signed portal object
  -> one simple frame
  -> signed scene objects
  -> app-local visitor entry
  -> builder Cockpit inspection
  -> hosted node keeps it reachable
```

Defer:

- arbitrary generated code;
- full multiplayer simulation;
- global discovery;
- true shared-daemon multi-tenancy;
- economic layer;
- deep federation;
- generalized frame authoring UI.

## Product principle

Start with spaces people can feel.

The protocol matters because it gives the space gravity, but the first portal should be legible before the protocol is explained.
