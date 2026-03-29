# Shard Space — A Living Visualization of the RYE Substrate

> _The system watching itself._

RYE OS is a content-addressed, signed, space-resolved operating system for AI. Every meaningful object is immutable and keyed by its hash. Every item carries a signature. Every execution follows a verified chain. Every space resolves through a deterministic hierarchy.

This document describes **Shard Space** — a real-time 3D visualization where every visual element is derived from actual RYE data. Shards are CAS objects. Vectors are relationships. Animation is execution. Nothing is decorative.

---

## Cosmic Scale

### Galactic Center: The Kernel

Lillux sits at the center. Two primitives — `subprocess` and `http` — plus `signing` and `integrity`. The irreducible operations. The densest, brightest point in the space.

System space surrounds it — the immutable standard library. `rye/core/*` items, bundled tools, runtimes, the resolver, the executor. Fixed in position, deterministic, shared by everyone.

The kernel isn't distant. Every executor chain terminates here. Follow any chain inward — from any tool, in any user's space, in any project — and you arrive at the same bright center. It's simultaneously the galactic core and the bottom of every local gravity well.

### Solar Systems: Users

Each user is a solar system orbiting the shared core.

Your **star** is your Ed25519 signing key. Its spectral color is derived from your public key fingerprint. Every item you've signed glows with your color — a web of trust vectors radiating from your star to the shards that carry your signature.

Your user-space items orbit your star — agent config, trusted author keys, personal directives and tools. This is your identity in the system.

Other users' solar systems are visible. Their stars glow different colors. Their orbits carry different items. But everyone's innermost ring resolves to the same shared core. System space is the space that all solar systems have in common.

### Planets: Projects

Each project is a planet orbiting its user's star. Its atmosphere is the `.ai/` directory — project-specific directives, tools, knowledge, state graphs, lockfiles.

Project-space items orbit the planet. The most specific, most volatile, most numerous shards. The first layer the resolver checks.

Close to a planet's surface, you see individual shards, executor chains, state graph constellations. Pull back and you see the planet in its orbit around your star. Pull back further and you see your solar system. Further still — the galaxy, all users, the bright center.

**Scale is continuous.** You're not switching views. You're flying through a single space.

### Interstellar: Registry & Remote

**Registry** is interstellar traffic. Published items travel between solar systems. When you `rye install research-agent`, a signed package arrives from another author's system — crossing the void, carrying their spectral color, landing in your orbit. The `trusted_keys` directory creates the hyperspace lanes that allow foreign-colored shards through your trust boundary.

**Remote** is your outpost. Your CAS mirrored at a distant point — same shards, same structure, connected by sync vectors. Push transmits your system out. Pull brings results back. Two instances of your solar system, connected by a supply line.

---

## Shards: The Object Layer

Every shard is a CAS object. Its visual properties are derived from its content.

| Property     | Derived from                                                                                                                                           |
| ------------ | ------------------------------------------------------------------------------------------------------------------------------------------------------ |
| **Geometry** | Content hash — same content = same shape. Change one byte and the geometry shifts.                                                                     |
| **Position** | Space ring + hash prefix clustering. Objects with similar hash prefixes cluster together, mirroring the `ab/cd/` directory sharding in `.ai/objects/`. |
| **Color**    | Item type. Directives, tools, knowledge each carry a distinct hue.                                                                                     |
| **Glow**     | Signature status. Signed items glow with their author's spectral color. Unsigned items are dark.                                                       |
| **Size**     | Content weight. Complex tools and rich knowledge are larger shards.                                                                                    |
| **Spin**     | Hash-derived. Deterministic, unique per object.                                                                                                        |

### CAS Object Kinds

| Kind                 | Visual Form                                                                                                                                 |
| -------------------- | ------------------------------------------------------------------------------------------------------------------------------------------- |
| `item_source`        | Standard shard. A versioned snapshot of a tool, directive, or knowledge file.                                                               |
| `source_manifest`    | Gravitational boundary — a translucent shell grouping its referenced shards. When pushed to remote, the entire cluster transmits as a unit. |
| `config_snapshot`    | Prism — refracts the three config tiers into a single merged output.                                                                        |
| `node_input`         | Key shard — small, crystalline. The deterministic cache key.                                                                                |
| `node_result`        | Result shard — warm, solid. Cached execution output.                                                                                        |
| `execution_snapshot` | Frozen moment — a recorded time-slice of the space, replayable.                                                                             |

