<!-- ryeos:signed:2026-07-22T04:32:50Z:181a4fb57997888aa624c0644a0a9dd5e2714f01a6bcd0f007f5bf6e2f957f43:2/wvaXu1X9HtUyuxPbggNTff+do8pAc9qgKNv/PGwc3B8/fd8SeGzz67iGyRMj0DNROdojCKZJn27DzwZwfnBg==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
```yaml
category: ryeos/future
name: public-first-contact-live-node
title: Public First Contact and Live-Node Web Experience
description: Future direction for a cinematic public RyeOS arrival surface that resolves into a real capability-limited hosted-node UI rather than a detached marketing site
entry_type: design
version: "0.1.0"
created_at: 2026-07-22T00:00:00Z
updated_at: 2026-07-22T00:00:00Z
tags:
  - ryeos-ui
  - hosted-node
  - first-contact
  - onboarding
  - public-surface
  - scroll-video
  - cinematic-interface
  - identity
  - provenance
  - future-work
```

# Public First Contact and Live-Node Web Experience

## Status

Future product and implementation direction. This is not part of the current
public HTTP or RyeOS UI contract.

This note describes the intended relationship between:

- a short cinematic public arrival experience;
- the existing `ryeos init` first-contact ceremony;
- the RyeOS browser renderer and signed surface model;
- a real hosted RyeOS node;
- public, realm, and RyeOS principal authority; and
- the asset, session, and read-model work needed to connect them safely.

It does not authorize public arbitrary execution, shared hostile tenancy, or a
second frontend application model.

## Thesis

The future RyeOS website should not be a detached marketing site that happens
to link to RyeOS.

It should be a public first-contact surface served by an actual RyeOS
deployment. A short scroll-driven film introduces the system's physical and
conceptual language, then resolves into a capability-limited live RyeOS
surface. The visitor discovers that the film's final image is not another
rendered claim: it is the running node.

```text
cinematic first contact
  -> identity and trust become legible
  -> the hosted node reveals verifiable public facts
  -> the film aligns with and yields to the live scene
  -> the visitor enters a real RyeOS surface
```

The experience begins as cinema and ends as verification.

The primary action is therefore:

```text
Enter RyeOS
```

It is not `Get started`, `Book a demo`, or an artificial lead-generation
conversion. The conversion is entry into the product itself.

## Product truth the experience must express

RyeOS is portable verified execution.

It represents executable capability, authority, runtime state, and execution
history as signed, content-addressed data. That gives work an identity that can
survive the process, session, machine, and vendor runtime currently projecting
it.

The public experience should build from the actual RyeOS primitives:

- **items** are typed units of behavior or context;
- **bundles** distribute signed collections of items and their executable
  dependencies;
- **keys** are the actors and authority holders;
- **threads** are durable execution objects rather than disposable client
  sessions;
- **events** form the execution record;
- **CAS objects and signed refs** make state verifiable, movable, and
  reconstructable;
- **remotes** let work move with its trust instead of borrowing the ambient
  authority of another machine; and
- **surfaces and views** are interpretations of semantic RyeOS state, not the
  state itself.

The page must not reduce this to generic AI-agent positioning. Agents are an
important RyeOS application, but portable verified execution is the product
thesis.

## Why the `ryeos init` ceremony is the narrative source

The interactive `ryeos init` flow already contains the strongest RyeOS brand
and product sequence. It is not decorative onboarding. It performs real work
at explicit, recoverable boundaries:

```text
Welcome to RyeOS
  -> explain identities and trust
  -> collect optional operator semantics and entropy contribution
  -> create operator and node identities
  -> pin publisher trust
  -> discover, verify, and install bundles
  -> initialize the vault identity
  -> verify initialized state
  -> reveal the distinct fingerprints
  -> optionally configure a verified provider and model
  -> enter the RyeOS workspace
```

The ceremony's opening language is the right first-contact statement:

> RyeOS is one operating intelligence expressed through many concurrent
> threads. It carries verified work between machines without surrendering the
> operator's authority.

Its identity explainer names four separate responsibilities:

```text
operator identity   signs what you author and request
node identity       identifies this RyeOS node
vault identity      encrypts secrets held by this node
publisher trust     decides which installed bundles may execute
```

Its closing boundary is equally important:

```text
t open RyeOS TUI  ·  enter return to shell
```

Initialization finishes before the workspace begins. The future public web
experience should preserve this rhythm: a bounded first-contact ceremony
finishes, identity is revealed, and the visitor chooses to enter the operating
surface.

