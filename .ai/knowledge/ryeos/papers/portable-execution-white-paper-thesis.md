<!-- ryeos:signed:2026-07-23T10:04:49Z:0bbb70aa83ca746406c0a1ab9ca1efddc1c5a4aeda75b1c1652f3a097e327a99:yPUKhkg0qqNvr/a8NS3qB2Qm50fGc1QlP/R847OLjpxjU6cDUdPfoyttVdO7qOea/YqMtM5eByrBREQD9JzmAQ==:64f806fe8f81efdecf5245e1b1941aeecfe3a56ff1826adc1214538ab69953ca -->
---
category: ryeos/papers
tags: [white-paper, portable-execution, cryptographic-identity, architecture]
version: "0.3.0"
description: >
  Working notes for the RyeOS white paper thesis: portable verified execution
  through cryptographic identity. This is reference material, not a draft.
---

# RyeOS white paper notes: portable verified execution

These notes capture the core thesis and surrounding ideas from the white paper
discussion. This is intentionally not a polished white paper draft. It is a
reference document for later analysis, positioning, and writing.

This document is part of the papers series in this directory. The series map
(`series-map.md`) holds the shared derivation and vocabulary; the contract
between this thesis and the four downstream papers is recorded in
`white-paper-relation.md`. This document keeps its own scope: portable
verified execution as a general systems thesis, with agents as one
application.

## Core thesis

Short version:

> RyeOS is portable verified execution: cryptographic identity gives execution
> both properties in one act.

Portable and verified are one fused property, not a goal and a mechanism. A
signed, content-addressed object is verified (it proves what it is and who
stands behind it) and portable (a hash means the same thing on every machine,
so trust travels with the bits) by the same act. Execution inherits both
because execution is represented as data end to end.

Expanded version:

> RyeOS is a signed, content-addressed substrate for portable verified
> execution. It gives cryptographic identity to executable capability and
> runtime history, allowing tools, commands, runtimes, workflows, services,
> authority, events, snapshots, and refs to be inspected, verified, moved,
> resumed, replayed, and owned independently of any single process, machine,
> session, or vendor runtime.

Possible title:

> RyeOS: Portable Verified Execution

Possible subtitle:

> A signed, content-addressed substrate for executable artifacts, authority,
> runtime state, and replayable histories.

## Important refinement

Do not say merely:

> RyeOS makes computation into data.

That is the mechanism, not the headline. Everything-is-data is how
cryptographic identity reaches execution at all; the property to lead with is
portable verified execution.

Do not center the paper on AI agents. Agents are an important application and
stress test, but they are not the thesis.

The general systems conclusion is:

> Existing systems made source code, packages, containers, and data portable.
> RyeOS targets execution itself: executable capability plus operational history.

## The necessity argument

The fused property should be derived, not asserted. Derivation:

1. Represent an execution as data and it can leave the machine that ran it —
   across a restart, to another node, into the future.
2. Once an execution can outlive and leave its machine, the machine can no
   longer serve as the authority for what the execution is or who authorized
   it. Location-based trust does not survive portability.
3. Therefore the data must vouch for itself: content addressing for identity,
   signatures for authorship and authority. Self-certification is not a
   security feature layered onto portable execution; it is the minimal
   machinery portability forces.
4. Once identity, authority, and history travel inside the data, the executor
   becomes fungible: any node with the right trust can be the site of an
   execution.

Compressed: portability forces proof. Portable and verified are one property
because the first is impossible without the second. An asserted fusion is a
design choice; a forced fusion is a theorem, and the paper should present it
as the latter.

## Historical lineage

The idea began with portable context in Context Kiwi:

1. Useful context/instructions/directives should be reusable across projects.
2. Portable context needs stable identity.
3. Hashes give exact content identity.
4. Signatures give provenance/trust across boundaries.
5. The key leap was applying the same hash/signature idea to tools.
6. Signed tools are portable executable capability, not just portable context.
7. Portable executable capability forces the need for a real execution substrate.
8. RyeOS is that substrate.

Condensed lineage:

```text
portable context
  -> hash/version/sign
  -> verifiable portability
  -> apply same idea to tools
  -> portable capability
  -> portable execution
  -> RyeOS node, bundles, CAS, refs, runtimes, commands, threads
```

Useful phrase:

> Once context has cryptographic identity, it can travel. Once tools have
> cryptographic identity, execution can travel.

## What "execution" means here

Avoid implying that RyeOS hashes a single magical thing called "execution."
Execution is represented as an object graph whose components and history have
cryptographic identity.

The graph can include:

