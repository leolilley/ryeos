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

The Cockpit is the home dimension: the place where a RyeOS principal sees their projects, worlds, portals, hosted nodes, trust pins, running jobs, recent changes, and memory. From there, a user opens portals into dimensions and worlds.

A dimension is not the source of truth. It is a renderer, runtime, or state interpretation over signed CAS objects. The same world state can appear as a Three.js scene, TUI, graph, timeline, dashboard, editor, simulation, or future immersive view.

The future product loop is:

```text
open Cockpit
  -> describe a space, object, system, or world
  -> AI proposes signed state
  -> Rye validates and stores it as CAS
  -> enter through a portal
  -> inspect, remix, host, sync, or share it
```

This is not only a game platform or developer tool. It is a way to inhabit digital structures whose provenance, ownership, and history are visible.

## Baseline assumed

This future direction assumes RyeOS already has the local/hosted seams needed to continue:

- principal-aware hosted user-space exists;
- hosted-node exists outside `standard` as the bundle for always-on hosted RyeOS behavior;
- central-auth exists as app-local realm auth, not RyeOS identity;
- local Studio/Cockpit project registration and launch flows exist as current substrate.

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

### Hosted node

A hosted node is an always-on RyeOS node.

It can keep a portal reachable, serve object closures, publish admissions, host subscriptions, and run admitted jobs. It is not a RyeOS identity authority. Runtime authority remains local to the target node: pinned descriptors, signed requests, node-local grants, and explicit policy.

Hosted nodes provide uptime, presence, and reachability. They do not own truth.

## Future spine

The future stack should develop in this order:

1. **Signed world/project objects** — non-executable CAS objects become first-class substrate.
2. **Policy objects** — project, world, frame, portal, and node policies are signed CAS objects.
3. **Node descriptors** — hosted nodes are discovered through signed/pinned descriptors, not central login.
4. **Portal model** — Cockpit can open local or hosted portals into dimensions/worlds.
5. **Dimension runtime** — renderers and simulations interpret signed state through frames.
6. **Hosted presence** — hosted nodes keep spaces online and reachable without becoming authorities.
7. **Object graph sync** — peers pull closures, accepted heads, and subscriptions by hash/ref.
8. **Remote execution** — durable jobs/results and signed requests run where the target node allows.
9. **Federation** — mirrors, node-to-node sync, cluster routing, and advanced attestations grow only when triggered.

## First experiences

The first spaces should make the new ontology felt before explaining it.

Good seed portals:

- **Garden** — a persistent private or shared space a person tends over time.
- **Collaborative fiction room** — people prompt a world into existence and its history persists.
- **Shared Three.js scene** — immediate visible loop: prompt object, see it, sign it, share it.
- **Music space** — a piece of music rendered as architecture/light/path.
- **Memory space** — personal RyeOS history rendered as landscape.
- **Codebase world** — project structure, tools, directives, tests, graphs, and threads as navigable space.

Builders enter through the Cockpit. Non-builders enter through portals. Both should feel that the space has gravity: things persist, provenance is visible, and history is load-bearing.

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