### Ceremony properties to preserve

- Sparse, operational language rather than marketing superlatives.
- The wide prism at welcome and compact prism at identity reveal.
- `RYEOS  <phase>` as a persistent orientation marker.
- The `◆` progress glyph.
- Distinct identity rows with full fingerprints when appropriate.
- Explicit optionality around model-provider configuration.
- Safe cancellation and interruption language.
- No claim that a provider or model is the source of RyeOS identity.
- A clear boundary between initialization and operating the system.

### Ceremony properties that must not be faked publicly

A public visitor is not the operator who initialized the hosted node. The
arrival surface must not pretend to create their operator, node, or vault keys.
It must not animate false initialization progress or reveal private node state.

The public version is a first-contact ceremony for an already initialized
node. Its job is to explain and then verify, not to imply ownership.

Only public facts should appear in the public identity reveal, for example:

- node fingerprint;
- node version;
- health state;
- public surface ref;
- official publisher fingerprint or trust statement;
- count of bundles intentionally exposed by the public read model; and
- current public demonstration thread state.

The vault fingerprint and private operator context should not be public
telemetry merely because the local initialization ceremony displays them to
the owner.

## Relationship to the earlier RyeOS web home

The earlier Studio/RyeOS browser home established a useful visual language:

- a fractured orange crystal or prism;
- charcoal, ember, ochre, muted aqua, and warm neutral tones;
- optical corner registration marks;
- concise statements about hashes, signatures, authority, and portability;
- an install action; and
- an animated field behind the workspace.

That work was visually distinctive, but its marketing composition lived inside
the operator UI. The later UI architecture correctly removed the special
`home` mode: an empty center is derived from the tile list, a backdrop is
ordinary signed scene content, and browser and terminal render the same shared
semantic model.

The future direction should recover the earlier visual identity without
reintroducing a hard-coded marketing state into the operator renderer.

The public arrival is its own bounded surface. The operator UI remains the
operator UI.

## Experience architecture

The preferred public shape is same-origin and node-backed:

```text
GET /
  public cinematic arrival assets
  native scroll-scrubbed film
  sanitized live-node annotations
             |
             v
  Enter RyeOS
  mint or redeem a capability-limited public browser session
             |
             v
GET /ui
  surface:ryeos/ui/public
  real RyeOS browser renderer
  real shared Rust UI core
  live public Atlas, identity, thread, and provenance views
```

The public page may be visually rich, but it is not allowed to become a second
semantic RyeOS client. State transitions, view meaning, execution semantics,
and operator behavior continue to belong to the shared Rust UI core and signed
surface/view content.

### The seam

The last part of the film should be authored to match the initial live scene:

1. Load the final live public scene beneath the fixed video.
2. Arrange the film's final frame to match that scene's major geometry.
3. During the last 10-15 percent of progress, reduce film opacity while the
   live scene gains contrast and interactivity.
4. Replace illustrative annotations with live node facts.
5. Resolve UI chrome from the edges only after the scene is recognizably live.
6. Let `Enter RyeOS` establish the public session and navigate into `/ui`.

The final-frame reference should be designed before generating the film. Where
the video model permits first/last-frame conditioning, use a still derived from
the real public Atlas composition as the last-frame reference. Text-only
generation is unlikely to create a sufficiently precise handoff by accident.

## Commercial purpose and conversion

The offering is RyeOS itself: a portable verified execution substrate and a
live node that demonstrates its own claims.

The primary conversion goal is:

```text
Enter and inspect a real running RyeOS node.
```

Useful secondary actions are:

- inspect the node identity;
- inspect the signed public surface definition;
- inspect the art-direction/reconstruction knowledge item;
- watch a safe scheduled execution become a durable thread;
- read the source;
- install RyeOS locally; and
- return to the beginning.

The page should not show fabricated performance telemetry, invented adoption
numbers, synthetic thread counts, or fake terminal output. If a fact looks
live, it must come from the public node read model.

## Narrative chapters

### Chapter 1: Welcome

Approximate progress: `0-18%`.

```text
Welcome to RyeOS

One operating intelligence.
Many concurrent threads.
```

The intact prism is dormant in darkness. Scroll begins the camera's approach.
The layout is sparse and uses the ceremony's wide-prism register rather than a
conventional centered SaaS hero.

### Chapter 2: Identity

Approximate progress: `18-38%`.

```text
Give work an identity.

Hashes identify the exact object.
Signatures identify who stands behind it.
```