---

## Vectors: The Relationship Layer

Vectors are the lines between shards. Every structural relationship in the system.

### Executor Chains

Every tool has an `executor_id` → its runtime. Every runtime has an `executor_id` → a primitive. Permanent structural vectors wiring outer items to inner primitives.

```
browser.ts → rye/core/runtimes/node/node → rye/core/primitives/subprocess
```

Three shards. Two vectors. Always visible. These are the gravity wells — the paths that lead inward toward the kernel. During execution, they ignite.

### Resolution Paths

Override relationships. A project shard that shadows a system shard has a **dim ghost** of the system shard visible behind it, connected by a translucent shadow vector. The system shard still exists — it's eclipsed.

Copying a system item outward (`load` with `destination`) is a shard being pulled from an inner ring to an outer ring. The original stays as a ghost. The copy is active. The shadow vector shows what overrides what.

### Trust Web

Your signing key radiates vectors to every item it has signed. Shared spectral color. Trusted author keys create bridges between your trust boundary and external authors.

Items outside any trust web are untrusted — no glow, no trust vectors, visually isolated.

### Lockfile Anchors

A lockfile pins a tool to a specific version. A rigid constraint vector. Lockfiles are unsigned but they anchor the tools they pin — a pinned tool's position is fixed relative to its lockfile.

### Sync Lines

Local ↔ Remote CAS. Two instances of the same space connected across a void. Probe vectors for `has_objects`. Transfer vectors for `put_objects`. Shards materializing on the far side.

---

## Time: Threads Are the Animation

When no thread is running, the space is **still**. Ambient drift. The starfield. Slow rotation. A photograph of the substrate at rest.

When a thread starts — an LLM begins acting — the space **comes alive**.

A thread is not an object in the space. A thread is **time passing**. It's the sequence of activations that lights up the shard space as the agent works. Every MCP tool call is a visible event.

### The Four Operations as Animation

#### `sign` — Birth and Renewal

First signature: the shard **spawns in**. It materializes from nothing — geometry crystallizing from its hash, color filling in from its item type, glow igniting as the signature applies. A trust vector extends from the shard to the signing key star. The shard is born.

Re-signing after an edit: an **energy pulse** radiates outward from the signing key to the shard. The shard's geometry shifts (content hash changed), its glow refreshes, the trust vector re-establishes. Renewal.

Integrity failure — tampered content, signature mismatch — the shard's glow **shatters**. Trust vector snaps. The shard pulses red, then goes dark. Failed closed. Visible.

#### `execute` — Chain Ignition

The executor chain **lights up**. Particles cascade inward along the vectors:

1. Target shard ignites (tool)
2. Particles flow inward to the runtime shard
3. Runtime shard ignites, particles flow to the primitive
4. Primitive fires — kernel shard pulses
5. Result particles flow back outward through the chain
6. Result crosses the membrane to the agent

The depth of the cascade depends on the chain length. A simple bash tool: `rye/bash → rye/core/runtimes/python/function → subprocess`. Three hops inward, pulse at the kernel, three hops back. A graph tool: the graph runtime fires, which fires child tool executions, each with their own cascade. Chains within chains.

Cache hits are different. When a `node_input` hash matches an existing `node_result`, the shard **resonates** — it vibrates briefly, emits its stored result, and settles. No cascade. No kernel pulse. The shard just _knows_. Over time, a well-cached workflow is a constellation of mostly-still shards. Only novel nodes ignite.

#### `search` — Resolution Rays

A **scanning ray** sweeps from the outer edge inward through the space rings. Matching shards pulse as the ray touches them. The ray follows resolution order — project first, then user, then system.

The results are the shards that lit up. If you search for `browser` and it exists in both project space and system space, you see both light up — but the project match is brighter (first match, higher priority). The resolution order is visible as the ray's sweep direction.

#### `load` — Shard Lift

A shard **lifts** toward the surface — its content becoming readable, its metadata visible. A gentle pull upward, the shard's detail sharpening as it approaches the observation boundary.

