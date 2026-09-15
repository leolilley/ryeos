```yaml
category: ryeos/future
name: environment-build-system
title: RyeOS Environment Build System
description: A RyeOS-native model for producing, verifying, publishing, and consuming portable execution environments from exact admitted content.
entry_type: design
version: "0.1.0"
status: discussion
```

# RyeOS environment build system

Discussion recorded: 2026-09-07. This is a future design note, not an
implementation authorization or a claim that the complete system described
here has landed.

## Purpose

RyeOS needs to create portable environments for workers, Tools, directives,
local inference, and eventually broader execution workloads. An environment
may contain an interpreter, compiler, shell, command-line utilities, libraries,
models, or other runtime resources. Its consumers should receive exact admitted
bytes without depending on whatever software happens to be installed on the
execution host.

The first demanding case is RyeOS producing the environment used to develop
RyeOS itself. That case includes a native compiler, linker, libc, shell,
utilities, corresponding sources, license material, and offline dependencies.
It is a bootstrap-heavy edge case, not the intended setup experience for every
project.

The desired result is a RyeOS-native environment build system composed from
existing execution and content authorities. It is not a new universal package
language and must not become a second execution substrate.

## The central separation

Environment production and environment use are different operations:

```text
publisher or maintainer path
  exact sources and bootstrap inputs
    -> admitted preparation
    -> admitted build or assembly
    -> independent verification
    -> retained artifact
    -> explicit publication and consumer binding

project or worker path
  select an already published environment
    -> materialize its exact content
    -> compose its executable search and environment contribution
    -> execute within the consumer's existing authority
```

The first path may be technically difficult. The second should be small and
declarative. A project consuming a Python or authoring environment must not
copy the producer, understand its archive layout, or reconstruct its native
dependency closure.

## Existing RyeOS foundation

The build system should compose existing mechanisms:

- signed Tools as bounded operations;
- Graphs as durable orchestration;
- signed configuration as exact recipe input;
- managed and explicitly pinned external content;
- runtime content dependencies and environment contributions;
- isolated or otherwise admitted execution authority;
- retained private workspace results;
- content manifests and exact realization identity;
- effect records, receipts, and execution lineage;
- explicit import, publication, and consumer binding; and
- independent trust policy at the consuming node.

No new kind is justified merely to give these mechanisms a collective name.
If a missing artifact concept is eventually proved, it should be introduced
only after the existing retained-result and content-binding model is shown to
be insufficient.

Related boundaries are described in:

- [Large-content realization follow-ons](large-content-realization-follow-ons.md)
- [Execution identity](execution-identity.md)
- [RyeOS-native development platform](ryeos-native-development-platform.md)
- [Reflexive deployment](reflexive-deployment.md)
- [Determinism classes](determinism-classes.md)

## The RyeOS-native build model

The system is a graph of typed artifact-producing operations:

```text
managed source content
  -> prepare Tool
  -> build or assemble Tool
  -> closure-verification Tool
  -> independent reproduction or comparison
  -> retained content tree
  -> authorized publication
  -> exact consumer binding
```

Each Tool declares its own:

- runtime and executable;
- content inputs and their exact identities;
- filesystem authority;
- network authority;
- effect class;
- parameter schema and bounds;
- output location and retention contract; and
- time and resource ceilings supported by the runtime.

The Graph supplies ordering, branching, failure behavior, and durable lineage.
The content system supplies byte identity. Publication supplies the authority
transition from a private result to a reusable input. None of these roles
should be reimplemented by a recipe language or producer script.

## Implementation languages are replaceable

Python, Rust, Zig, shell, or another admitted runtime may implement a Tool.
The implementation language is not the authority model and is not the build
system's public contract.

The current environment producer uses a pinned standalone Python because its
standard library provides inspectable archive, JSON, hashing, and filesystem
operations without requiring the native Rust toolchain that the producer is
helping construct. Its source, interpreter, inputs, filesystem view, and output
are still admitted by RyeOS. Python grants no authority of its own.

This is a practical way to break the first bootstrap cycle:

```text
need a controlled native toolchain
  -> need an admitted producer for that toolchain
  -> cannot require the unfinished toolchain to compile its own producer
  -> use an independently admitted bootstrap runtime
```