- executable artifacts;
- tools;
- commands;
- command surfaces and registries;
- services;
- runtimes;
- workflows/graphs;
- authority/capability grants;
- inputs and outputs;
- events;
- snapshots;
- refs;
- bundles;
- CAS objects;
- typed schemas/kinds;
- provenance and trust records.

Precise formulation:

> RyeOS represents the components and history of execution as a signed,
> content-addressed object graph.

## What cryptographic identity provides

Cryptographic identity provides:

- integrity;
- stable addressing;
- provenance;
- verifiability;
- portable executable definitions;
- inspectable history;
- replay/resume substrate;
- inputs to local trust and policy decisions.

It does not automatically provide:

- safety;
- isolationing;
- determinism;
- semantic correctness;
- authorization;
- revocation;
- universal runtime compatibility.

Those are layered concerns. The paper should separate identity from trust policy,
authorization, isolationing, and deterministic replay.

## Adjacent systems and distinction

RyeOS is not novel because it uses hashes, signatures, registries, bundles, or
runtimes. The novelty is the layer where those primitives are applied.

There is a sharper cut than the layer distinction. The verified-execution
lineage — Nix, build systems, deterministic replay, blockchains — verifies by
recomputation: re-run and compare against a specification. That model
requires an external correctness predicate for the executor. Executors that
have no specification independent of themselves (human operators, LLM
runtimes — see the actor model below) cannot be verified this way even in
principle; they can only be attributed and held to account. RyeOS sits on the
other branch: verification by signed testimony over a durable record. The
adjacent systems below are therefore not merely at a different layer; most
are on the other branch of verification theory, which is why none of them
could host this class of executor by extension. The full argument is the
second paper in this series (`testimony-not-determinism.md`).

Useful positioning:

> Git gave content identity to source history. Nix gave stronger identity to
> build inputs and outputs. Docker gave portable process environments.
> Capability systems modeled authority. RyeOS combines these lines at a
> different layer: it gives cryptographic identity to executable capability and
> runtime history, so execution can move across machines, sessions, projects,
> and vendors while remaining inspectable and verifiable.

Comparison notes:

- Git makes source files and source history portable through content-addressed
  blobs, trees, and commits. It does not model executable capability, runtime
  authority, service state, or execution histories as first-class portable
  objects.
- Nix makes build recipes, dependencies, and store paths reproducible and
  addressable. It is not primarily a substrate for signed tools, commands,
  workflows, services, event histories, authority grants, and resumable
  execution.
- Docker/OCI packages filesystem images and process environments. Runtime
  execution, authority, provenance, and histories remain mostly external or
  imperative.
- Package managers distribute installable code units using names, versions,
  registry metadata, and sometimes checksums/signatures. They are generally
  registry/name-centric rather than a portable execution graph with typed
  authority, runtime state, replay, and provenance.
- Capability systems model authority as unforgeable references/tokens. They do
  not usually combine this with signed content-addressed executable artifacts
  and portable runtime histories.
- IPFS/CAS systems address content well, but do not define execution semantics,
  command registries, runtime state, capability grants, or replayable execution
  histories.
- Sigstore/SBOM/provenance tools sign and attest artifacts around execution.
  They generally do not make execution itself a portable object graph.
- Kubernetes and other orchestrators model desired service/process state, but
  execution remains cluster/runtime-bound and is not inherently signed,
  content-addressed, replayable, or owned as a portable object.

Short differentiator:

> RyeOS is portable executable capability plus portable execution history.

## White paper structure idea

### 1. Abstract

State the thesis directly: execution remains trapped even though source,
packages, containers, and data have portability models. RyeOS gives execution
cryptographic identity by representing executable artifacts, authority, runtime
state, and history as signed, content-addressed objects.

### 2. Problem: execution is still trapped

Execution is usually bound to:

- local processes;
- machines;
- shell sessions;
- CI runners;
- SaaS workflow platforms;
- application/plugin hosts;
- vendor runtimes;
- ambient filesystem paths and environment variables.

Existing systems often fail to answer:

- what exact capability ran?
- who authored or approved it?
- what runtime evaluated it?
- what authority did it use?
- what inputs and outputs were involved?
- what state changed?
- can this be moved, resumed, replayed, forked, or audited elsewhere?

### 3. Prior systems and what they solved

Discuss Git, package managers, Nix, Docker/OCI, CAS/IPFS, capability systems,
and provenance/signature systems. The transition should be:

> RyeOS adopts proven primitives, but applies them to executable capability and
> runtime history.

### 4. Core model: cryptographic identity for execution

Define the model:

- hash/content address: exact immutable content identity;
- signature: provenance/approval/trust input;
- typed kind schema: inspectable and interoperable object meaning;
- ref: mutable continuity over immutable objects;
- bundle: transport unit for related executable objects;
- thread/event/snapshot: durable execution history and resumable state.

The actor primitive is the signing key. An operator, a node, an agent: each is
a key, and anything that acts, acts by signing. Trust is always a decision
about a key, never about a machine. This is not agent framing — it is what
remains when every component of execution is data: the only thing that can act
on data with checkable consequence is a key.

A consequence worth stating: the actor class always contained authored-output
executors — operators. A human running a command has no external
specification of correct behavior; their acts can only be attributed, never
recomputed. The substrate's juridical shape (keys, signatures, grants,
records) was therefore required before any model entered the picture, and
model executors slot in without a single new primitive because they are the
second member of a class humans founded.

### 5. RyeOS object model

Explain objects such as signed items, kind schemas, tools, commands, services,
runtimes, workflows/graphs, bundles, refs, CAS objects, threads, events,
snapshots, registries, and trust/provenance records.

Key sentence:

> A RyeOS execution is not one object. It is a graph: what was invoked, by whom,
> under what authority, using which runtime, with which inputs, producing which
> outputs, emitting which events, and resulting in which snapshots.

Concretely, every thread event carries two hash links — the previous event in
the chain and the previous event in the thread — so the execution trace is a
woven structure in CAS, not a flat log a client braids after the fact.

### 6. Execution lifecycle

Walk through:

1. author a tool/command/workflow;
2. assign typed schema/kind;
3. store/hash content;
4. sign or attest it;
5. publish in a registry or bundle;
6. resolve by content identity or ref;
7. verify trust policy;
8. execute through a runtime;
9. record events;
10. snapshot state;
11. resume, replay, fork, or sync elsewhere.

### 7. Trust, authority, and provenance

Clarify that immutable content identity does not itself authorize execution.
Identity is global-ish; execution depends on local/organizational trust policy.

This split is implemented as the attestation object: a signed claim about a
CAS object (issuer key, subject hash, claim, policy, evidence). An attestation
proves only that a key made a claim under a policy; local policy decides
whether a verified attestation is authoritative. Verification is objective;
trust is a local decision applied to the same evidence everywhere — which is
what lets the evidence move between parties at all.

Discuss integrity, signer identity, provenance, capability grants, inspection
before execution, auditability, delegation, and revocation limits.

### 8. Replay, resume, and ownership

The strong claim, grounded in implementation:

> The record is the execution. A running process is only a rebuildable
> projection of the execution object (process : execution :: SQLite
> projection : CAS). Completed steps are consumed as data and never
> re-crossed; nondeterminism exists only at the frontier — the
> not-yet-executed edge where the run touches the world.

Consequences:

- Resume after process death, replay of history, continuation across a
  segment cut, and (future) cross-node resume are all the same operation:
  reading the execution back in from its own record.
- "Record-complete" and "reproducible" are one property, not a trade-off.
  Once execution is data, re-instantiating it is reading the data back; the
  dichotomy only exists if execution means process.
- Deterministic re-execution of the world remains out of scope (see
  limitations); replay never meant re-running the world.

Implemented today: checkpoint plus event-log resume, project snapshot pinned
at spawn, reconciliation re-spawning resumable work under the same thread
identity after a daemon restart, and completed child results spliced into a
successor's resume state instead of re-dispatched.

### 9. Applications

Keep AI agents out of the thesis and put them here as one application.

Potential application areas:

- portable developer tools;
- reusable command/workflow bundles;
- cross-project operational playbooks;
- verifiable automation;
- runtime migration;
- audit and provenance;
- resumable long-running workflows;
- remote execution;
- AI agents as clients that benefit from portable tools/context/authority.

Agent framing:

> AI agents benefit because they are heavy users of tools, context, authority,
> and long-running execution histories. But RyeOS is not an agent OS; agents are
> one class of clients.

### 10. Limitations and future work

Acknowledge:

- deterministic replay is runtime-dependent;
- signatures do not imply safety;
- isolationing is separate;
- trust policy is hard;
- revocation is hard for immutable objects;
- portable authority must avoid leaking secrets;
- cross-runtime compatibility has limits;
- content identity does not solve semantic equivalence.

Possible future work:

- stronger isolationing;
- richer policy engines;
- transparency logs;
- deterministic runtime profiles;
- formal schema validation;
- broader runtime adapters;
- organizational governance models.

## Suggested demonstrations/evaluation criteria

A rigorous paper could demonstrate (most of these exist today and should be
presented as demonstrations, not aspirations):

- moving a signed tool bundle from one project/machine to another and verifying
  identity (implemented: bundle publish/install/verify, remote sync);