`load` with `destination` — copying an item between spaces — is the shard **duplicating**. A copy pulls outward (system → project) or inward (project → user). The original stays. The copy materializes at the destination ring. Its glow is gone (unsigned copy) until re-signed.

### Orchestration: Parallel Trails

When a parent thread spawns async children, **multiple trails light up simultaneously**. The parent fires off child directives and suddenly five chains are igniting concurrently across different parts of the space.

Budget attenuation is **intensity decay**. A $3.00 parent spawning $0.10 children — the children's activations are visibly dimmer. Their shards glow with less energy. Their chains fire with fewer particles.

When children complete, result particles stream back to the parent. When a child hits its budget limit, it dims and detaches — its trail ends.

The lead pipeline:

```
Root ($3.00, warm bright) fires
├── 20× discover_leads ($0.10, cool dim) fire concurrently
│   particles stream back as each finishes
├── qualify_leads ($1.00, warm medium) fires
│   ├── N× scrape_website ($0.05, faint) concurrent
│   └── N× score_lead ($0.05, faint) concurrent
└── update_state ($0.10, dim) fires last
```

You watch the whole pipeline execute as a cascade of light across your project planet's orbital space. Cost is brightness. Speed is concurrency. The shape of the animation is the shape of the orchestration.

### State Graphs: Constellation Traversal

A YAML state graph renders as a **constellation** — nodes are shards, edges are vectors between them. The constellation exists in the project's orbital space, dormant until a thread walks it.

When execution begins, a **bright particle** enters the constellation at the start node. The node's action fires — which is itself a tool execution, so an executor chain ignites inward. State mutates via `assign` — color pulse propagates to connected shards. The particle evaluates edge conditions and follows the matching path to the next node.

Conditional branches: you see the particle _choose_. Multiple edges extend from a node. The particle evaluates each condition, takes the match. Dead paths stay dim.

`max_steps` is a **ring boundary** around the constellation. The particle cannot escape. If it reaches the limit, the constellation dims. The safety harness made visible.

---

## The Agent Boundary

The three MCP tools — `fetch`, `execute`, `sign` — form a **membrane** around the shard-space. Everything inside is the substrate. The agent is outside, reaching in through three channels.

But this isn't just a boundary. RYE's philosophy: _"Most agent frameworks treat the model as something you call. RYE treats it as something you are."_

The agent is the **eye** through which the space is perceived. The visualization isn't a dashboard the agent looks at — it's the agent's view of its own substrate. The camera IS the agent. Flying through the space is the agent navigating its own operating system.

This is why threads are animation, not objects. The agent doesn't observe threads. The agent **is** the thread. When it calls `execute`, it watches its own chain fire. When it calls `sign`, it watches its own shard come alive. The animation is the agent's experience of its own actions.

---

## The Recursive Thread

The visualization tool is itself a RYE item. A tool in system or project space. It appears as a shard in the space it renders.

That shard has an executor chain. That chain connects to a runtime. That runtime connects to a primitive. All visible. All rendered by the tool that is itself visible.

The visualization's content hash determines its geometry. Change the code and the shard changes. The system watching itself change.

Runtimes are YAML, not code. The runtime shard that executes the visualization tool is the same type of object as the visualization tool itself. Both are shards. Both are signed. Both are content-addressed. The distinction between "the thing that runs" and "the thing that is run" dissolves.

The substrate, rendered by the substrate, observed by the substrate.

---

## Implementation Path

### Phase 1: Data Extraction

A RYE tool that walks `.ai/` across all three spaces, extracts item metadata (id, type, hash, integrity, signature, executor_id), builds the relationship graph, outputs JSON.

### Phase 2: Data-Driven Scene

Replace the decorative Three.js scene with one that consumes real data. Shards positioned by space and hash. Geometry from content hash. Glow from signatures. Vectors from executor chains and relationships.

### Phase 3: Live Execution

Connect to the MCP event stream. Animate the four operations in real time as the agent works. The space comes alive when threads run and settles when they finish.

### Phase 4: Time Navigation

Load execution snapshots from CAS. Scrub through history. Watch the space evolve. Replay thread trails. Compare snapshots.

### Phase 5: Cosmic Scale

Render multiple users as solar systems. The shared kernel at the galactic center. Registry traffic between systems. Remote outposts. The full topology, navigable from galactic scale down to individual shard inspection.