The camera enters the prism and finds distinct operator, node, and vault
structures. They are visibly connected but never merge. Small measurement
marks may label their responsibilities, but the generated film itself should
contain no model-generated readable text.

### Chapter 3: Verification and durability

Approximate progress: `38-60%`.

```text
The record is the run.

A process can die. The execution object remains.
```

Publisher signatures pin verified bundle fragments into the internal lattice.
Invalid fragments fail to attach. A dual event braid begins to carry the
execution forward.

The chapter can echo real initialization phases without pretending the public
visitor is mutating the node:

- publisher trust pinned;
- bundle signatures verified;
- vault identity present; and
- initialized state verified.

### Chapter 4: Portability

Approximate progress: `60-82%`.

```text
Move the work. Keep the trust.

The machine changes. The identity holds.
```

The event braid crosses a dark boundary to another node. Portable verified
objects reassemble without borrowing ambient authority from the destination.

### Chapter 5: Identity reveal and entry

Approximate progress: `82-100%`.

```text
You are looking at a running RyeOS node.
```

The film emerges into the namespace Atlas. Illustrative labels yield to actual
public node values. The final register resembles the local completion screen:

```text
RYEOS  NODE READY

node       fp:...
bundles    <public count> verified
surface    surface:ryeos/ui/public
status     healthy

enter open live node  ·  inspect identity
```

## Ten candidate macro journeys

The film should be one continuous extreme-macro camera movement, not a montage.
Each candidate below expresses a different RyeOS property and lands naturally
on a real product surface.

### 1. First Contact / Node Genesis

Enter a dormant obsidian prism through an ember fracture, pass the distinct
identity structures, watch verified bundles form the node, and emerge over the
live namespace Atlas.

Natural landing: public Atlas and node identity.

### 2. The Verified Crystal

Move through content-addressed glass cells, signature filaments, and a braided
execution history before crossing into a second matching crystal node.

Natural landing: item Atlas or remote topology.

### 3. The Event Braid

Follow two luminous fibres weaving global-chain and per-thread event links. A
surrounding process strand snaps while the intact recorded braid continues.

Natural landing: live thread transcript and replay.

### 4. The Black Box

Travel through running machinery into its flight recorder. The mechanism dies,
the record survives, and a new mechanism reconstructs around it.

Natural landing: recovery, continuation, and thread history.

### 5. The CAS Foundry

Molten information cools into uniquely stamped objects, which assemble into a
branching snapshot DAG and then a living node.

Natural landing: object and provenance inspector.

### 6. The Travelling Flame

A computation flame is sealed inside a transparent signed vessel, crosses a
machine boundary, and reignites without surrendering its provenance.

Natural landing: remote-node topology.

### 7. The Key Terrain

Skim the microscopic ridges of a cryptographic key, follow its imprint into a
signature, and watch invalid material disintegrate at verification.

Natural landing: node identity and verification view.

### 8. The Folding Atlas

Paper fibres become a topographic map of project and system spaces, then fold
into portals connecting independent nodes.

Natural landing: namespace Atlas and future portal model.

### 9. The Rye Kernel

Enter a grain whose internal cells branch into a durable execution graph. A
seed moves to new soil and continues growing without losing lineage.

Natural landing: graph/workflow view.

### 10. The Optical Loom

Tools, directives, runtimes, inputs, and authority arrive as distinct fibres
and are woven into one traceable executable fabric.

Natural landing: item graph or execution plan view.

`First Contact / Node Genesis` is the preferred initial direction because it
joins the existing prism identity, the real initialization ceremony, and the
live Atlas handoff in one spatial journey.

## Preferred video-generation prompt

```text
Single continuous 8-second extreme-macro cinematic journey beginning directly
above a dormant charcoal-black crystalline prism suspended in a quiet dark
field, a single restrained ember-orange point glowing at its center, camera
moving steadily forward toward a narrow fracture as the prism awakens, passing
through the mineral surface into three distinct but connected internal
crystalline structures representing operator authorship, node identity and
sealed vault custody, each structure physically different and never merging,
continuing through fine amber signature filaments that pin incoming translucent
bundle fragments into a precise content-addressed lattice while dull unverified
fragments fail to connect and fall into darkness, moving deeper as the verified
lattice closes around a protected central core and twin event strands begin
carrying warm pulses outward, the completed node illuminating from within,
camera following those durable thread strands toward the far surface and
emerging smoothly into a clean top-down namespace atlas of orange, ochre,
muted-aqua and warm-white objects arranged on a deep charcoal plane, final frame
mostly orthographic and compositionally aligned with the RyeOS live Atlas
interface, photorealistic macro optical cinematography, physically accurate
mineral, glass and transmitted light, strong forward parallax, restrained
Gruvbox-derived palette, seamless one-take movement, no cuts, no readable
generated text, no logos, no circuit-board imagery, no cyberpunk neon.
```