- executing the same command through a resolved runtime (implemented);
- inspecting provenance and authority attached to a tool (implemented);
- resuming a workflow from a snapshot (implemented: checkpoint + event-log
  resume, snapshot pinned at spawn, reconciliation after daemon restart);
- replaying or auditing an execution thread (implemented: events replay,
  thread tail/chain/children);
- continuing an execution history from a prior state (implemented for
  continuation chains across segment cuts and lineage-linked child threads;
  arbitrary-point forking remains future work);
- replacing a runtime or client while preserving executable identity/history
  where compatible.

Evaluation dimensions:

- portability;
- inspectability;
- provenance clarity;
- replay/resume fidelity;
- trust-policy enforcement;
- operational overhead;
- developer ergonomics.

## Lillux and why the microkernel exists

The white paper should also explain why RyeOS has Lillux beneath it. Lillux is
not an incidental implementation detail. It is the small native substrate that
keeps portable execution grounded in a minimal set of stable OS-level
primitives.

Current Lillux framing:

> Microkernel for RyeOS — Execute, Memory, Identity, Time.

Lillux provides four primitives:

- Execute: process lifecycle, spawning, killing, status, timeouts;
- Memory: content-addressed storage;
- Identity: signing, verification, keypairs, sealed secret envelopes;
- Time: timestamps and sleep.

This belongs in the white paper because portable execution cannot be only a
high-level registry or daemon protocol. Eventually it bottoms out in host facts:

- a process was spawned;
- bytes were hashed and stored;
- a signature was created or verified;
- a secret was opened for a scoped execution;
- a timestamp was recorded;
- a child process was killed or timed out.

Lillux is the boundary where RyeOS stops being a symbolic execution substrate
and touches the host OS. Keeping that boundary small matters.

Possible thesis:

> Lillux is the execution microkernel for RyeOS: the minimal native layer that
> turns portable execution objects into real host effects while preserving the
> cryptographic and content-addressed invariants above them.

Another formulation:

> RyeOS needs a microkernel because verifiable portable execution cannot depend
> on ambient host behavior scattered across shell scripts, Python packages, and
> runtime conventions. The primitive operations — execute, store, sign, seal,
> and timestamp — need a narrow, auditable, native implementation.

### Why a compiled/native microkernel

The historical move from Python to Rust should be captured carefully. The reason
is not just performance. Portable execution needs a systems substrate with:

- stable process lifecycle behavior;
- cross-platform process controls;
- deterministic content hashing/canonicalization;
- atomic materialization of CAS objects and executables;
- controlled environment passing;
- cryptographic key handling;
- sealed secret envelopes below the higher-level runtime;
- fewer ambient dependency and interpreter hazards;
- a small surface that can be audited independently of higher-level RyeOS logic.

The paper should avoid making Lillux sound like a full kernel. It is a
microkernel in the architectural sense: a deliberately small primitive layer
under a larger signed/content-addressed execution system.

### Layering

Useful stack:

```text
RyeOS forms/registries/runtimes/threads
  -> execution planning and policy
  -> Lillux primitives: Execute, Memory, Identity, Time
  -> host OS processes, files, clocks, keys
```

Or:

```text
portable execution object graph
  -> RyeOS node evaluates and records
  -> Lillux performs primitive host effects
  -> OS provides actual process/filesystem/time
```

### Relation to the core thesis

Lillux reinforces the main thesis rather than replacing it:

- cryptographic identity needs a signing/verification primitive;
- content-addressed portability needs a CAS primitive;
- executable capability needs a process primitive;
- replayable/resumable history needs time and lifecycle facts;
- portable authority needs sealed secret material below the high-level runtime.

So Lillux is the minimal host-facing substrate required by portable execution
through cryptographic identity.

## Capability and consequence

This may be the strongest conceptual refinement after the basic portable
execution thesis.

Money line:

> A signed tool without history is a package. A history without signed capability
> is a log. RyeOS connects capability and consequence into a portable execution
> graph.

Another compact phrasing:

> RyeOS makes capability and consequence portable together.

Or:

> RyeOS makes execution portable by giving cryptographic identity to both what
> can be done and what was done.

Most systems separate these two sides:

```text
what can be done  = code, tools, APIs, binaries, scripts, permissions
what was done     = logs, audit trails, shell history, metrics, database rows
```

RyeOS tries to connect both sides in one signed/content-addressed object model:

```text
capability object -> this can be invoked
execution event   -> this invocation happened
state object      -> this changed as a result
```

The resulting loop:

```text
portable capability
  -> invocation under authority/runtime/input
  -> durable consequence
  -> new context/state/capability
```

