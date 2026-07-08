<!-- ryeos:signed:2026-06-21T03:45:13Z:a726e6720cacccab3285f3bdc34cb0b6abe7107809a88f96c47caf34560193c4:ewtST9iAdK9vHZwph6xwdF06nx7wbAPWArC25/s9JPmE/MkpBhNLueLq4lEUxFUjc1OSQ8+EgT0sP1wyxvSqBQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
<!-- rye:signed:2026-06-03T03:31:14Z:029d844e16f5bd8aca351ee88439aceac8f86745be195ff2019487e19f3ba434:oP35NL5kiEQmuc01oeZ8bUMSbD4yrhMZQ73dTUJpLhSjl5HNbEOmbOeaQeDOJxsoIW_053I_8D4pg06JSEZWCg:4b987fd4e40303ac -->
```yaml
category: ryeos/future/creative-code-spaces
name: README
title: Creative Code Spaces Overview
entry_type: overview
version: "0.2.0"
author: amp
created_at: 2026-05-28T00:00:00Z
updated_at: 2026-06-03T00:00:00Z
description: Future overview for RyeOS as an OS-inside-the-OS for signed digital spaces, cockpit home dimensions, portals, worlds, hosted presence, and multiple renderer/runtime dimensions over signed CAS state.
tags:
  - creative-code-spaces
  - cockpit
  - portals
  - dimensions
  - worlds
  - hosted-node
  - signed-cas
  - future-work
```

# Creative Code Spaces Overview

## Thesis

RyeOS can become an OS-inside-the-OS for signed digital spaces.

It should not be one game, one engine, one renderer, or one metaverse. It should be the bare creation substrate where people define the layers that make digital spaces possible: object types, rules, renderers, engines, editors, worlds, portals, and games.

A person should be able to start with an almost empty signed space and make the kind of space itself. Someone else can build a renderer. Another person can build a game on top of that renderer. Others can add objects, lore, tools, rules, art packs, simulations, editors, or entire new engines. Each layer remains inspectable, signed, portable, forkable, and remixable.

The Cockpit is the home dimension: the place where a RyeOS principal sees their projects, worlds, portals, hosted nodes, trust pins, running jobs, recent changes, and memory. From there, a user opens portals into dimensions and worlds.

A dimension is not the source of truth. It is a renderer, runtime, or state interpretation over signed CAS objects. The same world state can appear as a Three.js scene, TUI, graph, timeline, dashboard, editor, simulation, or future immersive view. Those dimensions are themselves things people can create, publish, fork, and build on.

The future product loop is:

```text
start with a blank signed space
  -> create an object, rule, renderer, frame, or engine
  -> Rye validates and stores it as signed state
  -> enter through a portal or editor dimension
  -> package a reusable layer others can build on
  -> inspect, remix, fork, host, sync, or share it
```

This is not only a game platform or developer tool. It is a way to crowdsource the making of digital places themselves while keeping provenance, ownership, dependencies, and history visible.

## Baseline assumed

This future direction assumes RyeOS already has the local/hosted seams needed to continue:

- principal-aware hosted user-space exists;
- hosted-node exists outside `standard` as the bundle for always-on hosted RyeOS behavior;
- central-auth exists as app-local realm auth, not RyeOS identity;
- local RyeOS UI/Cockpit project registration and launch flows exist as current substrate.

Those pieces are not the future described here. They are the footing.

## Core vocabulary

### RyeOS principal

A RyeOS principal is a cryptographic identity: a key/fingerprint that can sign RyeOS protocol objects, world changes, policies, descriptors, and execution requests.

This is the identity layer that matters for RyeOS authority.

### Realm principal

A realm principal is an app-local visitor or browser session identity inside one portal/app realm.

`central-auth` can provide realm auth. It can decide whether a human browser session may enter a particular app or portal UI. It is not RyeOS global identity and must not authorize RyeOS protocol execution.

### Cockpit

The Cockpit is the home dimension of RyeOS.

It is the primary creation surface, similar in role to how OpenCode, Codex, Claude Code, or another coding-agent interface is the surface for vibe coding today. The broad first experience should be web-first because it needs visual portals, stack inspection, spatial preview, and layer editing in one surface. The TUI can keep developing toward the OpenCode-style build loop and should connect to RyeOS through the same daemon/actions/model path, but it is not enough by itself to sell the wide creative-space experience. Later, the same Home role can also appear in native apps or richer terminal surfaces.

It should show:

- the active RyeOS principal;
- local and hosted spaces;
- portals;
- projects and worlds;
- remotes/hosted nodes as reachability;
- signed node descriptors and trust pins;
- running jobs and recent changes;
- object provenance and validation state;
- memory/history as navigable space.

The Cockpit is not just a dashboard. It is the operating surface for this digital space.

### Portal

A portal is an entry point from the Cockpit into a dimension or world.

A portal may be local, hosted, private, shared, or public. Entering a portal is not the same as trusting it. Trust is still expressed through signed descriptors, policies, object provenance, and local decisions.

### World

A world is a signed CAS graph interpreted by a frame.

It may contain:

- object type definitions;
- rules and behavior primitives;
- renderer, engine, and editor dependencies;
- world policy;
- frame policy;
- accepted object heads;
- scene/state objects;
- event history;
- renderer/dimension bindings;
- directive/tool inhabitants;
- provenance/remix lineage;
- hosted presence metadata;
- sync/admission records.