## Visual system

### Palette

The existing RyeOS palette is the starting point:

```text
background       #1d2021
panel            #282828
deep structure   #17191a
foreground       #ebdbb2
soft foreground  #d5c4a1
muted            #a89984
ember accent     #d65d0e
bright ember     #fe8019
warning ochre    #fabd2f
verified aqua    #8ec07c
danger           #fb4934
```

The film may use physically plausible variations of these colors, but should
not drift into blue-purple cyberpunk lighting or generic luxury gradients.

### Typography

- Monospaced type remains the operational voice.
- Display scale may be large, but letterforms should remain plain and exact.
- Identity values, refs, hashes, phases, and commands are treated as data, not
  decorative code texture.
- No tiny pseudo-technical labels.
- No model-generated text should be embedded in the video.

### Concept-specific instruments

- fingerprint registration ticks;
- content-hash brackets;
- signature and trust-path leader lines;
- dual event-chain tracks;
- bundle verification counts;
- surface and canonical-ref labels;
- node health and identity readouts; and
- the ceremony's prism and `◆` phase marker.

## Motion language

Motion should be continuous, physically motivated, and subordinate to the
film's forward movement.

Signature behaviors include:

1. **Prism aperture** — the initial fracture opens only as the camera reaches
   it.
2. **Identity separation** — operator, node, and vault structures drift into
   distinct registered positions without becoming disconnected.
3. **Trust pinning** — verified fragments settle sharply into the lattice;
   invalid fragments fail to bind.
4. **Event braiding** — two related tracks weave without collapsing into one
   line.
5. **Process loss without history loss** — surrounding machinery may disappear
   while the event structure continues.
6. **Remote parallax** — a second node becomes spatially legible across a dark
   boundary.
7. **Film-to-scene dissolve** — the final frame yields to real rendered state,
   not to a blank page or unrelated dashboard.
8. **Chrome resolution** — live UI edges appear only after the underlying scene
   has become interactive.

Decorative objects should not float independently of these mechanisms.

## Scroll-film behavior

The reference cinematic-site pattern contains several useful engineering
requirements, but they should be adapted to RyeOS rather than copied as a
framework prescription.

### Keep

- A native fixed full-viewport `<video>`.
- Duration read from loaded metadata.
- Document progress mapped to a target media time.
- One `requestAnimationFrame` playhead with frame-rate-independent damping.
- A bounded newest-target seek policy rather than an accumulating seek queue.
- Decoder-aware use of `seeking`, `seeked`, and
  `requestVideoFrameCallback` where supported.
- A restrained loading state, buffered indication, media-error state, and
  reduced-motion fallback.
- Center-safe film composition for mobile crops.
- Keyboard-accessible `Begin first contact`, pause, resume, skip, restart, and
  `Enter RyeOS` actions.
- Manual wheel, touch, pointer drag, or scrolling keys pausing any automatic
  tour.

### Do not inherit blindly

- React, Vite, Tailwind, BrowserRouter, and a second application model are not
  RyeOS UI requirements.
- A fixed `500vh` document and fixed 20-second tour are creative defaults, not
  product laws. Approximately `350-420vh` and a 12-16 second tour may better fit
  an eight-second film.
- Lenis should not be added only because a reference prompt names it. Native
  browser scrolling plus a damped media playhead is simpler and preserves the
  browser's accessibility behavior.
- GSAP is optional. If used, it must be locally packaged and should coordinate
  presentation only, not own RyeOS product state.
- Sixty-frame-per-second interpolation is not the primary cure for scrub
  quality. Codec choice, keyframe cadence, GOP size, byte-range delivery,
  decoder-safe seeking, media dimensions, and damping are at least as
  important.
- A public `/prompt/` portfolio route is not necessary. The reconstruction
  brief should be a signed RyeOS knowledge item that the product can inspect.

### Reduced motion

Under `prefers-reduced-motion`, show a strong poster frame, the concise node
identity statement, live public facts, and the entry action. Do not require the
scroll ceremony to access the node.

## Public live surface

The handoff target should be a purpose-built surface such as:

```text
surface:ryeos/ui/public
```

It must not be the full operator Atlas with a cosmetic read-only flag. Its
views and services should be public-safe by construction.