A prebuilt native producer could also break the cycle, but that binary would
itself require exact source, publication, verification, and portability
evidence. Replacing Python is useful only if it simplifies or strengthens the
whole closure rather than moving the bootstrap obligation elsewhere.

## Reusable operations, not a universal evaluator

The first complete producers will reveal operations that recur across
environment families. Likely reusable operations include:

- bounded archive inspection and member selection;
- safe source-tree materialization;
- canonical tree inventory and manifest calculation;
- exact file selection from an authored contract;
- binary interpreter and shared-library inspection;
- native runtime relocation and closure verification;
- reproducibility comparison;
- retained-result import; and
- artifact publication and consumer binding.

Ecosystem-specific operations remain separate:

- Cargo dependency preparation and native Rust builds;
- Python runtime or wheel-environment assembly;
- tinygrad runtime, model, and kernel preparation;
- JavaScript dependency preparation;
- native C or Zig builds; and
- any later device-specific compilation.

Repeated security-sensitive mechanics should converge on shared RyeOS-owned
Tools or libraries. Artifact-specific selections should remain signed
configuration. A second implementation should not be generalized merely
because its filenames resemble the first.

The system must not grow into:

- a privileged expression evaluator;
- a second scheduler;
- a second content-addressed store;
- a host package-manager wrapper;
- arbitrary ambient shell execution presented as a Tool; or
- one giant producer that knows every ecosystem.

## Recipe shape

The eventual author-facing recipe should describe intent and composition, not
implement archive parsing or authority transitions. Conceptually:

```yaml
inputs:
  - exact source set
  - exact bootstrap runtime
  - exact native libraries

steps:
  - prepare source closure
  - build required programs
  - assemble environment tree
  - verify runtime closure
  - reproduce and compare

output:
  form: retained_tree
  name: development-environment
```

Each step resolves to an independently signed Tool. This notation is
illustrative; it does not propose another schema before the current Graph and
Tool schemas have been tested for the same composition.

## Output contract

An environment result should carry enough exact evidence to determine:

- the admitted source and bootstrap input identities;
- the Tool and Graph definitions that produced it;
- the execution lineage and runtime identity;
- the portable file manifest and executable modes;
- native interpreter and library closure where applicable;
- corresponding source and notice inventory where redistribution requires it;
- independent reproduction or verification outcome;
- the private result snapshot from which it was imported;
- the publication authority and published content identity; and
- every consumer binding authorized from that publication.

A content hash proves byte identity, not semantic fitness, license compliance,
portability, or trust. Those remain separate claims with separate evidence and
local policy.

## Reproducibility and determinism

The strongest useful claim is not that every build is inherently deterministic.
The system should state what was actually proved:

- **assembled:** exact inputs produced a retained output once;
- **independently reproduced:** a separate admitted operation produced the same
  portable manifest;
- **recorded:** an effectful producer result can be reused through its exact
  recorded-effect identity;
- **sealed:** only a separately qualified scope may claim re-derivable output
  under the declared runtime and device contract; or
- **verified only:** an existing artifact passed finite structural and runtime
  checks without a reproducible-build claim.

These claims should use the existing effect and execution-identity vocabulary.
Environment production does not need its own weaker synonyms.

## Bootstrap boundary

Some inputs must initially enter RyeOS through an operator or publisher path.
That path should be explicit and narrow:

1. acquire an upstream artifact under an exact expected identity;
2. retain its origin and corresponding-source information;
3. import it through existing content authority;
4. bind it to the specific producer; and
5. perform all subsequent transformation through admitted execution.

Bootstrap does not justify permanent dependence on host PATH, inherited proxy
configuration, mutable package registries, or unverified archives. Conversely,
moving a host download script under `.ai/tools` does not make its dependencies
portable or its behavior RyeOS-native.

The boundary should shrink as managed acquisition and reusable producers are
qualified, but RyeOS should state honestly when an external publisher action
still exists.

## Consumer experience

The environment consumer should need only an exact environment selection plus
project-specific authority:

```text
environment: exact published development environment
project: exact source generation or retained candidate
operations: explicitly admitted project Tools
credentials: only those required by the workload
```

The consumer does not receive producer authority. It cannot republish the
environment merely because it can execute its commands. Likewise, placing a
compiler or shell in an environment does not grant unrestricted process,
network, node-control, signing, or project-publication authority.