A world is not a server database. It can be hosted by a node, mirrored by peers, rendered by many dimensions, and verified locally.

### Dimension

A dimension is a renderer/runtime/state interpretation over signed world state.

Examples:

- Three.js spatial dimension;
- TUI operator dimension;
- knowledge graph dimension;
- timeline/history dimension;
- project/codebase dimension;
- simulation dimension;
- music/memory/garden dimensions.

The dimension is how the state is experienced. The signed CAS graph is what persists.

Dimensions should be user-buildable layers, not only first-party RyeOS surfaces. One creator may publish a symbolic card dimension; another may publish a voxel dimension; another may publish a Three.js dimension; another may publish a graph/editor dimension. Worlds can depend on these layers without making any one renderer the truth.

### Frame

A frame is the schema, validator, runtime, and policy contract for a world or dimension family.

It defines:

- object kinds;
- canonical representation;
- validators;
- renderer/runtime bindings;
- allowed behavior primitives;
- admission rules;
- trust requirements;
- time/replay model;
- migration/version compatibility.

Frames are how worlds avoid becoming arbitrary generated code blobs.

Frames are also how a community makes a reusable kind of world. A garden frame, dungeon frame, fiction-room frame, codebase frame, or alchemy-object frame should be something a person can author, publish, fork, and extend.

### Hosted node

A hosted node is an always-on RyeOS node.

It can keep a portal reachable, serve object closures, publish admissions, host subscriptions, and run admitted jobs. It is not a RyeOS identity authority. Runtime authority remains local to the target node: pinned descriptors, signed requests, node-local grants, and explicit policy.

Hosted nodes provide uptime, presence, and reachability. They do not own truth.

## Future spine

The future stack should develop in this order:

1. **Signed world/project objects** — non-executable CAS objects become first-class substrate.
2. **Policy objects** — project, world, frame, portal, and node policies are signed CAS objects.
3. **Node descriptors** — hosted nodes are discovered through signed/pinned descriptors, not central login.
4. **Creation ladder** — users can create objects, rules, renderers, frames, engines, portals, and templates as signed layers.
5. **Portal model** — Cockpit can open local or hosted portals into dimensions/worlds.
6. **Dimension runtime** — renderers and simulations interpret signed state through frames.
7. **Hosted presence** — hosted nodes keep spaces online and reachable without becoming authorities.
8. **Object graph sync** — peers pull closures, accepted heads, dependencies, and subscriptions by hash/ref.
9. **Remote execution** — durable jobs/results and signed requests run where the target node allows.
10. **Federation** — mirrors, node-to-node sync, cluster routing, and advanced attestations grow only when triggered.

## First experiences

The first experiences should be curated before they become open-ended. If the first prompt is “make anything,” the substrate will feel too abstract. Start from a specific vibe-coding seed, then use that seed to reveal the RyeOS stack.

The first curated seed should be small, emotionally legible, and layered enough to teach composition. For example:

```text
Memory Moth Room
  premise: visitors type one sentence; a glowing moth appears and remembers it
  object: memory_moth
  rule: visitor_message_creates_moth
  renderer: simple glow-room/moth interpretation
  frame: memory-room template
  portal: enter from Cockpit/Home
  remix: fork the renderer, rule, or frame into a new space
```

The user begins in Home/Cockpit as a vibe coder: “make a tiny room where visitors leave glowing moths that remember what they said.” RyeOS drafts the stack from that same surface, lets the user enter the room through a portal, then returns them to Home/Cockpit to reveal how the room is composed. The lesson is not delivered as vocabulary first. It is discovered through the thing the user just made.

A person should see how a curated space becomes a world by adding layers that other people can reuse. After that first guided example, the creation surface can open up.

Good first creation ladder:

```text
curated vibe prompt
  -> generated object
  -> generated rule or behavior
  -> generated renderer interpretation
  -> generated frame/world template
  -> enter through a portal
  -> inspect the stack from Cockpit/Home
  -> fork/remix one layer
```

The important demo is not that RyeOS shipped a beautiful world or asked the user to invent everything from scratch. The important demo is that a curated vibe-coded world can be opened, inspected, decomposed into layers, and remixed into a reusable kind of world.

Good seed portals:

- **Garden frame** — a reusable rule/renderer layer for tending persistent objects over time.
- **Collaborative fiction frame** — a text-first world family with canon, branches, and narrator policy.
- **Scene engine** — a renderer/runtime layer that maps signed scene objects into Three.js, voxels, cards, or another medium.
- **Music space frame** — composition rendered as architecture/light/path.
- **Memory space frame** — personal RyeOS history rendered as landscape.
- **Codebase world frame** — project structure, tools, directives, tests, graphs, and threads as navigable space.

Builders enter through the Cockpit. Non-builders enter through portals. Some users create objects; some create rules; some create engines; some create games; some only enter and play. All should feel that the space has gravity: things persist, provenance is visible, dependencies are inspectable, and history is load-bearing.

## Non-goals

This direction should not become:

- a global RyeOS account system;
- a central RyeOS Cloud authority;
- central-auth as RyeOS identity;
- hosted nodes owning world truth;
- one global metaverse;
- blockchain/consensus-first architecture;
- arbitrary generated code as the first runtime;
- shared-daemon multi-tenancy as the default assumption.

The right default is local-first, signed, portable, verifiable, hosted when useful, federated when needed.

## Guiding sentence

RyeOS should let people create, enter, host, inspect, remix, and share signed digital spaces without surrendering identity or truth to a central platform.