Candidate content:

- public node identity and health;
- public bundle and item Atlas projections;
- an intentionally published demonstration project;
- a scheduled safe example thread;
- redacted thread lifecycle and provenance events;
- signed surface and view definitions;
- public source and installation instructions; and
- a path to authenticated or local ownership where one later exists.

The most compelling live demonstration is a safe scheduled execution that
continuously produces a real durable thread. Visitors can watch execution
events, process interruption/recovery, and completion without being granted
arbitrary compute authority.

## Authority and security boundaries

### Public visitor

A public visitor is an anonymous or realm-authenticated browser principal with
only the capabilities required by the public surface.

They are not automatically a RyeOS principal and do not receive protocol
execution authority merely by entering the page.

### Realm principal

If central-auth participates, it answers which browser session may enter a
particular web realm. The consuming backend continues to own cookies, CSRF,
CORS, and route middleware.

Realm authentication must not be treated as RyeOS protocol authority.

### RyeOS principal

A RyeOS principal remains a signing identity. Signed requests, explicit grants,
and node-local policy authorize RyeOS execution.

### Hosted-node boundary

The initial public experience should run on an isolated demonstration node or
container with no private operator projects, credentials, or unrelated
workloads.

The current hosted-node architecture does not claim hostile shared-daemon
tenancy. Public arbitrary directives, tools, uploads, project mutation, remote
admission, and unrestricted thread input remain out of scope until the
principal-specific storage, quota, audit, and outer worker boundaries exist.

### Session minting

The current browser launch path requires a verified signed caller and grants a
read-only UI capability. The public path should be a separate, narrower
contract rather than a weakening of operator launch minting.

Requirements include:

- only the public surface may be selected;
- the project root is fixed to an intentionally published demo project or
  absent;
- capabilities are an explicit public allowlist narrower than generic
  `ui.read` where necessary;
- sessions are short-lived and revocable;
- public session creation is rate-limited;
- cookies are secure under public HTTPS;
- public services redact paths, secrets, private thread content, launch
  metadata, and unapproved item bodies; and
- no entry action can mint execution authority.

## Static assets and media delivery

The current web asset provider embeds a fixed list of browser UI assets into
the daemon. The current generic static response supports full-body assets,
ETags, cache control, and security headers, but not production byte-range video
delivery.

The arrival experience needs:

1. A signed asset pack or a generic bundle-backed web asset provider.
2. Explicit media content types, including `video/mp4` where used.
3. HTTP `Range`, `Accept-Ranges`, `Content-Range`, conditional requests, and
   correct `206 Partial Content` behavior.
4. Long-lived immutable caching for content-addressed film and poster assets.
5. A no-cache shell that refers to immutable asset names or digests.
6. A locally served script/style dependency policy.
7. A CSP that explicitly covers the required media, worker, WASM, and
   connection sources without broadening unrelated routes.
8. A poster and non-video fallback.
9. Mobile-safe encodes and an explicit preload budget.
10. Asset provenance connected to a signed bundle manifest or signed knowledge
    record.

### Near-term packaging option

For the official RyeOS public site, the first implementation may add the
arrival assets to the RyeOS UI composition and add official bundle-owned
routes. This is practical but keeps the asset list compiled into a first-party
provider.

### Long-term packaging option

The more RyeOS-native endpoint is a generic signed web-surface asset kind or
provider. A bundle or deployable project surface would declare:

- entry document;
- immutable assets and media digests;
- route intent;
- CSP requirements;
- public/auth policy;
- linked RyeOS surface ref; and
- reconciliation into node-owned runtime routes.

That work should align with the deferred project deployable-surface registry.
Project `.ai/node/routes` must remain node-owned and must not become broadly
syncable as an accidental way to deploy websites.

## Reconstruction and provenance

The commercial microsite pattern often adds a `/prompt/` page containing the
generation prompt. RyeOS can express the underlying idea more honestly.

The reconstruction record should be a signed knowledge item containing:

- brand and art-direction brief;
- selected video concept;
- original and final media prompt;
- media filename, digest, duration, dimensions, codec, and path;
- final-frame reference;
- chapter narrative and copy;
- palette and typography;
- motion and responsive behavior;
- public surface ref;
- asset and route architecture;
- scroll-video constants;
- reduced-motion behavior; and
- accessibility requirements.

The public surface can expose an `Inspect this experience` action that opens
that knowledge item through RyeOS itself. This document is the initial design
record and can later be refined or split into the exact reconstruction item
when media and implementation choices are fixed.