Workers may use ordinary admitted CLI programs for interactive development.
Consequential or dependency-heavy project operations can remain separate
Tools invoked through the restricted workload client. This preserves a normal
working experience without smuggling ambient host authority into the worker.

## First hard case and subsequent proving cases

The native RyeOS authoring environment is an appropriate first hard case
because it exercises:

- bootstrap without assuming the finished compiler environment;
- mixed static and dynamically linked executables;
- exact shell and command-line utility behavior;
- native compiler and linker closure;
- offline dependency preparation;
- retained production and independent verification; and
- reuse by a hosted development worker.

It should not define the minimum ceremony for subsequent environments. Simpler
interpreted or pure-data environments should demonstrate that consumers can
reuse the same substrate with much smaller recipes and no copied production
framework.

The local-inference environment is a valuable second producer because it adds
large model/runtime content and device-sensitive execution. Common mechanics
shared by these two independently complete cases are stronger candidates for
extraction than abstractions inferred from the first case alone.

## Ownership and source layout

Production logic belongs with the Tool or shared library that owns the
operation. Fixed input contracts belong in signed configuration. Composition
belongs in Graphs. Qualification fixtures and disposable-node harnesses belong
under tests. Only genuine installation, publisher bootstrap, or external CI
entrypoints should remain outside the RyeOS operation surface.

Folder placement does not grant worker access. A Tool may be operator-only,
publisher-only, or available solely to a narrowly configured worker. Source
ownership and runtime authorization are separate decisions.

The native authoring producer should be named and documented as environment
or platform **publication**, not as something run for every authoring session.
Its proximity to everyday check, build, and format Tools must not imply that
consumers reproduce the platform before using it.

## Delivery stages

### Stage 1 — complete one honest producer

- Finish the native authoring environment with exact admitted inputs.
- Prove assembly, independent verification, retained import, publication, and
  consumer binding.
- Qualify actual command execution without ambient host binaries or libraries.
- Keep bootstrap exceptions explicit.

### Stage 2 — prove simple consumption

- Bind the published environment to more than one retained project worker.
- Demonstrate that consumers use a small environment declaration.
- Prove restart and placement continuity without rebuilding the environment.
- Confirm that environment access does not confer producer or publication
  authority.

### Stage 3 — extract demonstrated common operations

- Compare the native authoring and local-inference producers.
- Identify duplicated archive, manifest, closure, and comparison mechanics.
- Move only genuinely shared mechanics into reusable Tools or libraries.
- Replace copied recipe logic with signed configuration where possible.

### Stage 4 — make publication routine

- Provide a clear operator surface for production, verification, retained
  import, publication, and binding.
- Preserve complete provenance across those authority transitions.
- Expose environment identity and qualification status in the UI.
- Support offline export and admission on another trusted node.

## Acceptance criteria

The design is successful when:

- a complex environment can be produced without ambient undeclared tools;
- an independent admitted operation can verify or reproduce it;
- production output enters the content system without reopening mutable
  workspace state;
- another node can admit the exact published environment under local policy;
- multiple projects can consume it without copying its producer;
- workers receive familiar commands but only their declared authority;
- changing the environment changes execution identity visibly;
- ordinary project activation does not invoke the producer;
- generic mechanics have one owner rather than copied Python or shell
  implementations; and
- no universal package evaluator or parallel content store has been added.

## Questions to resolve from implementation evidence

1. Which archive and portable-manifest operations are already sufficiently
   shared to warrant a core Tool or library?
2. Does the existing Graph schema express production and independent
   verification cleanly without a recipe schema?
3. Which reproducibility facts belong in retained result receipts versus a
   separately signed publication record?
4. How should environment compatibility name architecture, ABI, loader,
   device, and numerics constraints without conflating them with content hash?
5. Which publisher bootstrap acquisitions can become managed operations, and
   which must remain external by construction?
6. What is the smallest useful environment a normal interpreted project needs,
   and does its activation prove that the hard first case did not leak into the
   consumer experience?
7. When does a repeated producer implementation justify a native reusable Tool
   rather than another signed implementation in the most suitable language?

These questions should be answered by completed producer and consumer evidence,
not by expanding the substrate in anticipation of every future ecosystem.