This is what creates the feeling of a self-describing execution world. The
system can carry both the ability to act and the record of action in the same
addressable substrate.

### Capability without history

A signed tool registry can answer:

```text
what can I run?
who signed it?
what is its hash?
```

But without durable execution history, it cannot fully answer:

```text
who ran it?
under what authority?
with what inputs?
through what runtime?
against what state?
what happened?
what changed?
can it be resumed, replayed, audited, or moved?
```

Portable capability without portable history is mobile power, but it is not yet
portable execution.

### History without capability

Logs and audit trails often say only that something named `deploy`, `build`, or
`sync` ran. They usually do not preserve:

- exact tool bytes/hash;
- signer/provenance;
- runtime descriptor;
- input contract;
- authority/capability context;
- object closure;
- resulting state refs.

History without signed capability is weak evidence. It may help forensics, but
it is not strongly computational. RyeOS history should point back to exact
capability objects, making it inspectable, resumable, replayable where possible,
and portable across contexts.

### Capability provenance -> invocation provenance -> state provenance

RyeOS can represent a stronger provenance chain:

```text
capability provenance
  where did this ability come from?
  who authored/approved it?
  what type/kind/runtime does it have?

invocation provenance
  who invoked it?
  with what authority and input?
  through what runtime?

state provenance
  what event/result/snapshot/ref followed?
  what world or project state changed?
```

This is more operational than ordinary artifact provenance. It covers not only
where a binary or file came from, but how an ability was exercised and what
consequence followed.

### Identity bridges capability and consequence

The same cryptographic identity should connect the two sides:

```text
capability:
  tool hash H
  signed by K

history:
  event says H was invoked
  under authority A
  with input I
  producing output O / snapshot S / ref R
```

This prevents execution history from degrading into ambiguous strings. A thread
or event can point to an exact signed object rather than merely to a command
name.

### Why refs matter here

CAS gives immutable facts:

```text
tool hash
event hash
snapshot hash
manifest hash
```

Refs give continuity:

```text
project head
thread head
bundle registration
accepted world/head
```

Without refs, the system has a pile of immutable objects. Without CAS, refs are
mutable pointers with weak history. Together they let RyeOS answer:

```text
what can be done now?
how did that become true?
what was done before?
what world/state did it produce?
```

### Lillux as the capability-to-consequence boundary

This also gives a sharper way to explain Lillux.

Lillux is where capability becomes consequence:

```text
signed capability object
  -> primitive host action
  -> process result / CAS write / signature / timestamp
```

Lillux provides the primitive verbs needed to turn capability into consequence:

- Execute -> process consequence;
- Memory -> content consequence;
- Identity -> signature/provenance consequence;
- Time -> temporal consequence.

Clean role split:

```text
RyeOS decides what execution means.
Lillux makes primitive execution happen.
CAS, refs, events, and snapshots remember what happened.
```

This phrasing may be stronger than describing Lillux only as a host-effects
microkernel.

### Phrases worth preserving

- Portable execution through cryptographic identity.
- Portable executable capability plus portable execution history.
- Capability and consequence, portable together.
- Execution as the bridge between what can be done and what was done.
- Lillux is the primitive layer where capability becomes consequence.
- A signed tool without history is a package; a history without signed
  capability is a log; RyeOS connects them into an execution graph.

## Guardrails for future writing

- Do not frame RyeOS primarily as an AI agent OS.
- Agent consequences (identity, standing, governance) belong to the paper
  series (`the-corporate-agent.md`), not this document; keep agents in the
  applications section here.
- Do not say cryptographic identity alone means safe execution.
- Do not overstate deterministic replay.
- Do not reduce RyeOS to a package manager, tool registry, or plugin system.
- Do not present "computation as data" / "everything is data" as the thesis
  itself: it is the mechanism — how cryptographic identity reaches execution
  at all — not the headline property.
- Always distinguish identity, trust, authorization, isolationing, and execution.
- Emphasize the layer distinction: source, packages, containers, and data are
  adjacent portability layers; RyeOS targets executable capability and runtime
  history.

## Best current formulation

> RyeOS is a signed, content-addressed substrate for portable verified
> execution. It gives cryptographic identity to executable capability and
> runtime history, allowing tools, commands, runtimes, workflows, services,
> authority, events, snapshots, and refs to be inspected, verified, moved,
> resumed, replayed, and owned independently of any single process, machine,
> session, or vendor runtime.

Short public version:

> RyeOS is portable verified execution.

Longer public version:

> Execution you can hand to another machine, another runtime, or your future
> self, without losing the proof of what it is and what it did.