## Implementation sequence

### Phase 0: Visual proof

- Capture a representative Atlas final-frame composition.
- Produce a desktop-safe and mobile-safe storyboard.
- Generate several eight-second film candidates from the preferred prompt.
- Select based on continuity, final-frame match, material plausibility, and
  absence of generated text/artifacts.
- Produce seek-friendly delivery encodes without altering the narrative.

This phase may use a temporary standalone viewer for evaluation, but that
viewer is not the product architecture.

### Phase 1: Public read model

- Define the exact public node facts.
- Build public-safe services or projections.
- Define `surface:ryeos/ui/public` and its views.
- Run it against an isolated demo node and demo project.
- Add redaction and authority tests before visual integration.

### Phase 2: Media and asset substrate

- Add the signed asset packaging decision.
- Add MP4 content type and byte-range support.
- Add immutable caching, poster delivery, CSP coverage, and media tests.
- Keep all third-party runtime dependencies local or remove them.

### Phase 3: Arrival surface

- Implement native scroll progress and the bounded damped video playhead.
- Implement the five chapters and init-derived visual register.
- Add reduced-motion, skip, tour, pause, and restart behavior.
- Bind annotations to sanitized live node data.
- Preload the final live scene without blocking first paint.

### Phase 4: Guest handoff

- Add a public-only session mint/redeem contract.
- Restrict it to the public surface and demo context.
- Make cookies and redirects production-HTTPS aware.
- Crossfade the final film frame into the real scene.
- Navigate into `/ui` with no semantic duplication in the arrival code.

### Phase 5: Verification and release

- Test current Safari/iOS media seeking behavior.
- Test keyboard-only and reduced-motion entry.
- Test refresh, back, direct `/ui`, expired session, and interrupted tour
  behavior.
- Test public read-model redaction and capability denial.
- Test HTTP range and caching behavior under the production proxy.
- Test load and rate limits on anonymous session creation.
- Sign the final knowledge, surface, view, route, and asset declarations.

## Acceptance criteria

The direction is successful when all of the following are true:

- The public endpoint is served as part of a real RyeOS deployment.
- The page communicates portable verified execution without depending on
  generic AI marketing language.
- Its narrative visibly inherits the `ryeos init` first-contact ceremony.
- The film remains a single continuous macro journey.
- The final frame aligns closely enough with the live public scene that the
  transition feels spatial rather than navigational.
- Every live-looking fact comes from an approved public read model.
- The visitor can skip the film and enter directly.
- Reduced-motion visitors receive the complete identity and entry path.
- The public session cannot access operator/private surfaces or mint execution
  authority.
- The real RyeOS browser client owns the live product experience.
- Browser and terminal semantic parity remains intact.
- The experience's own art direction and media provenance are inspectable as
  RyeOS data.

## Non-goals

- A portfolio of unrelated cinematic worlds.
- A generic SaaS homepage.
- Replacing the RyeOS browser client with React.
- Reintroducing a hard-coded marketing `home` mode into the shared UI core.
- Hiding the real product behind a long mandatory scroll sequence.
- Claiming that signatures imply safety, correctness, or isolation.
- Treating app-local login as RyeOS execution authority.
- Public arbitrary code or directive execution on the first demo node.
- Shared hostile multi-tenant hosting before its required boundaries exist.
- Using fake terminal output or fabricated node telemetry.
- Making an external video-generation provider part of the RyeOS runtime
  authority model.

## Open decisions

1. Whether the initial public scene should be the namespace Atlas, a narrower
   node-identity scene, or a composition that moves from identity into Atlas.
2. Whether public entry is anonymous, invite-based, or available in both modes.
3. Whether the arrival assets initially live in the first-party UI composition
   or wait for a generic signed asset provider.
4. Which live thread fields are safe and meaningful to publish.
5. Whether the scheduled demonstration should visibly exercise recovery or
   remain a simpler signed execution loop.
6. Whether the final CTA navigates after a completed crossfade or lets the
   public surface become interactive in place before URL transition.
7. Whether a mobile-specific source encode is necessary or a center-safe
   master is sufficient.
8. Which exact node and publisher trust facts belong in the public identity
   reveal.

## Decision rule

The film is justified only if it strengthens entry into the real RyeOS
experience.

If implementation pressure turns it into a separate marketing application,
fake initialization, fake telemetry, or a second product-state model, stop and
return to the core boundary:

```text
first contact explains
the node proves
RyeOS UI operates
```
